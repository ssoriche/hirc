//! One server's protocol state machine. No sockets: a line goes in, and
//! lines to send plus events for the UI come out. All registration, CAP
//! and routing logic lives here so it can be unit-tested; `conn` just
//! moves bytes.

use crate::caps::Caps;
use crate::event::{Command, ConnStatus, Event, PeerChange, Target};
use crate::isupport::Isupport;
use crate::multiline::{self, Limits};
use crate::proto::Message;
use crate::store::{Delivery, Line, LineKind};
use crate::{bridge, sasl, split};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Send(Message),
    Event(Event),
    /// Close and don't reconnect until the user asks (bad password).
    Stop(String),
    /// The user quit: close and don't reconnect until they ask.
    Quit,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SaslPlain {
    pub username: String,
    pub password: String,
}

/// A server's settings with every password already looked up.
#[derive(Clone, Default)]
pub struct SessionConfig {
    pub nickname: String,
    pub alt_nicks: Vec<String>,
    pub username: String,
    pub realname: String,
    pub password: Option<String>,
    pub sasl: Option<SaslPlain>,
    /// `"#chan"` or `"#chan key"`.
    pub channels: Vec<String>,
    /// Relay bots whose lines are unwrapped to their authors (`bridge`).
    pub bridges: Vec<String>,
}

impl std::fmt::Debug for SaslPlain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaslPlain").field("username", &self.username).field("password", &"<redacted>").finish()
    }
}

impl std::fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let password = self.password.as_ref().map(|_| "<redacted>");
        f.debug_struct("SessionConfig")
            .field("nickname", &self.nickname)
            .field("alt_nicks", &self.alt_nicks)
            .field("username", &self.username)
            .field("realname", &self.realname)
            .field("password", &password)
            .field("sasl", &self.sasl)
            .field("channels", &self.channels)
            .field("bridges", &self.bridges)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reg {
    /// Waiting for the last CAP LS line.
    CapLs,
    /// Waiting for this many ACK/NAK replies.
    CapReq(usize),
    Sasl,
    /// CAP END sent, waiting for 001.
    Registering,
    Done,
}

pub struct Session {
    network: String,
    cfg: SessionConfig,
    nick: String,
    alt: usize,
    reg: Reg,
    caps: Caps,
    isupport: Isupport,
    /// Lowercased channel -> (channel as the server spelled it, lowercased nicks), to route QUIT and NICK.
    members: HashMap<String, (String, HashSet<String>)>,
    seq: u64,
    /// SASL finished (903/907) on this connection.
    authed: bool,
    /// The server's `ERROR` text: why it is about to close the connection.
    closing: Option<String>,
    /// `draft/multiline` batches still arriving, by ref, oldest first.
    incoming: Vec<(String, Incoming)>,
    /// Numbers the batches we send, so each ref is new on the connection.
    batch_seq: u64,
    /// The multiline limits last told to the UI.
    multiline_said: Option<Limits>,
}

/// A `draft/multiline` batch being received: one message once it closes.
#[derive(Debug)]
struct Incoming {
    /// `BATCH +ref draft/multiline <target>`: the message's sender, and its
    /// time and msgid when the server tags them here.
    open: Message,
    lines: Vec<Message>,
}

fn send<I, S>(command: &str, params: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Output::Send(Message::new(command, params))
}

fn last(msg: &Message) -> &str {
    msg.params.last().map_or("", String::as_str)
}

impl Session {
    pub fn new(network: &str, cfg: SessionConfig) -> Session {
        Session {
            network: network.to_owned(),
            nick: cfg.nickname.clone(),
            cfg,
            alt: 0,
            reg: Reg::CapLs,
            caps: Caps::default(),
            isupport: Isupport::default(),
            members: HashMap::new(),
            seq: 0,
            authed: false,
            closing: None,
            incoming: Vec::new(),
            batch_seq: 0,
            multiline_said: None,
        }
    }

    /// Why the server said it is closing the connection (its `ERROR`),
    /// the reason to show once the socket closes.
    pub fn closing(&self) -> Option<&str> {
        self.closing.as_deref()
    }

    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    pub fn nick(&self) -> &str {
        &self.nick
    }

    /// The first lines of a fresh connection. PASS goes first: bouncers
    /// such as IRCCloud read it before anything else.
    pub fn start(&mut self) -> Vec<Output> {
        self.reg = Reg::CapLs;
        self.caps.reset();
        self.isupport = Isupport::default();
        self.members.clear();
        self.nick = self.cfg.nickname.clone();
        self.alt = 0;
        self.authed = false;
        self.incoming.clear();
        self.multiline_said = None;
        let mut out = Vec::new();
        if let Some(p) = &self.cfg.password {
            out.push(send("PASS", [p.as_str()]));
        }
        out.push(send("CAP", ["LS", "302"]));
        out.push(send("NICK", [self.nick.as_str()]));
        out.push(send("USER", [self.cfg.username.as_str(), "0", "*", self.cfg.realname.as_str()]));
        out
    }

    /// The connection is gone: a multiline batch that never closed is
    /// shown with the lines it got, rather than lost.
    pub fn end(&mut self, now: DateTime<Utc>) -> Vec<Output> {
        let mut out = Vec::new();
        for (_, batch) in std::mem::take(&mut self.incoming) {
            self.finish(batch, now, &mut out);
        }
        out
    }

    /// What one multiline batch may hold, while the server takes them.
    pub fn multiline(&self) -> Option<Limits> {
        let on = ["batch", "message-tags", multiline::CAP].iter().all(|c| self.caps.is_enabled(c));
        if on { self.caps.value(multiline::CAP).and_then(Limits::parse) } else { None }
    }

    pub fn handle(&mut self, msg: Message, now: DateTime<Utc>) -> Vec<Output> {
        let mut out = Vec::new();
        if matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE")
            && let Some(r) = msg.tag("batch")
            && let Some(i) = self.incoming.iter().position(|(k, _)| k == r)
        {
            self.incoming[i].1.lines.push(msg);
            return out;
        }
        match msg.command.as_str() {
            "PING" => out.push(send("PONG", msg.params.clone())),
            "CAP" => self.on_cap(&msg, &mut out),
            "AUTHENTICATE" => self.on_authenticate(&msg, &mut out),
            "903" | "907" if self.reg == Reg::Sasl => {
                self.authed = true;
                self.end_cap(&mut out);
            }
            "902" | "904" | "905" | "906" => out.push(Output::Stop(format!("SASL authentication failed: {}", last(&msg)))),
            "432" | "433" if self.reg != Reg::Done => match self.next_nick(msg.command == "433") {
                Some(n) => {
                    self.nick = n;
                    out.push(send("NICK", [self.nick.as_str()]));
                }
                None => out.push(Output::Stop("no usable nickname".into())),
            },
            "001" => self.on_welcome(&msg, now, &mut out),
            "005" => self.isupport.apply(&msg.params),
            "PRIVMSG" | "NOTICE" => self.on_message(&msg, now, &mut out),
            "JOIN" => self.on_join(&msg, now, &mut out),
            "PART" => self.on_part(&msg, now, &mut out),
            "KICK" => self.on_kick(&msg, now, &mut out),
            "QUIT" => self.on_quit(&msg, now, &mut out),
            "NICK" => self.on_nick(&msg, now, &mut out),
            "TOPIC" => self.on_topic(&msg, now, &mut out),
            "332" => {
                if let [_, chan, topic, ..] = msg.params.as_slice() {
                    out.push(Output::Event(Event::Topic { network: self.network.clone(), channel: chan.clone(), topic: Some(topic.clone()) }));
                }
            }
            "353" => self.on_names(&msg, &mut out),
            // away-notify: someone in a channel we share went away or came back.
            "AWAY" => {
                if let Some(nick) = msg.nick().filter(|n| !self.is_me(n)) {
                    let reason = msg.params.first().filter(|r| !r.is_empty()).cloned();
                    out.push(self.peer(nick, PeerChange::Away(reason)));
                }
            }
            // RPL_AWAY: the reply to messaging someone who is away.
            "301" => {
                if let [_, nick, reason, ..] = msg.params.as_slice() {
                    out.push(self.peer(nick, PeerChange::Away(Some(reason.clone()))));
                }
                let text = msg.params.get(1..).unwrap_or_default().join(" ");
                let line = self.line(&msg, now, LineKind::Server, None, text, false);
                out.push(self.event_line(Target::Server, line));
            }
            "MODE" => self.on_mode(&msg, now, &mut out),
            // No line of its own: the close follows, and its status line
            // gives this text as the reason.
            "ERROR" => self.closing = Some(last(&msg).to_owned()).filter(|t| !t.is_empty()),
            "BATCH" => self.on_batch(&msg, now, &mut out),
            "TAGMSG" | "366" => {}
            c if c.len() == 3 && c.bytes().all(|b| b.is_ascii_digit()) => {
                let text = msg.params.get(1..).unwrap_or_default().join(" ");
                let line = self.line(&msg, now, LineKind::Server, None, text, false);
                out.push(self.event_line(Target::Server, line));
            }
            _ => {}
        }
        out
    }

    /// Next nickname to try: alt_nicks in order, then (only when the nick
    /// is in use) up to 3 underscores. `None` when exhausted.
    fn next_nick(&mut self, in_use: bool) -> Option<String> {
        let alts = self.cfg.alt_nicks.len();
        let next = match self.cfg.alt_nicks.get(self.alt) {
            Some(n) => n.clone(),
            None if in_use && self.alt - alts < 3 => format!("{}_", self.nick),
            None => return None,
        };
        self.alt += 1;
        Some(next)
    }

    fn on_cap(&mut self, msg: &Message, out: &mut Vec<Output>) {
        let list = last(msg).to_owned();
        match msg.params.get(1).map(String::as_str) {
            Some("LS") => {
                let last_line = self.caps.on_ls(&msg.params);
                if last_line && self.reg == Reg::CapLs {
                    self.request_caps(out);
                }
            }
            Some("ACK") => {
                self.caps.on_ack(&list);
                self.got_reply(out);
            }
            Some("NAK") => {
                self.caps.on_nak(&list);
                self.got_reply(out);
            }
            Some("NEW") => {
                self.caps.on_new(&list);
                for req in self.caps.to_request() {
                    out.push(send("CAP", ["REQ".to_owned(), req]));
                }
            }
            Some("DEL") => self.caps.on_del(&list),
            _ => {}
        }
        let limits = self.multiline();
        if limits != self.multiline_said {
            self.multiline_said = limits;
            out.push(Output::Event(Event::Multiline { network: self.network.clone(), limits }));
        }
    }

    /// `BATCH +ref type ...` / `BATCH -ref`. Only `draft/multiline` batches
    /// are collected; the lines of any other type are handled as they come.
    fn on_batch(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let Some(r) = msg.params.first() else { return };
        let (open, r) = match (r.strip_prefix('+'), r.strip_prefix('-')) {
            (Some(r), _) => (true, r),
            (_, Some(r)) => (false, r),
            _ => return,
        };
        // A close, or a ref opened again while its batch never closed: show
        // what it got.
        if let Some(i) = self.incoming.iter().position(|(k, _)| k == r) {
            let (_, batch) = self.incoming.remove(i);
            self.finish(batch, now, out);
        }
        if open && msg.params.get(1).map(String::as_str) == Some(multiline::CAP) {
            self.incoming.push((r.to_owned(), Incoming { open: msg.clone(), lines: Vec::new() }));
        }
    }

    /// A multiline batch's lines as the one message they are: `\n`
    /// between lines, continuations joined, the batch's sender and target,
    /// and its time and msgid (else the first line's).
    fn finish(&mut self, batch: Incoming, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let Some(first) = batch.lines.first() else { return };
        let text = multiline::join(batch.lines.iter().map(|m| (m.params.get(1).map_or("", String::as_str), m.tag(multiline::CONCAT).is_some())));
        let tags = ["time", "msgid"]
            .into_iter()
            .filter_map(|k| batch.open.tag(k).or_else(|| first.tag(k)).filter(|v| !v.is_empty()).map(|v| (k.to_owned(), Some(v.to_owned()))))
            .collect();
        let target = batch.open.params.get(2).or(first.params.first()).cloned().unwrap_or_default();
        let source = batch.open.source.clone().or_else(|| first.source.clone());
        let msg = Message { tags, source, command: first.command.clone(), params: vec![target, text] };
        self.on_message(&msg, now, out);
    }

    fn request_caps(&mut self, out: &mut Vec<Output>) {
        let reqs = self.caps.to_request();
        if reqs.is_empty() {
            return self.after_caps(out);
        }
        self.reg = Reg::CapReq(reqs.len());
        for req in reqs {
            out.push(send("CAP", ["REQ".to_owned(), req]));
        }
    }

    fn got_reply(&mut self, out: &mut Vec<Output>) {
        if let Reg::CapReq(n) = self.reg {
            if n <= 1 {
                self.after_caps(out);
            } else {
                self.reg = Reg::CapReq(n - 1);
            }
        }
    }

    /// Caps are settled. SASL must finish before CAP END, or the server
    /// registers us unauthenticated.
    fn after_caps(&mut self, out: &mut Vec<Output>) {
        if self.cfg.sasl.is_none() {
            return self.end_cap(out);
        }
        let plain = self.caps.value("sasl").is_none_or(|v| v.split(',').any(|m| m.eq_ignore_ascii_case("PLAIN")));
        if self.caps.is_enabled("sasl") && plain {
            self.reg = Reg::Sasl;
            out.push(send("AUTHENTICATE", ["PLAIN"]));
        } else {
            out.push(Output::Stop("the server does not offer SASL PLAIN".into()));
        }
    }

    fn on_authenticate(&mut self, msg: &Message, out: &mut Vec<Output>) {
        if self.reg != Reg::Sasl || msg.params.first().map(String::as_str) != Some("+") {
            return;
        }
        let Some(creds) = &self.cfg.sasl else { return };
        for chunk in sasl::chunks(&sasl::plain("", &creds.username, &creds.password)) {
            out.push(send("AUTHENTICATE", [chunk]));
        }
    }

    fn end_cap(&mut self, out: &mut Vec<Output>) {
        if matches!(self.reg, Reg::CapLs | Reg::CapReq(_) | Reg::Sasl) {
            self.reg = Reg::Registering;
            out.push(send("CAP", ["END"]));
        }
    }

    fn on_welcome(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        // A server that ignores CAP registers us without SASL; don't go on
        // unauthenticated when the user asked for authentication.
        if self.cfg.sasl.is_some() && !self.authed {
            out.push(Output::Stop("server registered us before SASL finished".into()));
            return;
        }
        self.reg = Reg::Done;
        if let Some(n) = msg.params.first() {
            self.nick = n.clone();
        }
        out.push(Output::Event(Event::Status { network: self.network.clone(), status: ConnStatus::Connected { nick: self.nick.clone() } }));
        let line = self.line(msg, now, LineKind::Server, None, last(msg).to_owned(), false);
        out.push(self.event_line(Target::Server, line));
        for ch in &self.cfg.channels {
            let params: Vec<&str> = ch.splitn(2, ' ').collect();
            out.push(send("JOIN", params));
        }
    }

    /// A store line for `msg`: server time and msgid from its tags.
    fn line(&mut self, msg: &Message, now: DateTime<Utc>, kind: LineKind, nick: Option<&str>, text: String, own: bool) -> Line {
        self.seq += 1;
        let time = msg.tag("time").and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map_or(now, |t| t.with_timezone(&Utc));
        let msgid = msg.tag("msgid").filter(|m| !m.is_empty()).map(str::to_owned);
        Line { seq: self.seq, time, msgid, kind, nick: nick.map(str::to_owned), text, own, delivery: Delivery::Sent, boundary: false }
    }

    fn peer(&self, nick: &str, change: PeerChange) -> Output {
        Output::Event(Event::Peer { network: self.network.clone(), nick: nick.to_owned(), change })
    }

    /// `msg`'s sender as seen: their `user@host` when the source has one.
    fn seen(&self, msg: &Message, nick: &str) -> Output {
        let host = msg.source.as_deref().and_then(|s| s.split_once('!')).map(|(_, h)| h.to_owned()).filter(|h| !h.is_empty());
        self.peer(nick, PeerChange::Seen { host })
    }

    fn event_line(&self, target: Target, line: Line) -> Output {
        Output::Event(Event::Line { network: self.network.clone(), target, line })
    }

    fn is_me(&self, nick: &str) -> bool {
        nick.eq_ignore_ascii_case(&self.nick)
    }

    fn on_message(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let Some(to) = msg.params.first() else { return };
        let mut text = msg.params.get(1).map_or("", String::as_str);
        let from_user = msg.source.as_deref().is_some_and(|s| s.contains('!'));
        let from = msg.nick().unwrap_or("");
        let own = from_user && self.is_me(from);
        let mut kind = if msg.command == "NOTICE" { LineKind::Notice } else { LineKind::Privmsg };
        if let Some(ctcp) = text.strip_prefix('\x01') {
            let ctcp = ctcp.strip_suffix('\x01').unwrap_or(ctcp);
            match ctcp.strip_prefix("ACTION") {
                Some(rest) if msg.command == "PRIVMSG" => {
                    kind = LineKind::Action;
                    text = rest.strip_prefix(' ').unwrap_or(rest);
                }
                // Other CTCP (VERSION, PING, ...) isn't shown.
                _ => return,
            }
        }
        let relay = from_user && !own && kind == LineKind::Privmsg && bridge::is_bridge(&self.cfg.bridges, from);
        // A bridge's multiline message whose lines aren't all one person's:
        // each line stands alone, unwrapped (or not) by itself. Their msgids
        // stay distinct so the store keeps every line.
        if relay && text.contains('\n') && bridge::unwrap_lines(text).is_none() {
            let msgid = msg.tag("msgid").filter(|m| !m.is_empty()).map(str::to_owned);
            for (i, line) in text.split('\n').filter(|l| !l.is_empty()).enumerate() {
                let mut one = Message { params: vec![to.clone(), line.to_owned()], ..msg.clone() };
                if let Some(id) = msgid.as_deref().filter(|_| i > 0) {
                    one.tags.retain(|(k, _)| k != "msgid");
                    one.tags.push(("msgid".into(), Some(format!("{id}:{i}"))));
                }
                self.on_message(&one, now, out);
            }
            return;
        }
        let target = if !from_user {
            Target::Server
        } else if self.isupport.is_channel(to) {
            Target::Channel(to.clone())
        } else if own {
            Target::Query(to.clone())
        } else {
            Target::Query(from.to_owned())
        };
        if from_user && !own {
            out.push(self.seen(msg, from));
        }
        let mut nick = from_user.then(|| from.to_owned());
        let mut text = text.to_owned();
        // A relay bot's `<name> text` reads as `name`'s own line; the bot's
        // own words and actions stay its.
        if relay && let Some((relayed, name, rest)) = bridge::unwrap_lines(&text) {
            (kind, nick, text) = (relayed, Some(name), rest);
        }
        let line = self.line(msg, now, kind, nick.as_deref(), text, own);
        out.push(self.event_line(target, line));
    }

    fn members_of(&mut self, chan: &str) -> &mut HashSet<String> {
        &mut self.members.entry(chan.to_ascii_lowercase()).or_insert_with(|| (chan.to_owned(), HashSet::new())).1
    }

    fn on_join(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let (Some(nick), Some(chan)) = (msg.nick().map(str::to_owned), msg.params.first().cloned()) else { return };
        if self.is_me(&nick) {
            self.members.insert(chan.to_ascii_lowercase(), (chan.clone(), HashSet::new()));
            out.push(Output::Event(Event::Joined { network: self.network.clone(), channel: chan.clone() }));
        } else {
            out.push(self.seen(msg, &nick));
        }
        self.members_of(&chan).insert(nick.to_ascii_lowercase());
        let line = self.line(msg, now, LineKind::Join, Some(&nick), format!("{nick} joined"), self.is_me(&nick));
        out.push(self.event_line(Target::Channel(chan), line));
    }

    fn left(&mut self, chan: &str, nick: &str, out: &mut Vec<Output>) {
        if self.is_me(nick) {
            self.members.remove(&chan.to_ascii_lowercase());
            out.push(Output::Event(Event::Parted { network: self.network.clone(), channel: chan.to_owned() }));
        } else {
            self.members_of(chan).remove(&nick.to_ascii_lowercase());
        }
    }

    fn on_part(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let (Some(nick), Some(chan)) = (msg.nick().map(str::to_owned), msg.params.first().cloned()) else { return };
        let text = match msg.params.get(1).filter(|r| !r.is_empty()) {
            Some(r) => format!("{nick} left ({r})"),
            None => format!("{nick} left"),
        };
        let line = self.line(msg, now, LineKind::Part, Some(&nick), text, self.is_me(&nick));
        out.push(self.event_line(Target::Channel(chan.clone()), line));
        self.left(&chan, &nick, out);
    }

    fn on_kick(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let [chan, victim, rest @ ..] = msg.params.as_slice() else { return };
        let (chan, victim) = (chan.clone(), victim.clone());
        let by = msg.nick().unwrap_or("").to_owned();
        let text = match rest.first().filter(|r| !r.is_empty()) {
            Some(r) => format!("{victim} was kicked by {by} ({r})"),
            None => format!("{victim} was kicked by {by}"),
        };
        // Worded from the one who leaves, as a part or a quit is, so the
        // `<--` always points at the first nick.
        let line = self.line(msg, now, LineKind::Kick, Some(&victim), text, self.is_me(&victim));
        out.push(self.event_line(Target::Channel(chan.clone()), line));
        self.left(&chan, &victim, out);
    }

    /// Channels `nick` is in, spelled as the server first gave them.
    fn channels_with(&self, nick: &str) -> Vec<String> {
        let key = nick.to_ascii_lowercase();
        let mut chans: Vec<String> = self.members.values().filter(|(_, m)| m.contains(&key)).map(|(c, _)| c.clone()).collect();
        chans.sort();
        chans
    }

    fn on_quit(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let Some(nick) = msg.nick().map(str::to_owned) else { return };
        let reason = msg.params.first().cloned().unwrap_or_default();
        let text = if reason.is_empty() { format!("{nick} quit") } else { format!("{nick} quit ({reason})") };
        if !self.is_me(&nick) {
            out.push(self.peer(&nick, PeerChange::Quit(reason)));
        }
        for chan in self.channels_with(&nick) {
            self.members_of(&chan).remove(&nick.to_ascii_lowercase());
            let line = self.line(msg, now, LineKind::Quit, Some(&nick), text.clone(), false);
            out.push(self.event_line(Target::Channel(chan), line));
        }
    }

    fn on_nick(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let (Some(old), Some(new)) = (msg.nick().map(str::to_owned), msg.params.first().cloned()) else { return };
        let own = self.is_me(&old);
        if own {
            self.nick = new.clone();
            out.push(Output::Event(Event::OwnNick { network: self.network.clone(), nick: new.clone() }));
        } else {
            out.push(self.peer(&old, PeerChange::Renamed(new.clone())));
        }
        for chan in self.channels_with(&old) {
            let m = self.members_of(&chan);
            m.remove(&old.to_ascii_lowercase());
            m.insert(new.to_ascii_lowercase());
            let line = self.line(msg, now, LineKind::Nick, Some(&old), format!("{old} is now known as {new}"), own);
            out.push(self.event_line(Target::Channel(chan), line));
        }
    }

    fn on_topic(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let [chan, topic, ..] = msg.params.as_slice() else { return };
        let (chan, topic) = (chan.clone(), topic.clone());
        let by = msg.nick().unwrap_or("").to_owned();
        out.push(Output::Event(Event::Topic { network: self.network.clone(), channel: chan.clone(), topic: Some(topic.clone()).filter(|t| !t.is_empty()) }));
        let line = self.line(msg, now, LineKind::Topic, Some(&by), format!("{by} changed the topic to: {topic}"), self.is_me(&by));
        out.push(self.event_line(Target::Channel(chan), line));
    }

    fn on_names(&mut self, msg: &Message, out: &mut Vec<Output>) {
        let [_, _, chan, names, ..] = msg.params.as_slice() else { return };
        let nicks: Vec<String> = names.split_whitespace().map(|n| self.isupport.bare_nick(n).to_owned()).collect();
        for nick in nicks.iter().filter(|n| !self.is_me(n)) {
            out.push(self.peer(nick, PeerChange::Seen { host: None }));
        }
        let chan = chan.clone();
        self.members_of(&chan).extend(nicks.iter().map(|n| n.to_ascii_lowercase()));
    }

    fn on_mode(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let Some(target) = msg.params.first().cloned() else { return };
        let by = msg.nick().unwrap_or("").to_owned();
        let modes = msg.params[1..].join(" ");
        let (dest, own) = if self.isupport.is_channel(&target) { (Target::Channel(target), self.is_me(&by)) } else { (Target::Server, true) };
        let line = self.line(msg, now, LineKind::Mode, Some(&by), format!("{by} set mode {modes}"), own);
        out.push(self.event_line(dest, line));
    }

    pub fn command(&mut self, cmd: Command, now: DateTime<Utc>) -> Vec<Output> {
        match cmd {
            Command::Say { target, text, .. } => self.say(&target, &text, false, now),
            Command::Action { target, text, .. } => self.say(&target, &text, true, now),
            Command::Join { channel, key, .. } => vec![send("JOIN", std::iter::once(channel).chain(key))],
            Command::Part { channel, reason, .. } => vec![send("PART", std::iter::once(channel).chain(reason))],
            Command::Nick { nick, .. } => vec![send("NICK", [nick])],
            Command::Raw { line, .. } => match Message::parse(&line) {
                Ok(m) => vec![Output::Send(m)],
                Err(e) => {
                    let line = self.local(LineKind::Error, format!("Can't send {line:?}: {e}"), now);
                    vec![self.event_line(Target::Server, line)]
                }
            },
            Command::Disconnect { reason, .. } => {
                vec![send("QUIT", reason.into_iter().collect::<Vec<_>>()), Output::Quit]
            }
            Command::Reconnect { .. } => Vec::new(),
        }
    }

    fn local(&mut self, kind: LineKind, text: String, now: DateTime<Utc>) -> Line {
        self.seq += 1;
        Line { seq: self.seq, time: now, msgid: None, kind, nick: None, text, own: true, delivery: Delivery::Sent, boundary: false }
    }

    /// PRIVMSG, split to fit. Each piece shows at once as our own line.
    /// With echo-message it is pending until the server's echo, the copy
    /// that stays (right time, msgid, order), takes its place (the store
    /// matches them); without it, ours is the copy and it is sent.
    fn say(&mut self, target: &str, text: &str, action: bool, now: DateTime<Utc>) -> Vec<Output> {
        let echo = self.caps.is_enabled("echo-message");
        let wrap = if action { "\x01ACTION \x01".len() } else { 0 };
        let budget = split::body_budget("PRIVMSG", target, self.nick.len() + 75).saturating_sub(wrap).max(1);
        let dest = if self.isupport.is_channel(target) { Target::Channel(target.to_owned()) } else { Target::Query(target.to_owned()) };
        let mut out = Vec::new();
        // Several lines, and the server takes them as one message: a batch
        // (or several, to stay inside its limits), each shown as one line
        // of ours that its echo replaces, as below.
        if !action
            && multiline::lines(text).len() > 1
            && let Some(limits) = self.multiline()
        {
            for batch in multiline::batches(text, budget, &limits) {
                self.batch_seq += 1;
                let r = format!("hirc{}", self.batch_seq);
                let joined = multiline::join(batch.iter().map(|p| (p.text, p.concat)));
                let mut l = self.local(LineKind::Privmsg, joined, now);
                l.nick = Some(self.nick.clone());
                if echo {
                    l.delivery = Delivery::Pending;
                }
                out.push(self.event_line(dest.clone(), l));
                out.push(send("BATCH", [format!("+{r}"), multiline::CAP.to_owned(), target.to_owned()]));
                for p in &batch {
                    let mut m = Message::new("PRIVMSG", [target, p.text]);
                    m.tags.push(("batch".into(), Some(r.clone())));
                    if p.concat {
                        m.tags.push((multiline::CONCAT.into(), None));
                    }
                    out.push(Output::Send(m));
                }
                out.push(send("BATCH", [format!("-{r}")]));
            }
            return out;
        }
        for line in text.lines().filter(|l| !l.is_empty()) {
            for chunk in split::split_utf8(line, budget) {
                let body = if action { format!("\x01ACTION {chunk}\x01") } else { chunk.to_owned() };
                let mut l = self.local(if action { LineKind::Action } else { LineKind::Privmsg }, chunk.to_owned(), now);
                l.nick = Some(self.nick.clone());
                if echo {
                    l.delivery = Delivery::Pending;
                }
                // Shown before it goes, so an echo can never beat it.
                out.push(self.event_line(dest.clone(), l));
                out.push(send("PRIVMSG", [target.to_owned(), body]));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn now() -> DateTime<Utc> {
        "2026-10-22T12:00:00Z".parse().unwrap()
    }

    pub(super) fn cfg() -> SessionConfig {
        SessionConfig {
            nickname: "alice".into(),
            alt_nicks: vec!["alice2".into()],
            username: "alice".into(),
            realname: "Alice".into(),
            password: None,
            sasl: None,
            channels: vec!["#test".into()],
            bridges: vec![],
        }
    }

    pub(super) fn feed(s: &mut Session, line: &str) -> Vec<Output> {
        s.handle(Message::parse(line).unwrap(), now())
    }

    pub(super) fn sent(out: &[Output]) -> Vec<String> {
        out.iter()
            .filter_map(|o| match o {
                Output::Send(m) => Some(m.to_wire()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn start_sends_pass_then_cap_nick_user() {
        let mut s = Session::new("net", SessionConfig { password: Some("pw".into()), ..cfg() });
        assert_eq!(sent(&s.start()), ["PASS pw", "CAP LS 302", "NICK alice", "USER alice 0 * Alice"]);
    }

    #[test]
    fn negotiates_caps_then_sasl_then_joins() {
        let mut s = Session::new("net", SessionConfig { sasl: Some(SaslPlain { username: "alice".into(), password: "hunter22".into() }), ..cfg() });
        s.start();
        assert!(sent(&feed(&mut s, ":srv CAP * LS * :sasl=PLAIN,EXTERNAL server-time")).is_empty());
        assert_eq!(sent(&feed(&mut s, ":srv CAP * LS :echo-message message-tags")), ["CAP REQ :message-tags server-time echo-message sasl"]);
        assert_eq!(sent(&feed(&mut s, ":srv CAP alice ACK :message-tags server-time echo-message sasl")), ["AUTHENTICATE PLAIN"]);
        assert_eq!(sent(&feed(&mut s, "AUTHENTICATE +")), [format!("AUTHENTICATE {}", sasl::plain("", "alice", "hunter22"))]);
        assert!(sent(&feed(&mut s, ":srv 900 alice alice!a@h alice :You are now logged in")).is_empty());
        assert_eq!(sent(&feed(&mut s, ":srv 903 alice :SASL authentication successful")), ["CAP END"]);
        let out = feed(&mut s, ":srv 001 alice :Welcome");
        assert_eq!(sent(&out), ["JOIN #test"]);
        assert!(out.contains(&Output::Event(Event::Status { network: "net".into(), status: ConnStatus::Connected { nick: "alice".into() } })));
        assert!(s.caps().is_enabled("echo-message"));
    }

    #[test]
    fn welcome_before_sasl_stops() {
        let mut s = Session::new("net", SessionConfig { sasl: Some(SaslPlain { username: "alice".into(), password: "pw".into() }), ..cfg() });
        s.start();
        let out = feed(&mut s, ":srv 001 alice :Welcome");
        assert!(out.contains(&Output::Stop("server registered us before SASL finished".into())), "{out:?}");
        assert!(sent(&out).is_empty(), "{out:?}");
        assert!(!out.iter().any(|o| matches!(o, Output::Event(_))), "{out:?}");
    }

    #[test]
    fn sasl_failure_stops() {
        let mut s = Session::new("net", SessionConfig { sasl: Some(SaslPlain { username: "alice".into(), password: "bad".into() }), ..cfg() });
        s.start();
        feed(&mut s, ":srv CAP * LS :sasl");
        feed(&mut s, ":srv CAP alice ACK :sasl");
        feed(&mut s, "AUTHENTICATE +");
        let out = feed(&mut s, ":srv 904 alice :SASL authentication failed");
        assert!(out.iter().any(|o| matches!(o, Output::Stop(r) if r.contains("SASL"))), "{out:?}");
    }

    #[test]
    fn sasl_configured_but_not_offered_stops() {
        let mut s = Session::new("net", SessionConfig { sasl: Some(SaslPlain { username: "a".into(), password: "b".into() }), ..cfg() });
        s.start();
        feed(&mut s, ":srv CAP * LS :server-time");
        let out = feed(&mut s, ":srv CAP alice ACK :server-time");
        assert!(out.iter().any(|o| matches!(o, Output::Stop(_))), "{out:?}");
    }

    #[test]
    fn no_wanted_caps_ends_negotiation() {
        let mut s = Session::new("net", cfg());
        s.start();
        assert_eq!(sent(&feed(&mut s, ":srv CAP * LS :znc.in/playback")), ["CAP END"]);
    }

    #[test]
    fn nak_ends_negotiation() {
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, ":srv CAP * LS :batch");
        assert_eq!(sent(&feed(&mut s, ":srv CAP alice NAK :batch")), ["CAP END"]);
    }

    #[test]
    fn nick_in_use_tries_alternatives() {
        let mut s = Session::new("net", cfg());
        s.start();
        assert_eq!(sent(&feed(&mut s, ":srv 433 * alice :Nickname is already in use")), ["NICK alice2"]);
        assert_eq!(sent(&feed(&mut s, ":srv 433 * alice2 :Nickname is already in use")), ["NICK alice2_"]);
    }

    #[test]
    fn answers_ping() {
        let mut s = Session::new("net", cfg());
        assert_eq!(sent(&feed(&mut s, "PING :abc")), ["PONG abc"]);
    }

    #[test]
    fn cap_new_and_del_after_registration() {
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, ":srv CAP * LS :server-time");
        feed(&mut s, ":srv CAP alice ACK :server-time");
        feed(&mut s, ":srv 001 alice :hi");
        assert_eq!(sent(&feed(&mut s, ":srv CAP alice NEW :batch")), ["CAP REQ batch"]);
        feed(&mut s, ":srv CAP alice ACK :batch");
        assert!(s.caps().is_enabled("batch"));
        feed(&mut s, ":srv CAP alice DEL :batch");
        assert!(!s.caps().is_enabled("batch"));
    }

    #[test]
    fn debug_redacts_passwords() {
        let c = SessionConfig { password: Some("srvsecret".into()), sasl: Some(SaslPlain { username: "a".into(), password: "saslsecret".into() }), ..cfg() };
        let d = format!("{c:?}");
        assert!(!d.contains("srvsecret") && !d.contains("saslsecret"), "{d}");
        assert!(d.contains("<redacted>"));
    }

    #[test]
    fn nick_in_use_is_bounded() {
        let mut s = Session::new("net", cfg());
        s.start();
        let mut nicks = Vec::new();
        for _ in 0..4 {
            nicks.extend(sent(&feed(&mut s, ":srv 433 * x :in use")));
        }
        assert_eq!(nicks, ["NICK alice2", "NICK alice2_", "NICK alice2__", "NICK alice2___"]);
        let out = feed(&mut s, ":srv 433 * x :in use");
        assert!(sent(&out).is_empty());
        assert!(out.contains(&Output::Stop("no usable nickname".into())));
    }

    #[test]
    fn erroneous_nick_without_alts_stops() {
        let mut s = Session::new("net", SessionConfig { alt_nicks: vec![], ..cfg() });
        s.start();
        let out = feed(&mut s, ":srv 432 * bad :Erroneous nickname");
        assert!(sent(&out).is_empty());
        assert!(out.contains(&Output::Stop("no usable nickname".into())));
    }

    #[test]
    fn sasl_success_ignored_outside_sasl() {
        let mut s = Session::new("net", cfg());
        s.start();
        assert!(sent(&feed(&mut s, ":srv 903 alice :ok")).is_empty());
    }

    fn registered(caps: &str) -> Session {
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, &format!(":srv CAP * LS :{caps}"));
        feed(&mut s, &format!(":srv CAP alice ACK :{caps}"));
        feed(&mut s, ":srv 001 alice :hi");
        feed(&mut s, ":srv 005 alice CHANTYPES=# PREFIX=(ov)@+ :are supported");
        s
    }

    fn lines(out: &[Output]) -> Vec<(Target, Line)> {
        out.iter()
            .filter_map(|o| match o {
                Output::Event(Event::Line { target, line, .. }) => Some((target.clone(), line.clone())),
                _ => None,
            })
            .collect()
    }

    const ECHO: &str = "message-tags server-time echo-message";

    #[test]
    fn channel_message_uses_server_time_and_msgid() {
        let mut s = registered(ECHO);
        let out = feed(&mut s, "@time=2026-10-22T10:00:00.000Z;msgid=abc :bob!b@h PRIVMSG #test :hello there");
        let (t, l) = &lines(&out)[0];
        assert_eq!(*t, Target::Channel("#test".into()));
        assert_eq!(l.time, "2026-10-22T10:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(l.msgid.as_deref(), Some("abc"));
        assert_eq!((l.kind, l.nick.as_deref(), l.text.as_str(), l.own), (LineKind::Privmsg, Some("bob"), "hello there", false));
    }

    #[test]
    fn missing_time_tag_uses_receive_time() {
        let mut s = registered(ECHO);
        assert_eq!(lines(&feed(&mut s, ":bob!b@h PRIVMSG #test :x"))[0].1.time, now());
    }

    #[test]
    fn dms_route_to_the_other_party() {
        let mut s = registered(ECHO);
        let (t, l) = lines(&feed(&mut s, ":bob!b@h PRIVMSG alice :psst")).remove(0);
        assert_eq!((t, l.own), (Target::Query("bob".into()), false));
        let (t, l) = lines(&feed(&mut s, ":alice!a@h PRIVMSG bob :hey")).remove(0);
        assert_eq!((t, l.own), (Target::Query("bob".into()), true));
    }

    #[test]
    fn a_bridge_relays_lines_as_their_authors() {
        let mut s = registered(ECHO);
        s.cfg.bridges = vec!["IrcBot".into()];
        let said = |s: &mut Session, line: &str| {
            let (t, l) = lines(&feed(s, line)).remove(0);
            (t, l.kind, l.nick.unwrap_or_default(), l.text)
        };
        let chan = Target::Channel("#test".into());
        // Listed (in any case): the relayed name and text, zero-width gone.
        let got = said(&mut s, ":ircbot!b@h PRIVMSG #test :<ha\u{200B}cker> The only drawback");
        assert_eq!(got, (chan.clone(), LineKind::Privmsg, "hacker".into(), "The only drawback".into()));
        let got = said(&mut s, ":IrcBot!b@h PRIVMSG #test :* hacker waves");
        assert_eq!(got, (chan.clone(), LineKind::Action, "hacker".into(), "waves".into()));
        // The bot's own words and its own CTCP ACTION stay its.
        let got = said(&mut s, ":IrcBot!b@h PRIVMSG #test :Bridge restarted");
        assert_eq!(got, (chan.clone(), LineKind::Privmsg, "IrcBot".into(), "Bridge restarted".into()));
        let got = said(&mut s, ":IrcBot!b@h PRIVMSG #test :\x01ACTION <hacker> waves\x01");
        assert_eq!(got, (chan.clone(), LineKind::Action, "IrcBot".into(), "<hacker> waves".into()));
        // An unlisted nick is never unwrapped.
        let got = said(&mut s, ":OtherBot!b@h PRIVMSG #test :<hacker> hi");
        assert_eq!(got, (chan.clone(), LineKind::Privmsg, "OtherBot".into(), "<hacker> hi".into()));
        // The bot itself is still the peer seen.
        assert_eq!(peers(&feed(&mut s, ":IrcBot!b@h PRIVMSG #test :<hacker> x"))[0].0, "IrcBot");
    }

    #[test]
    fn actions_notices_and_ctcp() {
        let mut s = registered(ECHO);
        let (_, l) = lines(&feed(&mut s, ":bob!b@h PRIVMSG #test :\x01ACTION waves\x01")).remove(0);
        assert_eq!((l.kind, l.text.as_str()), (LineKind::Action, "waves"));
        let (t, l) = lines(&feed(&mut s, ":srv NOTICE alice :server says hi")).remove(0);
        assert_eq!((t, l.kind), (Target::Server, LineKind::Notice));
        let (t, _) = lines(&feed(&mut s, ":NickServ!NickServ@services NOTICE alice :You are now identified")).remove(0);
        assert_eq!(t, Target::Query("NickServ".into()));
        assert!(lines(&feed(&mut s, ":bob!b@h PRIVMSG alice :\x01VERSION\x01")).is_empty());
    }

    #[test]
    fn say_with_echo_message_shows_a_pending_line() {
        let mut s = registered(ECHO);
        let out = s.command(Command::Say { network: "net".into(), target: "#test".into(), text: "hi".into() }, now());
        assert_eq!(sent(&out), ["PRIVMSG #test hi"]);
        let (t, l) = lines(&out).remove(0);
        assert_eq!((t, l.own, l.delivery, l.text.as_str()), (Target::Channel("#test".into()), true, Delivery::Pending, "hi"));
    }

    #[test]
    fn say_without_echo_message_shows_locally() {
        let mut s = registered("server-time");
        let out = s.command(Command::Say { network: "net".into(), target: "#test".into(), text: "hi".into() }, now());
        let (t, l) = lines(&out).remove(0);
        assert_eq!((t, l.own, l.nick.as_deref(), l.delivery), (Target::Channel("#test".into()), true, Some("alice"), Delivery::Sent));
    }

    fn peers(out: &[Output]) -> Vec<(String, PeerChange)> {
        out.iter()
            .filter_map(|o| match o {
                Output::Event(Event::Peer { nick, change, .. }) => Some((nick.clone(), change.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn other_people_are_tracked() {
        let mut s = registered(ECHO);
        feed(&mut s, ":alice!a@h JOIN #test");
        let seen = |host: Option<&str>| PeerChange::Seen { host: host.map(Into::into) };
        assert_eq!(peers(&feed(&mut s, ":srv 353 alice = #test :alice @bob")), [("bob".to_owned(), seen(None))]);
        assert_eq!(peers(&feed(&mut s, ":carol!c@h.example JOIN #test")), [("carol".to_owned(), seen(Some("c@h.example")))]);
        assert_eq!(peers(&feed(&mut s, ":dan!d@h PRIVMSG alice :hi")), [("dan".to_owned(), seen(Some("d@h")))]);
        assert_eq!(peers(&feed(&mut s, ":bob!b@h AWAY :lunch")), [("bob".to_owned(), PeerChange::Away(Some("lunch".into())))]);
        assert_eq!(peers(&feed(&mut s, ":bob!b@h AWAY")), [("bob".to_owned(), PeerChange::Away(None))]);
        assert_eq!(peers(&feed(&mut s, ":srv 301 alice dan :gone fishing")), [("dan".to_owned(), PeerChange::Away(Some("gone fishing".into())))]);
        assert_eq!(peers(&feed(&mut s, ":carol!c@h NICK carla")), [("carol".to_owned(), PeerChange::Renamed("carla".into()))]);
        assert_eq!(peers(&feed(&mut s, ":bob!b@h QUIT :bye")), [("bob".to_owned(), PeerChange::Quit("bye".into()))]);
        // Our own comings and goings are not a peer's.
        assert!(peers(&feed(&mut s, ":alice!a@h NICK alicia")).is_empty());
        assert!(peers(&feed(&mut s, ":alicia!a@h PRIVMSG #test :mine")).is_empty());
    }

    #[test]
    fn long_and_multiline_say_is_split() {
        let mut s = registered(ECHO);
        let out = s.command(Command::Say { network: "net".into(), target: "#test".into(), text: format!("{}\nsecond", "a".repeat(1000)) }, now());
        let msgs = sent(&out);
        assert_eq!(msgs.len(), 4);
        assert!(msgs.iter().all(|m| m.len() + 2 + 1 + "alice".len() + 75 < 512), "{msgs:?}");
        assert_eq!(msgs[3], "PRIVMSG #test second");
    }

    /// Registered with echo-message, batch and `draft/multiline=<value>`.
    fn multiline(value: &str) -> Session {
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, &format!(":srv CAP * LS :message-tags server-time echo-message batch draft/multiline={value}"));
        feed(&mut s, ":srv CAP alice ACK :message-tags server-time echo-message batch draft/multiline");
        feed(&mut s, ":srv 001 alice :hi");
        feed(&mut s, ":srv 005 alice CHANTYPES=# PREFIX=(ov)@+ :are supported");
        s
    }

    fn say(s: &mut Session, text: &str) -> Vec<Output> {
        s.command(Command::Say { network: "net".into(), target: "#test".into(), text: text.into() }, now())
    }

    #[test]
    fn multiline_limits_are_the_servers_and_reach_the_ui() {
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, ":srv CAP * LS :message-tags batch draft/multiline=max-bytes=4096,max-lines=24");
        let out = feed(&mut s, ":srv CAP alice ACK :message-tags batch draft/multiline");
        let limits = Some(Limits { max_bytes: 4096, max_lines: Some(24) });
        assert!(out.contains(&Output::Event(Event::Multiline { network: "net".into(), limits })), "{out:?}");
        assert_eq!(s.multiline(), limits);
        let out = feed(&mut s, ":srv CAP alice DEL :draft/multiline");
        assert!(out.contains(&Output::Event(Event::Multiline { network: "net".into(), limits: None })), "{out:?}");
        // Without batch the cap is no use; a garbled value is none either.
        assert_eq!(multiline("max-lines=5").multiline(), None);
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, ":srv CAP * LS :message-tags draft/multiline=max-bytes=4096");
        feed(&mut s, ":srv CAP alice ACK :message-tags draft/multiline");
        assert_eq!(s.multiline(), None);
    }

    #[test]
    fn multiline_say_is_one_batch_shown_as_one_pending_line() {
        let mut s = multiline("max-bytes=4096,max-lines=24");
        let out = say(&mut s, "one\n\nthree");
        assert_eq!(
            sent(&out),
            [
                "BATCH +hirc1 draft/multiline #test",
                "@batch=hirc1 PRIVMSG #test one",
                "@batch=hirc1 PRIVMSG #test :",
                "@batch=hirc1 PRIVMSG #test three",
                "BATCH -hirc1",
            ]
        );
        let shown = lines(&out);
        assert_eq!(shown.len(), 1);
        assert_eq!((shown[0].1.text.as_str(), shown[0].1.delivery), ("one\n\nthree", Delivery::Pending));
        // A second send gets a ref of its own.
        assert_eq!(sent(&say(&mut s, "a\nb"))[0], "BATCH +hirc2 draft/multiline #test");
    }

    #[test]
    fn single_line_say_stays_a_plain_privmsg() {
        let mut s = multiline("max-bytes=4096");
        assert_eq!(sent(&say(&mut s, "\nhi\n")), ["PRIVMSG #test hi"]);
    }

    #[test]
    fn a_long_line_in_a_batch_continues_with_concat() {
        let mut s = multiline("max-bytes=4096");
        let long = format!("{} {}", "a".repeat(300), "b".repeat(300));
        let msgs = sent(&say(&mut s, &format!("{long}\nend")));
        assert_eq!(msgs.len(), 5, "{msgs:?}");
        assert_eq!(msgs[1], format!("@batch=hirc1 PRIVMSG #test :{} ", "a".repeat(300)));
        assert_eq!(msgs[2], format!("@batch=hirc1;draft/multiline-concat PRIVMSG #test {}", "b".repeat(300)));
        assert_eq!(msgs[3], "@batch=hirc1 PRIVMSG #test end");
    }

    #[test]
    fn multiline_say_splits_into_batches_by_max_lines_and_max_bytes() {
        let mut s = multiline("max-bytes=4096,max-lines=2");
        let out = say(&mut s, "1\n2\n3");
        let msgs = sent(&out);
        assert_eq!(msgs.iter().filter(|m| m.starts_with("BATCH +")).count(), 2, "{msgs:?}");
        assert_eq!(msgs[3], "BATCH -hirc1");
        assert_eq!(msgs[4], "BATCH +hirc2 draft/multiline #test");
        let shown: Vec<String> = lines(&out).into_iter().map(|(_, l)| l.text).collect();
        assert_eq!(shown, ["1\n2", "3"]);

        let mut s = multiline("max-bytes=10");
        let shown: Vec<String> = lines(&say(&mut s, "aaaaa\nbbbbb\nccccc")).into_iter().map(|(_, l)| l.text).collect();
        assert_eq!(shown, ["aaaaa\nbbbbb", "ccccc"]);
    }

    #[test]
    fn without_the_cap_each_line_is_its_own_privmsg() {
        let mut s = registered(ECHO);
        assert_eq!(sent(&say(&mut s, "one\n\ntwo")), ["PRIVMSG #test one", "PRIVMSG #test two"]);
    }

    #[test]
    fn a_received_batch_is_one_message() {
        let mut s = multiline("max-bytes=4096");
        assert!(feed(&mut s, "@time=2026-10-22T10:00:00.000Z;msgid=m1 :bob!b@h BATCH +x draft/multiline #test").is_empty());
        assert!(feed(&mut s, "@batch=x :bob!b@h PRIVMSG #test :first ").is_empty());
        assert!(feed(&mut s, "@batch=x;draft/multiline-concat :bob!b@h PRIVMSG #test :half").is_empty());
        assert!(feed(&mut s, "@batch=x :bob!b@h PRIVMSG #test :").is_empty());
        assert!(feed(&mut s, "@batch=x :bob!b@h PRIVMSG #test :last").is_empty());
        let got = lines(&feed(&mut s, ":bob!b@h BATCH -x"));
        assert_eq!(got.len(), 1);
        let (t, l) = &got[0];
        assert_eq!(*t, Target::Channel("#test".into()));
        assert_eq!((l.nick.as_deref(), l.text.as_str(), l.msgid.as_deref()), (Some("bob"), "first half\n\nlast", Some("m1")));
        assert_eq!(l.time, "2026-10-22T10:00:00Z".parse::<DateTime<Utc>>().unwrap());
    }

    #[test]
    fn other_batches_pass_through() {
        let mut s = multiline("max-bytes=4096");
        feed(&mut s, ":srv BATCH +n netsplit irc.a irc.b");
        assert_eq!(lines(&feed(&mut s, "@batch=n :bob!b@h PRIVMSG #test :still here")).len(), 1);
        assert!(lines(&feed(&mut s, ":srv BATCH -n")).is_empty());
    }

    #[test]
    fn our_echoed_batch_replaces_the_pending_line() {
        let mut s = multiline("max-bytes=4096");
        let mut store = crate::store::Store::new(["net".to_owned()]);
        let apply = |store: &mut crate::store::Store, out: Vec<Output>| {
            out.into_iter()
                .filter_map(|o| match o {
                    Output::Event(e) => Some(store.apply(e)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        apply(&mut store, say(&mut s, "hello\nworld"));
        feed(&mut s, "@msgid=e1 :alice!a@h BATCH +srv1 draft/multiline #test");
        feed(&mut s, "@batch=srv1 :alice!a@h PRIVMSG #test hello");
        feed(&mut s, "@batch=srv1 :alice!a@h PRIVMSG #test world");
        let applied = apply(&mut store, feed(&mut s, ":alice!a@h BATCH -srv1"));
        assert!(matches!(applied[..], [crate::store::Applied::Replaced { .. }]), "{applied:?}");
        let buf = &store.networks[0].buffers[1];
        assert_eq!(buf.lines.len(), 1);
        assert_eq!((buf.lines[0].text.as_str(), buf.lines[0].delivery, buf.lines[0].msgid.as_deref()), ("hello\nworld", Delivery::Sent, Some("e1")));
    }

    #[test]
    fn an_unterminated_batch_is_flushed_not_leaked() {
        let mut s = multiline("max-bytes=4096");
        feed(&mut s, ":bob!b@h BATCH +x draft/multiline #test");
        feed(&mut s, "@batch=x :bob!b@h PRIVMSG #test :cut");
        feed(&mut s, "@batch=x :bob!b@h PRIVMSG #test :off");
        let got = lines(&s.end(now()));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.text, "cut\noff");
        assert!(s.end(now()).is_empty());
        // The same ref opened again: the old one is shown, the new one collected.
        feed(&mut s, ":bob!b@h BATCH +y draft/multiline #test");
        feed(&mut s, "@batch=y :bob!b@h PRIVMSG #test :old");
        let got = lines(&feed(&mut s, ":bob!b@h BATCH +y draft/multiline #test"));
        assert_eq!(got[0].1.text, "old");
        feed(&mut s, "@batch=y :bob!b@h PRIVMSG #test :new");
        assert_eq!(lines(&feed(&mut s, ":bob!b@h BATCH -y"))[0].1.text, "new");
    }

    #[test]
    fn a_bridged_batch_is_unwrapped_per_line() {
        let mut s = multiline("max-bytes=4096");
        s.cfg.bridges = vec!["IrcBot".into()];
        let batch = |s: &mut Session, id: &str, texts: &[&str]| {
            feed(s, &format!("@msgid={id} :IrcBot!b@h BATCH +{id} draft/multiline #test"));
            for t in texts {
                feed(s, &format!("@batch={id} :IrcBot!b@h PRIVMSG #test :{t}"));
            }
            lines(&feed(s, &format!(":IrcBot!b@h BATCH -{id}")))
                .into_iter()
                .map(|(_, l)| (l.nick.unwrap_or_default(), l.text, l.msgid.unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        // One name on every line: one message from that name.
        let got = batch(&mut s, "a", &["<hacker> one", "<hacker> two"]);
        assert_eq!(got, [("hacker".into(), "one\ntwo".into(), "a".into())]);
        // Mixed: each line on its own, with msgids kept apart.
        let got = batch(&mut s, "b", &["<hacker> one", "<other> two", "bot words"]);
        let want: Vec<(String, String, String)> = vec![
            ("hacker".into(), "one".into(), "b".into()),
            ("other".into(), "two".into(), "b:1".into()),
            ("IrcBot".into(), "bot words".into(), "b:2".into()),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn action_command() {
        let mut s = registered(ECHO);
        let out = s.command(Command::Action { network: "net".into(), target: "#test".into(), text: "waves".into() }, now());
        assert_eq!(sent(&out), ["PRIVMSG #test :\x01ACTION waves\x01"]);
    }

    #[test]
    fn own_join_part_and_members_route_quit_and_nick() {
        let mut s = registered(ECHO);
        let out = feed(&mut s, ":alice!a@h JOIN #test");
        assert!(out.contains(&Output::Event(Event::Joined { network: "net".into(), channel: "#test".into() })));
        feed(&mut s, ":srv 353 alice = #test :alice @bob +carol");
        feed(&mut s, ":srv 366 alice #test :End of /NAMES list");
        let (t, l) = lines(&feed(&mut s, ":bob!b@h QUIT :bye")).remove(0);
        assert_eq!((t, l.kind, l.text.as_str()), (Target::Channel("#test".into()), LineKind::Quit, "bob quit (bye)"));
        let (_, l) = lines(&feed(&mut s, ":carol!c@h NICK carla")).remove(0);
        assert_eq!(l.text, "carol is now known as carla");
        assert!(lines(&feed(&mut s, ":carol!c@h QUIT :gone")).is_empty(), "carol was renamed");
        let out = feed(&mut s, ":alice!a@h PART #test :later");
        assert!(out.contains(&Output::Event(Event::Parted { network: "net".into(), channel: "#test".into() })));
    }

    #[test]
    fn own_nick_change() {
        let mut s = registered(ECHO);
        let out = feed(&mut s, ":alice!a@h NICK alicia");
        assert!(out.contains(&Output::Event(Event::OwnNick { network: "net".into(), nick: "alicia".into() })));
        assert_eq!(s.nick(), "alicia");
    }

    #[test]
    fn topics_kicks_modes() {
        let mut s = registered(ECHO);
        feed(&mut s, ":alice!a@h JOIN #test");
        let out = feed(&mut s, ":bob!b@h TOPIC #test :new topic");
        assert!(out.contains(&Output::Event(Event::Topic { network: "net".into(), channel: "#test".into(), topic: Some("new topic".into()) })));
        assert_eq!(lines(&out)[0].1.text, "bob changed the topic to: new topic");
        let out = feed(&mut s, ":srv 332 alice #test :old topic");
        assert!(out.contains(&Output::Event(Event::Topic { network: "net".into(), channel: "#test".into(), topic: Some("old topic".into()) })));
        assert_eq!(lines(&feed(&mut s, ":bob!b@h MODE #test +o carol"))[0].1.text, "bob set mode +o carol");
        let out = feed(&mut s, ":bob!b@h KICK #test alice :out");
        assert_eq!(lines(&out)[0].1.text, "alice was kicked by bob (out)");
        assert_eq!(lines(&out)[0].1.nick.as_deref(), Some("alice"));
        assert!(out.contains(&Output::Event(Event::Parted { network: "net".into(), channel: "#test".into() })));
    }

    #[test]
    fn numerics_and_errors_go_to_server_buffer() {
        let mut s = registered(ECHO);
        let (t, l) = lines(&feed(&mut s, ":srv 372 alice :- MOTD line")).remove(0);
        assert_eq!((t, l.kind, l.text.as_str()), (Target::Server, LineKind::Server, "- MOTD line"));
        // ERROR is kept as the reason for the close that follows.
        assert!(lines(&feed(&mut s, "ERROR :Closing link")).is_empty());
        assert_eq!(s.closing(), Some("Closing link"));
    }

    #[test]
    fn simple_commands() {
        let mut s = registered(ECHO);
        let n = || "net".to_owned();
        assert_eq!(sent(&s.command(Command::Join { network: n(), channel: "#x".into(), key: Some("k".into()) }, now())), ["JOIN #x k"]);
        assert_eq!(sent(&s.command(Command::Part { network: n(), channel: "#x".into(), reason: None }, now())), ["PART #x"]);
        assert_eq!(sent(&s.command(Command::Nick { network: n(), nick: "z".into() }, now())), ["NICK z"]);
        assert_eq!(sent(&s.command(Command::Raw { network: n(), line: "WHO #x".into() }, now())), ["WHO #x"]);
        let bad = s.command(Command::Raw { network: n(), line: "".into() }, now());
        assert_eq!(lines(&bad)[0].1.kind, LineKind::Error);
        let out = s.command(Command::Disconnect { network: n(), reason: Some("bye".into()) }, now());
        assert_eq!(sent(&out), ["QUIT bye"]);
        assert!(out.contains(&Output::Quit), "a quit is not a failure: {out:?}");
    }

    #[test]
    fn quit_and_nick_lines_keep_channel_spelling() {
        let mut s = registered(ECHO);
        feed(&mut s, ":alice!a@h JOIN #Test");
        feed(&mut s, ":srv 353 alice = #Test :alice @bob +carol");
        let (t, _) = lines(&feed(&mut s, ":bob!b@h QUIT :bye")).remove(0);
        assert_eq!(t, Target::Channel("#Test".into()));
        let (t, _) = lines(&feed(&mut s, ":carol!c@h NICK carla")).remove(0);
        assert_eq!(t, Target::Channel("#Test".into()));
    }
}
