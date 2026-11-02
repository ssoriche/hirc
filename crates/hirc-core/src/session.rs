//! One server's protocol state machine. No sockets: a line goes in, and
//! lines to send plus events for the UI come out. All registration, CAP
//! and routing logic lives here so it can be unit-tested; `conn` just
//! moves bytes.

use crate::batch::{self, Batches};
use crate::cache::{self, Above, Below, Entry};
use crate::caps::{self, Caps};
use crate::event::{Command, ConnStatus, Event, PeerChange, Target};
use crate::history::{self, Anchor, Book, Kind, Request};
use crate::isupport::Isupport;
use crate::multiline::{self, Limits};
use crate::proto::Message;
use crate::store::{Delivery, Line, LineKind};
use crate::{bridge, playback, sasl, services, split};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::{HashMap, HashSet};

/// The read-marker cap.
const READ_MARKER: &str = "draft/read-marker";

#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Send(Message),
    Event(Event),
    /// Close and don't reconnect until the user asks (bad password).
    Stop(String),
    /// The user quit: close and don't reconnect until they ask.
    Quit,
    /// The server has no history before the oldest line held for this
    /// target: its window ended, which is not the start of history. The
    /// network task reads the cache past its breaks, and says "start"
    /// only when the cache has nothing older either (`conn::resolve`).
    /// Only said while keeping a cache.
    CacheOlder(String),
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
    /// Ask the server for history (the config's `chathistory`).
    pub chathistory: bool,
    /// Lines per history page as configured (`history_page_size`).
    pub history_page: Option<usize>,
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
            .field("chathistory", &self.chathistory)
            .field("history_page", &self.history_page)
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
    /// Every open batch: how they nest, and the labels they answer.
    batches: Batches,
    /// History pages arriving, by batch ref.
    pages: HashMap<String, Page>,
    /// The history request out for each lowercased target
    /// (`history::TARGETS` for TARGETS): one at a time.
    inflight: HashMap<String, Inflight>,
    /// Our labels, to the in-flight key each answers.
    labels: HashMap<String, String>,
    label_seq: u64,
    /// What history is held per target. The network task carries it from
    /// one connection to the next.
    book: Book,
    /// Registration finished (end of MOTD) and open queries were synced.
    synced: bool,
    /// Lowercased targets whose gap (since the newest line held) is still
    /// being filled, with the newest live line seen meanwhile. Until the
    /// gap closes, live lines don't move the book's newest past it, so a
    /// fill cut off is asked for again by the next connection.
    gaps: HashMap<String, Option<Anchor>>,
    /// Lowercased targets whose page before gave up on this connection:
    /// asking again would only fail again.
    gave_up: HashSet<String>,
    /// `Older` for a target that had a request out, or that came before
    /// registration ended, by lowercased target: sent once that request
    /// is done (or at `sync`).
    queued_older: HashMap<String, String>,
    /// A request that met one in flight for its target, by lowercased
    /// target: sent as asked once that one is done, never dropped.
    queued: HashMap<String, (String, Kind)>,
    /// Lowercased targets whose server history ended on this connection
    /// (an empty or short page before, while keeping a cache): what is
    /// older comes from the cache alone.
    ended: HashSet<String>,
    /// Names (with their latest activity) in TARGETS answers still
    /// arriving, by batch ref.
    found: HashMap<String, Vec<Found>>,
    /// When this connection's discovery began: once its TARGETS are all
    /// answered, every target busy until then has been found.
    discovering: Option<DateTime<Utc>>,
    /// TARGETS requests sent on this connection (`MAX_TARGETS_PAGES`).
    targets_pages: usize,
    /// Numbers the batches we send, so each ref is new on the connection.
    batch_seq: u64,
    /// The multiline limits last told to the UI.
    multiline_said: Option<Limits>,
    /// Runs of lines for the history cache, each saying how it meets what
    /// the cache holds; `None` when there is no cache.
    keep: Option<Vec<Entry>>,
    /// Per lowercased target, what the next live line follows on from in
    /// the cache. Absent: not known (a gap opened, a new connection).
    tail: HashMap<String, Tail>,
    /// Once this connection's `PLAY` is out: when it asked, by our clock
    /// (or its `<t>`, if that is later). Without batches, a line timed at
    /// or before this is ZNC's replay: ZNC sends no brackets to a client
    /// with server-time, and replays buffer by buffer, not in time order.
    /// Approximate, by one-way latency plus clock skew.
    play: Option<DateTime<Utc>>,
}

/// The live end of a target, as the cache holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tail {
    /// The newest line kept there.
    At(String),
    /// A line shown since then that the cache can't keep: the next one
    /// kept doesn't follow on, whatever a page says later.
    Hole,
    /// The newest line of a replay that came after a hole. The replay is
    /// from before the hole, so more of it follows on from this line, but
    /// a live line doesn't.
    Behind(String),
}

/// A name in a TARGETS answer, and when it was last busy.
type Found = (String, Option<DateTime<Utc>>);

/// A `draft/multiline` batch being received: one message once it closes.
#[derive(Debug)]
struct Incoming {
    /// `BATCH +ref draft/multiline <target>`: the message's sender, and its
    /// time and msgid when the server tags them here.
    open: Message,
    lines: Vec<Message>,
    /// The history page this batch is part of, if any.
    history: Option<String>,
}

/// A page of history being received (a `chathistory` batch).
#[derive(Debug)]
struct Page {
    /// The target the batch names.
    target: String,
    /// The label it answers.
    label: Option<String>,
    lines: Vec<Line>,
    /// Messages in it, shown or not (a multiline batch is one). The limit
    /// counts these, so a page with `limit` of them may have more after it.
    items: usize,
    /// Items kept at most; past it they are counted, not kept.
    cap: usize,
    first: Option<Anchor>,
    last: Option<Anchor>,
    /// ZNC's buffer playback (`znc.in/playback`), not a CHATHISTORY page.
    playback: bool,
}

#[derive(Clone, Debug)]
struct Inflight {
    request: Request,
    /// Asked again by time after the server couldn't find the msgid.
    retried: bool,
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

/// `msg`'s `time` tag, when it has one that parses.
fn server_time(msg: &Message) -> Option<DateTime<Utc>> {
    msg.tag("time").and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc))
}

/// Nothing in `out` carries a made-up msgid to the server: not a
/// CHATHISTORY or MARKREAD anchor, not a tag. Only those two commands'
/// params are checked, because a PRIVMSG's text is the user's to say
/// anything in.
fn safe_to_send(out: &[Output]) -> bool {
    out.iter().all(|o| match o {
        Output::Send(m) => {
            let anchors = matches!(m.command.as_str(), "CHATHISTORY" | "MARKREAD");
            let anchor = anchors && m.params.iter().any(|p| p.strip_prefix("msgid=").is_some_and(crate::msgid::is_made_up));
            !anchor && !m.tags.iter().any(|(_, v)| v.as_deref().is_some_and(crate::msgid::is_made_up))
        }
        _ => true,
    })
}

// The event lines' words, shared by the live handlers and by replay, so a
// replayed event reads as its live copy did.

fn join_text(nick: &str) -> String {
    format!("{nick} joined")
}

fn part_text(nick: &str, reason: Option<&str>) -> String {
    match reason.filter(|r| !r.is_empty()) {
        Some(r) => format!("{nick} left ({r})"),
        None => format!("{nick} left"),
    }
}

fn kick_text(victim: &str, by: &str, reason: Option<&str>) -> String {
    match reason.filter(|r| !r.is_empty()) {
        Some(r) => format!("{victim} was kicked by {by} ({r})"),
        None => format!("{victim} was kicked by {by}"),
    }
}

fn quit_text(nick: &str, reason: &str) -> String {
    if reason.is_empty() { format!("{nick} quit") } else { format!("{nick} quit ({reason})") }
}

fn nick_text(old: &str, new: &str) -> String {
    format!("{old} is now known as {new}")
}

fn topic_text(by: &str, topic: &str) -> String {
    format!("{by} changed the topic to: {topic}")
}

fn mode_text(by: &str, modes: &str) -> String {
    format!("{by} set mode {modes}")
}

impl Session {
    pub fn new(network: &str, cfg: SessionConfig) -> Session {
        let wanted = if cfg.chathistory { caps::WANTED } else { caps::WITHOUT_PLAYBACK };
        Session {
            network: network.to_owned(),
            nick: cfg.nickname.clone(),
            cfg,
            alt: 0,
            reg: Reg::CapLs,
            caps: Caps::new(wanted),
            isupport: Isupport::default(),
            members: HashMap::new(),
            seq: 0,
            authed: false,
            closing: None,
            incoming: Vec::new(),
            batches: Batches::default(),
            pages: HashMap::new(),
            inflight: HashMap::new(),
            labels: HashMap::new(),
            label_seq: 0,
            book: Book::default(),
            synced: false,
            gaps: HashMap::new(),
            gave_up: HashSet::new(),
            queued_older: HashMap::new(),
            queued: HashMap::new(),
            ended: HashSet::new(),
            found: HashMap::new(),
            discovering: None,
            targets_pages: 0,
            batch_seq: 0,
            multiline_said: None,
            keep: None,
            tail: HashMap::new(),
            play: None,
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

    /// Start from what earlier connections (or the cache) hold.
    pub fn with_book(mut self, book: Book) -> Session {
        self.book = book;
        self
    }

    /// What this session holds, for the next connection.
    pub fn take_book(&mut self) -> Book {
        std::mem::take(&mut self.book)
    }

    pub fn book(&self) -> &Book {
        &self.book
    }

    /// Lines per history page: the configured size, capped by the server.
    pub fn page(&self) -> usize {
        history::page_size(self.cfg.history_page, self.isupport.chathistory)
    }

    /// Older lines for `target` from the cache, shown as a page of history.
    /// They are older than the oldest held, so only the oldest moves (on a
    /// tie in time too: they were stored before it), and the next page
    /// starts before them. Not a server page: no request, gap or queued
    /// `Older` is touched.
    pub fn cached(&mut self, target: &str, lines: Vec<Line>) -> Vec<Output> {
        let query = !self.isupport.is_channel(target);
        let first = lines.first().map(Anchor::of);
        // Nothing held yet: the page is all there is, both ends.
        let last = lines.last().map(Anchor::of).filter(|_| self.book.newest(target).is_none());
        if first.is_some() {
            self.book.reach(target, query, first.as_ref(), last.as_ref());
        }
        vec![self.page_event(target, lines, false, true)]
    }

    /// Say what to write to the history cache, through `kept`.
    pub fn keeping(mut self) -> Session {
        self.keep = Some(Vec::new());
        self
    }

    /// The runs for the history cache since last asked. Only this session
    /// knows how each meets what is held: a page after follows on from
    /// where it was asked from, a page before reaches up to it, a live line
    /// follows the last one; anything else (a page nobody asked for, one
    /// cut off, the latest page of a window with nothing held) is a break.
    pub fn kept(&mut self) -> Vec<Entry> {
        self.keep.as_mut().map(std::mem::take).unwrap_or_default()
    }

    fn keep_run(&mut self, target: &str, lines: &[Line], below: Below, above: Above, older: bool) {
        let target = self.dest(target);
        if let Some(keep) = &mut self.keep
            && !lines.is_empty()
        {
            keep.push(Entry { network: self.network.clone(), target, lines: lines.to_vec(), below, above, older });
        }
    }

    /// A live line goes to the cache following the last one kept. One the
    /// cache can't keep (ours, on a server without echo-message, has no
    /// msgid) is a hole: the next line kept is a break.
    fn keep_live(&mut self, target: &Target, line: &Line) {
        let Some(name) = target.name() else { return };
        let key = name.to_ascii_lowercase();
        if let (true, Some(id)) = (cache::cacheable(target, line), &line.msgid) {
            let below = match self.tail.get(&key) {
                Some(Tail::At(prev)) => Below::Joins(prev.clone()),
                Some(Tail::Hole | Tail::Behind(_)) | None => Below::Breaks,
            };
            if let Some(keep) = &mut self.keep {
                keep.push(Entry { network: self.network.clone(), target: target.clone(), lines: vec![line.clone()], below, above: Above::Leave, older: false });
            }
            self.tail.insert(key, Tail::At(id.clone()));
        } else if line.msgid.is_none() && line.delivery == Delivery::Sent && !line.boundary {
            // Behind a hole already: a replay still goes on from its line.
            if !matches!(self.tail.get(&key), Some(Tail::Behind(_))) {
                self.tail.insert(key, Tail::Hole);
            }
        }
    }

    /// The live end of `target` is `msgid`, unless something is known of
    /// it already: a line kept since, or a hole, which a page never fills.
    fn tail_unless_known(&mut self, target: &str, msgid: Option<String>) {
        if let Some(id) = msgid {
            self.tail.entry(target.to_ascii_lowercase()).or_insert(Tail::At(id));
        }
    }

    /// History can't come from this server: the user turned it off, or it
    /// registered us without it. The cache is all the history there is.
    pub fn history_off(&self) -> bool {
        !self.cfg.chathistory || (self.reg == Reg::Done && !self.history_on())
    }

    /// The server has nothing before the oldest held for `target` on this
    /// connection: older lines come from the cache, read past its breaks.
    pub fn server_ended(&self, target: &str) -> bool {
        self.ended.contains(&target.to_ascii_lowercase())
    }

    /// What the server said it supports, as of this connection.
    pub fn isupport(&self) -> &Isupport {
        &self.isupport
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
        self.batches.clear();
        self.pages.clear();
        self.inflight.clear();
        self.labels.clear();
        self.synced = false;
        self.gaps.clear();
        self.gave_up.clear();
        self.queued_older.clear();
        self.queued.clear();
        self.ended.clear();
        self.found.clear();
        self.discovering = None;
        self.targets_pages = 0;
        self.multiline_said = None;
        self.tail.clear();
        self.play = None;
        // A query's lines can come before registration ends and its gap is
        // asked for: until then they wait, as they would in an open gap.
        for q in self.book.queries() {
            if self.book.newest(&q).is_some() && !services::is_service(&q) {
                self.gaps.insert(q.to_ascii_lowercase(), None);
            }
        }
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
        // Pages cut off by the drop: what came is still history.
        for (_, page) in std::mem::take(&mut self.pages) {
            self.finish_page(page, true, &mut out);
        }
        self.note(&out);
        out
    }

    /// What one multiline batch may hold, while the server takes them.
    pub fn multiline(&self) -> Option<Limits> {
        let on = ["batch", "message-tags", multiline::CAP].iter().all(|c| self.caps.is_enabled(c));
        if on { self.caps.value(multiline::CAP).and_then(Limits::parse) } else { None }
    }

    pub fn handle(&mut self, msg: Message, now: DateTime<Utc>) -> Vec<Output> {
        let mut out = Vec::new();
        // The burst ends at the MOTD's end, or, from a server that sends
        // none (some bouncers), at its first PING or our first JOIN.
        let burst_over = match msg.command.as_str() {
            "376" | "422" | "PING" => true,
            "JOIN" => msg.nick().is_some_and(|n| self.is_me(n)),
            _ => false,
        };
        self.dispatch(msg, now, &mut out);
        if burst_over && self.reg == Reg::Done && !self.synced {
            self.synced = true;
            self.sync(now, &mut out);
        }
        self.note(&out);
        // Never with `out` in the message: it can hold AUTHENTICATE.
        debug_assert!(safe_to_send(&out), "a made-up msgid was about to go to the server");
        out
    }

    fn dispatch(&mut self, msg: Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        if let Some(r) = msg.tag("batch").map(str::to_owned) {
            if matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE")
                && let Some(i) = self.incoming.iter().position(|(k, _)| *k == r)
            {
                self.incoming[i].1.lines.push(msg);
                return;
            }
            // Inside a chathistory batch, however deep: history, not news.
            if msg.command != "BATCH"
                && let Some(h) = self.page_of(&r)
            {
                return self.replay(&h, &msg, now);
            }
            // One name in our TARGETS answer. Only as many as were asked for
            // are kept.
            if msg.command == "CHATHISTORY"
                && msg.params.first().map(String::as_str) == Some("TARGETS")
                && let Some(t) = self.within(&r, history::TARGETS_BATCH, &self.found)
                && let Some(name) = msg.params.get(1)
            {
                let limit = self.page();
                // `<time>`, though some servers write `timestamp=<time>`.
                let at = msg.params.get(2).map(|t| t.strip_prefix("timestamp=").unwrap_or(t));
                let at = at.and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc));
                if let Some(list) = self.found.get_mut(&t)
                    && list.len() < limit
                {
                    list.push((name.clone(), at));
                }
                return;
            }
        }
        // Our PLAY, echoed back: not a line, and no `*playback` window.
        if self.play_echo(&msg) {
            return;
        }
        // ZNC's replay without batches: anything timed at or before our
        // PLAY is history.
        if self.loose_replay(&msg) {
            return self.replay_loose(&msg, now, out);
        }
        match msg.command.as_str() {
            "PING" => out.push(send("PONG", msg.params.clone())),
            "CAP" => self.on_cap(&msg, out),
            "AUTHENTICATE" => self.on_authenticate(&msg, out),
            "903" | "907" if self.reg == Reg::Sasl => {
                self.authed = true;
                self.end_cap(out);
            }
            "902" | "904" | "905" | "906" => out.push(Output::Stop(format!("SASL authentication failed: {}", last(&msg)))),
            "432" | "433" if self.reg != Reg::Done => match self.next_nick(msg.command == "433") {
                Some(n) => {
                    self.nick = n;
                    out.push(send("NICK", [self.nick.as_str()]));
                }
                None => out.push(Output::Stop("no usable nickname".into())),
            },
            "001" => self.on_welcome(&msg, now, out),
            "005" => self.isupport.apply(&msg.params),
            "PRIVMSG" | "NOTICE" => self.on_message(&msg, now, out),
            "JOIN" => self.on_join(&msg, now, out),
            "PART" => self.on_part(&msg, now, out),
            "KICK" => self.on_kick(&msg, now, out),
            "QUIT" => self.on_quit(&msg, now, out),
            "NICK" => self.on_nick(&msg, now, out),
            "TOPIC" => self.on_topic(&msg, now, out),
            "332" => {
                if let [_, chan, topic, ..] = msg.params.as_slice() {
                    out.push(Output::Event(Event::Topic { network: self.network.clone(), channel: chan.clone(), topic: Some(topic.clone()) }));
                }
            }
            "353" => self.on_names(&msg, out),
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
            "MODE" => self.on_mode(&msg, now, out),
            // No line of its own: the close follows, and its status line
            // gives this text as the reason.
            "ERROR" => self.closing = Some(last(&msg).to_owned()).filter(|t| !t.is_empty()),
            "BATCH" => self.on_batch(&msg, now, out),
            "FAIL" if msg.params.first().map(String::as_str) == Some("CHATHISTORY") => self.on_fail(&msg, now, out),
            "ACK" => self.on_ack(&msg, out),
            // draft/read-marker: where the user has read up to (`*`: nowhere yet).
            "MARKREAD" => {
                if let [target, at, ..] = msg.params.as_slice() {
                    out.push(Output::Event(Event::ReadMarker { network: self.network.clone(), target: self.dest(target), time: history::parse_timestamp(at) }));
                }
            }
            "TAGMSG" | "366" => {}
            c if c.len() == 3 && c.bytes().all(|b| b.is_ascii_digit()) => {
                let text = msg.params.get(1..).unwrap_or_default().join(" ");
                let line = self.line(&msg, now, LineKind::Server, None, text, false);
                out.push(self.event_line(Target::Server, line));
            }
            _ => {}
        }
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

    /// `BATCH +ref type ...` / `BATCH -ref`. Every batch is tracked (see
    /// `batch`); `draft/multiline` ones are collected into one message, and
    /// the lines of any other type are handled as they come.
    fn on_batch(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let Some(r) = msg.params.first() else { return };
        let (open, r) = match (r.strip_prefix('+'), r.strip_prefix('-')) {
            (Some(r), _) => (true, r.to_owned()),
            (_, Some(r)) => (false, r.to_owned()),
            _ => return,
        };
        // A close, or a ref opened again while its batch never closed: show
        // what it got.
        if let Some(i) = self.incoming.iter().position(|(k, _)| *k == r) {
            let (_, batch) = self.incoming.remove(i);
            self.finish(batch, now, out);
        }
        self.batches.close(&r);
        if let Some(page) = self.pages.remove(&r) {
            self.finish_page(page, false, out);
        }
        if let Some(names) = self.found.remove(&r) {
            self.finish_targets(names, out);
        }
        if !open {
            return;
        }
        self.batches.open(msg);
        match msg.params.get(1).map(String::as_str) {
            // Collected even when the tracker is full (`batch::MAX_OPEN`),
            // so its page is found from the batch line's own `batch` tag.
            Some(multiline::CAP) => {
                let history = msg.tag("batch").and_then(|p| self.page_of(p));
                self.incoming.push((r, Incoming { open: msg.clone(), lines: Vec::new(), history }));
            }
            // A page even when the tracker is full, so its lines are never
            // news. Pages are bounded on their own, like open batches.
            Some(history::BATCH) if self.pages.len() < batch::MAX_OPEN => {
                let target = msg.params.get(2).cloned().unwrap_or_default();
                let label = self.batch_label(&r, msg);
                let cap = self.answers(label.as_deref(), &target).and_then(|k| self.inflight.get(&k)).map_or(0, |i| i.request.limit).max(self.page());
                self.pages.insert(r, Page { target, label, lines: Vec::new(), items: 0, cap, first: None, last: None, playback: false });
            }
            // ZNC's buffer playback: history, whoever asked for it. Its ref
            // comes back on every replay of that buffer (closed above).
            Some(playback::BATCH) if self.pages.len() < batch::MAX_OPEN => {
                let target = msg.params.get(2).cloned().unwrap_or_default();
                let page = Page { target, label: None, lines: Vec::new(), items: 0, cap: playback::MAX_LINES, first: None, last: None, playback: true };
                self.pages.insert(r, page);
            }
            // Only the answer to our TARGETS: anyone else's names open nothing.
            Some(history::TARGETS_BATCH) if self.found.len() < batch::MAX_OPEN => {
                let label = self.batch_label(&r, msg);
                if self.answers(label.as_deref(), history::TARGETS).is_some() {
                    self.found.insert(r, Vec::new());
                }
            }
            _ => {}
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
        match batch.history {
            Some(h) if self.pages.contains_key(&h) => self.replay(&h, &msg, now),
            _ => self.on_message(&msg, now, out),
        }
    }

    /// History can be asked for: the user wants it, and the server has
    /// batches, chathistory and server-time (without times there are no
    /// anchors to page from).
    fn history_on(&self) -> bool {
        self.cfg.chathistory && ["batch", "server-time", history::CAP].iter().all(|c| self.caps.is_enabled(c))
    }

    /// The history page (a chathistory page or ZNC's playback) batch `r`
    /// is part of: the nearest tracked one around it, else `r` itself when
    /// it is a page the tracker had no room for.
    fn page_of(&self, r: &str) -> Option<String> {
        self.within(r, history::BATCH, &self.pages).or_else(|| self.within(r, playback::BATCH, &self.pages))
    }

    /// The batch of type `kind` held in `held` that `r` is part of: the
    /// nearest tracked one around it, else `r` itself when the tracker had
    /// no room for it.
    fn within<V>(&self, r: &str, kind: &str, held: &HashMap<String, V>) -> Option<String> {
        self.batches.enclosing(r, kind).filter(|b| held.contains_key(b)).or_else(|| held.contains_key(r).then(|| r.to_owned()))
    }

    /// The label batch `r` (opened by `msg`) answers: from the tracker,
    /// else the batch line's own tag (when the tracker had no room).
    fn batch_label(&self, r: &str, msg: &Message) -> Option<String> {
        self.batches.label(r).or_else(|| msg.tag("label").filter(|l| !l.is_empty()).map(str::to_owned))
    }

    /// The in-flight request a page with `label` for `target` answers. With
    /// labels on, only a page with our label answers one; anything else
    /// (a bouncer's replay, someone else's label) is unsolicited.
    fn answers(&self, label: Option<&str>, target: &str) -> Option<String> {
        let key = match label {
            Some(l) => self.labels.get(l).cloned(),
            None if !self.caps.is_enabled("labeled-response") => Some(target.to_ascii_lowercase()),
            None => None,
        }?;
        self.inflight.contains_key(&key).then_some(key)
    }

    /// Ask for a page, unless one is out for `target` already. Labeled,
    /// when the server labels its answers, so each page finds its request.
    fn request(&mut self, target: &str, kind: Kind, out: &mut Vec<Output>) {
        let key = target.to_ascii_lowercase();
        // One out already: this one goes once that is done (`settled`),
        // as asked, and opens its gap only then.
        if self.inflight.contains_key(&key) {
            self.queued.entry(key).or_insert_with(|| (target.to_owned(), kind));
            return;
        }
        // Until the gap is filled, the newest held stays where it is.
        if matches!(kind, Kind::After(_)) && !self.gaps.contains_key(&key) {
            self.gaps.insert(key.clone(), None);
            // Live lines from here are past the hole until it is filled.
            self.tail.remove(&key);
        }
        let request = Request { target: target.to_owned(), kind, limit: self.page() };
        let mut msg = request.message();
        if self.caps.is_enabled("labeled-response") {
            self.label_seq += 1;
            let label = format!("hl{}", self.label_seq);
            msg.tags.push(("label".into(), Some(label.clone())));
            self.labels.insert(label, key.clone());
        }
        self.inflight.insert(key, Inflight { request, retried: false });
        out.push(Output::Send(msg));
    }

    /// What `target` missed: everything after the newest line held, else
    /// the latest page.
    fn backfill(&mut self, target: &str, out: &mut Vec<Output>) {
        if !self.history_on() {
            out.push(self.filled(target));
            return;
        }
        let kind = self.book.newest(target).map_or(Kind::Latest, |a| Kind::After(a.clone()));
        self.request(target, kind, out);
    }

    /// The page before the oldest line held. Without history the window
    /// is told at once that nothing older will come; after giving up, that
    /// nothing comes now. Before registration ends (the server's page
    /// limit, and every gap fill, come first), or with a request out, it
    /// waits. Once the server's history ended, the cache has been read
    /// already (`conn::older`): this is the start.
    fn older(&mut self, target: &str, out: &mut Vec<Output>) {
        if !self.history_on() {
            out.push(self.history_event(target, Vec::new(), true));
            return;
        }
        let key = target.to_ascii_lowercase();
        if !self.synced {
            self.queued_older.insert(key, target.to_owned());
            return;
        }
        if self.ended.contains(&key) {
            out.push(self.history_event(target, Vec::new(), true));
            return;
        }
        if self.gave_up.contains(&key) {
            out.push(self.history_event(target, Vec::new(), false));
            return;
        }
        if self.inflight.contains_key(&key) {
            self.queued_older.insert(key, target.to_owned());
            return;
        }
        let kind = self.book.oldest(target).map_or(Kind::Latest, |a| Kind::Before(a.clone()));
        self.request(target, kind, out);
    }

    /// A window for `target` is open: keep it in the book, and bring a
    /// query up to date now if registered (later, `sync` does).
    fn open(&mut self, target: &str, out: &mut Vec<Output>) {
        let query = !self.isupport.is_channel(target);
        self.book.open(target, query);
        if query && self.synced {
            self.sync_query(target, out);
        }
    }

    /// A query's history since we last had it, and where it was read up
    /// to (the server sends a channel's marker on JOIN; a query's is asked).
    /// Callers keep services queries the user did not open out of here.
    fn sync_query(&mut self, nick: &str, out: &mut Vec<Output>) {
        if self.caps.is_enabled(READ_MARKER) {
            out.push(send("MARKREAD", [nick]));
        }
        self.backfill(nick, out);
    }

    /// Registered, with ISUPPORT known: bring every open query up to date,
    /// then ask who wrote to us while we were away. Channels catch up as
    /// they are joined.
    /// Services queries are the user's to open: never backfilled on
    /// their own.
    fn sync(&mut self, now: DateTime<Utc>, out: &mut Vec<Output>) {
        for nick in self.book.queries().into_iter().filter(|n| !services::is_service(n)) {
            self.sync_query(&nick, out);
        }
        // Gaps opened at start that nothing is filling (history is off):
        // the lines that waited are held now.
        let idle: Vec<String> = self.gaps.keys().filter(|k| !self.inflight.contains_key(*k)).cloned().collect();
        for key in idle {
            let name = self.book.get(&key).map_or_else(|| key.clone(), |k| k.name.clone());
            self.close_gap(&key, &name);
        }
        self.discover(now, out);
        // `Older` asked before now: after the gap fills it waited for.
        let waiting: Vec<String> = self.queued_older.keys().filter(|k| !self.inflight.contains_key(*k)).cloned().collect();
        for key in waiting {
            if let Some(target) = self.queued_older.remove(&key) {
                self.older(&target, out);
            }
        }
        self.play_back(now, out);
    }

    /// `CHATHISTORY TARGETS` to a little past now (the server's clock may
    /// be ahead of ours), from the earlier of where discovery got to and
    /// the newest line held, else a week back. Where it got to only moves
    /// once an answer is in full, so a TARGETS cut off (while channels
    /// caught up past it) is asked again from the same point.
    fn discover(&mut self, now: DateTime<Utc>, out: &mut Vec<Output>) {
        if !self.history_on() {
            return;
        }
        let from = self.book.discovered().into_iter().chain(self.book.last_seen()).min().unwrap_or(now - TimeDelta::days(history::DISCOVERY_DAYS));
        // Asked from here, so found from here on: until an answer is in
        // full, the next connection asks from here too.
        if self.book.discovered().is_none() {
            self.book.set_discovered(from);
        }
        self.discovering = Some(now);
        self.targets_pages = 1;
        self.request(history::TARGETS, Kind::Targets { from, to: now + TimeDelta::hours(1) }, out);
    }

    /// The TARGETS answer: each name we may open is a query to open and
    /// bring up to date; its first page opens its window, at the end of
    /// the list. A full answer asks on from its last name's time (at most
    /// `MAX_TARGETS_PAGES` in all); a short one means all were found.
    fn finish_targets(&mut self, names: Vec<Found>, out: &mut Vec<Output>) {
        let Some(Inflight { request, .. }) = self.inflight.remove(history::TARGETS) else { return };
        self.labels.retain(|_, k| k != history::TARGETS);
        let full = names.len() >= request.limit;
        let last = names.last().and_then(|(_, at)| *at);
        for (name, _) in names {
            if self.auto_opens(&name) {
                self.book.open(&name, true);
                self.sync_query(&name, out);
            }
        }
        let Kind::Targets { from, to } = request.kind else { return };
        match last.filter(|at| full && *at > from) {
            Some(next) if self.targets_pages < history::MAX_TARGETS_PAGES => {
                self.targets_pages += 1;
                self.request(history::TARGETS, Kind::Targets { from: next, to }, out);
            }
            // Full but no further on, or out of requests: given up for
            // this connection, and the next asks from the same point.
            Some(_) => {}
            None if full => {}
            None => self.discovered_all(),
        }
    }

    /// Every TARGETS of this connection is answered in full.
    fn discovered_all(&mut self) {
        if let Some(at) = self.discovering {
            self.book.set_discovered(at);
        }
    }

    /// `PLAY` is how history comes: the user wants history, ZNC's playback
    /// cap is on, and the server has no chathistory (which wins).
    fn playback_on(&self) -> bool {
        self.cfg.chathistory && self.caps.is_enabled(playback::CAP) && !self.history_on()
    }

    /// One `PLAY` asks for every buffer's lines (queries too) since the
    /// newest line held anywhere on this network, or for all of them when
    /// nothing is held.
    fn play_back(&mut self, now: DateTime<Utc>, out: &mut Vec<Output>) {
        if !self.playback_on() {
            return;
        }
        let since = playback::since(self.book.last_seen());
        self.play = Some(DateTime::from_timestamp(since, 0).map_or(now, |t| t.max(now)));
        out.push(Output::Send(playback::play(since)));
    }

    /// ZNC's echo of our own `PLAY` (with echo-message), however ZNC
    /// spells it back: by what it is, not its exact text.
    fn play_echo(&self, msg: &Message) -> bool {
        self.play.is_some()
            && matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE")
            && msg.nick().is_some_and(|n| self.is_me(n))
            && msg.params.first().is_some_and(|t| t.eq_ignore_ascii_case(playback::MODULE))
            && msg.params.get(1).and_then(|t| t.get(..5)).is_some_and(|p| p.eq_ignore_ascii_case("PLAY "))
    }

    /// A services query the user hasn't opened. Our own lines there (an
    /// echo, or one sent from another client) open nothing: they can hold
    /// a password.
    fn unopened_service(&self, target: &Target) -> bool {
        matches!(target, Target::Query(n) if services::is_service(n) && !self.book.get(n).is_some_and(|k| k.query))
    }

    /// A message outside any history batch that is still ZNC's replay:
    /// without batches, one timed at or before our `PLAY`. With batches
    /// the whole replay is in them, so a loose line is news even when a
    /// clock ahead of the server's makes it look old.
    fn loose_replay(&self, msg: &Message) -> bool {
        matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE")
            && msg.tag("batch").is_none()
            && !self.caps.is_enabled("batch")
            && self.play.is_some_and(|until| server_time(msg).is_some_and(|t| t <= until))
    }

    /// One loose replayed message: shown as its live copy would be, but as
    /// a page of one replayed line. Nothing else the live one does (peers)
    /// happens. A server-buffer line passes through as it is.
    fn replay_loose(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let mut shown = Vec::new();
        self.on_message(msg, now, &mut shown);
        for o in shown {
            match o {
                Output::Event(Event::Line { target: Target::Server, .. }) => out.push(o),
                Output::Event(Event::Line { target, line, .. }) => {
                    let name = target.name().unwrap_or_default().to_owned();
                    self.finish_playback(&name, vec![Line { replayed: true, ..line }], out);
                }
                _ => {}
            }
        }
    }

    /// ZNC's replay of one buffer (or one line of it, without batches):
    /// history, as one page. Empty (ZNC sends one per buffer on attach),
    /// it shows nothing. A services query's replay opens no window: only
    /// one the user has open shows it, and it is never cached.
    ///
    /// The run is kept following the last line kept for its target on
    /// this connection, else as a break. The cache already holds the line
    /// a replay starts on when ZNC kept everything since we left (PLAY
    /// asks from that line's second). So only a replay that starts past
    /// what we hold leaves a break: ZNC's buffer ran out. Live lines then
    /// follow on from the replay.
    fn finish_playback(&mut self, target: &str, lines: Vec<Line>, out: &mut Vec<Output>) {
        let query = !self.isupport.is_channel(target);
        let unasked_service = query && services::is_service(target) && !self.book.get(target).is_some_and(|k| k.query);
        if target.is_empty() || lines.is_empty() || unasked_service {
            return;
        }
        self.book.note(target, query, &lines);
        let key = target.to_ascii_lowercase();
        let below = match self.tail.get(&key) {
            Some(Tail::At(prev) | Tail::Behind(prev)) => Below::Joins(prev.clone()),
            Some(Tail::Hole) | None => Below::Breaks,
        };
        self.keep_run(target, &lines, below, Above::Leave, false);
        // After a hole, the replay is from before it: the next live line
        // still doesn't follow on, but more of the replay does.
        let dest = self.dest(target);
        if let Some(id) = lines.iter().rev().find(|l| cache::cacheable(&dest, l)).and_then(|l| l.msgid.clone()) {
            let behind = matches!(self.tail.get(&key), Some(Tail::Hole | Tail::Behind(_)));
            self.tail.insert(key, if behind { Tail::Behind(id) } else { Tail::At(id) });
        }
        out.push(self.history_event(target, lines, false));
    }

    /// A name from a TARGETS answer that we may open a query for: a nick
    /// (not a channel, not us), well formed, not a service (its history
    /// can hold passwords), and not held already (`sync` did those).
    fn auto_opens(&self, name: &str) -> bool {
        !name.is_empty()
            && !name.chars().any(|c| c.is_whitespace() || c.is_control())
            && !self.isupport.is_channel(name)
            && !self.is_me(name)
            && !services::is_service(name)
            && !self.book.get(name).is_some_and(|k| k.query)
    }

    /// One message of history page `h`: shown as its live copy would be,
    /// but only its line goes in the page, flagged `replayed`. Nothing
    /// else the live one does (peers, members, the topic) happens.
    /// Past the page's cap an item is only counted.
    fn replay(&mut self, h: &str, msg: &Message, now: DateTime<Utc>) {
        let Some(page) = self.pages.get_mut(h) else { return };
        page.items += 1;
        if page.items > page.cap {
            return;
        }
        let target = page.target.clone();
        let mut shown = Vec::new();
        if matches!(msg.command.as_str(), "PRIVMSG" | "NOTICE") {
            self.on_message(msg, now, &mut shown);
        } else {
            self.replayed_event(&target, msg, now, &mut shown);
        }
        let anchor = Anchor::from_tags(msg);
        let Some(page) = self.pages.get_mut(h) else { return };
        if let Some(a) = anchor {
            page.first.get_or_insert_with(|| a.clone());
            page.last = Some(a);
        }
        page.lines.extend(shown.into_iter().filter_map(|o| match o {
            Output::Event(Event::Line { line, .. }) => Some(Line { replayed: true, ..line }),
            _ => None,
        }));
    }

    /// event-playback: a JOIN, PART, ... from history, as the line its
    /// live copy shows. Members, peers and the topic stay as they are now.
    fn replayed_event(&mut self, target: &str, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let nick = msg.nick().unwrap_or("").to_owned();
        let p = |i: usize| msg.params.get(i).map(String::as_str);
        let (kind, who, text, own) = match msg.command.as_str() {
            "JOIN" => (LineKind::Join, nick.clone(), join_text(&nick), self.is_me(&nick)),
            "PART" => (LineKind::Part, nick.clone(), part_text(&nick, p(1)), self.is_me(&nick)),
            "KICK" => {
                let victim = p(1).unwrap_or("").to_owned();
                let own = self.is_me(&victim);
                (LineKind::Kick, victim.clone(), kick_text(&victim, &nick, p(2)), own)
            }
            "QUIT" => (LineKind::Quit, nick.clone(), quit_text(&nick, p(0).unwrap_or("")), false),
            "NICK" => (LineKind::Nick, nick.clone(), nick_text(&nick, p(0).unwrap_or("")), self.is_me(&nick)),
            "TOPIC" => (LineKind::Topic, nick.clone(), topic_text(&nick, p(1).unwrap_or("")), self.is_me(&nick)),
            "MODE" => (LineKind::Mode, nick.clone(), mode_text(&nick, &msg.params.get(1..).unwrap_or_default().join(" ")), self.is_me(&nick)),
            _ => return,
        };
        let line = self.line(msg, now, kind, Some(&who), text, own);
        out.push(self.stamped(msg, self.dest(target), line));
    }

    /// A page is in (or `cut` off by the drop). It answers the request
    /// with our label (without labels, the one for its target); one nobody
    /// asked for is shown all the same. Empty, before the oldest, it is the
    /// start of history; full, after the newest, there is more to fetch;
    /// short, after the newest, the gap is closed.
    fn finish_page(&mut self, page: Page, cut: bool, out: &mut Vec<Output>) {
        if page.playback {
            return self.finish_playback(&page.target, page.lines, out);
        }
        // Items past the cap were dropped, next to the anchor or the live
        // end, so the page can't be said to join either. Cut off by the
        // drop, a page after still holds the part next to its anchor.
        let over = page.items > page.cap;
        let partial = cut || over;
        let answered = self.answers(page.label.as_deref(), &page.target).and_then(|k| self.inflight.remove(&k).map(|i| (k, i)));
        let Some((key, Inflight { request, .. })) = answered else {
            self.widen_unsolicited(&page);
            self.keep_run(&page.target, &page.lines, Below::Breaks, Above::Breaks, false);
            if !page.lines.is_empty() {
                out.push(self.history_event(&page.target, page.lines, false));
            }
            return;
        };
        self.labels.retain(|_, k| *k != key);
        // Only the end the request moves: a page before can tie in time
        // with the newest held, and must not pull it back. Cut off, a page
        // after still got the part next to what is held; any other got its
        // far end only.
        let (first, last) = match (&request.kind, cut) {
            (Kind::After(_), _) => (None, page.last.as_ref()),
            (_, true) => (None, None),
            (Kind::Before(_), false) => (page.first.as_ref(), None),
            (Kind::Latest | Kind::Targets { .. }, false) => (page.first.as_ref(), page.last.as_ref()),
        };
        if first.is_some() || last.is_some() {
            let query = !self.isupport.is_channel(&request.target);
            self.book.reach(&request.target, query, first, last);
        }
        // How it meets what the cache holds: as it meets what is held here.
        let (below, above) = match (&request.kind, partial) {
            (Kind::After(a), _) if !over => (a.msgid.clone().map_or(Below::Breaks, Below::Joins), Above::Breaks),
            (Kind::Before(a), false) => (Below::Breaks, a.msgid.clone().map_or(Above::Breaks, Above::Joins)),
            _ => (Below::Breaks, Above::Breaks),
        };
        let before = matches!(request.kind, Kind::Before(_));
        self.keep_run(&request.target, &page.lines, below, above, before);
        let newest_kept = {
            let dest = self.dest(&request.target);
            page.lines.iter().rev().find(|l| cache::cacheable(&dest, l)).and_then(|l| l.msgid.clone())
        };
        if cut {
            if !page.lines.is_empty() {
                out.push(self.page_event(&request.target, page.lines, false, before));
            }
            return;
        }
        let start = page.items == 0 && matches!(request.kind, Kind::Latest | Kind::Before(_));
        let full = page.items >= request.limit;
        // Short before: the server's history ends here, the cache's may not.
        let ended = matches!(request.kind, Kind::Before(_)) && !full && self.keep.is_some();
        if ended {
            self.ended.insert(key.clone());
        }
        if start && ended {
            out.push(Output::CacheOlder(request.target.clone()));
        } else {
            out.push(self.page_event(&request.target, page.lines, start, before));
        }
        // The page reaches the live end: live lines follow on from it.
        if matches!(request.kind, Kind::Latest) {
            if !partial {
                self.tail_unless_known(&request.target, newest_kept.clone());
            }
            out.push(self.filled(&request.target));
        }
        if let Kind::After(from) = &request.kind {
            match page.last.filter(|l| full && l != from) {
                Some(last) => self.request(&request.target, Kind::After(last), out),
                None if !full => {
                    if !partial {
                        self.tail_unless_known(&request.target, newest_kept.or_else(|| from.msgid.clone()));
                    }
                    self.close_gap(&key, &request.target);
                    out.push(self.filled(&request.target));
                }
                // Full but no further on: the gap stays open for next time,
                // and this connection has all it will get.
                None => out.push(self.filled(&request.target)),
            }
        }
        self.settled(&key, out);
    }

    /// A page nobody asked for widens what is held only where it touches
    /// it (or where nothing is held), never the newest while a request or
    /// a gap is open: anything else would bridge lines never fetched.
    fn widen_unsolicited(&mut self, page: &Page) {
        let (Some(first), Some(last)) = (&page.first, &page.last) else { return };
        let key = page.target.to_ascii_lowercase();
        let held = self.book.get(&page.target);
        let (newest, oldest) = (held.and_then(|k| k.newest.as_ref()), held.and_then(|k| k.oldest.as_ref()));
        let busy = self.inflight.contains_key(&key) || self.gaps.contains_key(&key);
        let to_newest = !busy && newest.is_none_or(|n| first.time <= n.time);
        let to_oldest = oldest.is_none_or(|o| last.time >= o.time);
        if to_newest || to_oldest {
            let query = !self.isupport.is_channel(&page.target);
            self.book.reach(&page.target, query, to_oldest.then_some(first), to_newest.then_some(last));
        }
    }

    /// The gap is filled: the live lines seen meanwhile are held now.
    fn close_gap(&mut self, key: &str, target: &str) {
        if let Some(Some(live)) = self.gaps.remove(key) {
            let query = !self.isupport.is_channel(target);
            self.book.reach(target, query, None, Some(&live));
        }
    }

    /// The request for `key` is done: send the request that met it, then
    /// an `Older` that waited for it (behind that one, if it went).
    fn settled(&mut self, key: &str, out: &mut Vec<Output>) {
        if self.inflight.contains_key(key) {
            return;
        }
        if let Some((target, kind)) = self.queued.remove(key) {
            self.request(&target, kind, out);
        }
        if let Some(target) = self.queued_older.remove(key) {
            self.older(&target, out);
        }
    }

    /// `target`'s history for this connection is in: its latest page or
    /// gap fill answered, given up on, or history can't come.
    fn filled(&self, target: &str) -> Output {
        Output::Event(Event::Filled { network: self.network.clone(), target: self.dest(target) })
    }

    /// A labeled request answered with a bare ACK: nothing there.
    fn on_ack(&mut self, msg: &Message, out: &mut Vec<Output>) {
        let Some(key) = msg.tag("label").and_then(|l| self.labels.remove(l)) else { return };
        let Some(Inflight { request, .. }) = self.inflight.remove(&key) else { return };
        // TARGETS with no names: nobody wrote, so all were found.
        if key == history::TARGETS {
            self.discovered_all();
            return;
        }
        let start = matches!(request.kind, Kind::Latest | Kind::Before(_));
        if matches!(request.kind, Kind::Before(_)) && self.keep.is_some() {
            self.ended.insert(key.clone());
            out.push(Output::CacheOlder(request.target.clone()));
        } else {
            out.push(self.history_event(&request.target, Vec::new(), start));
        }
        if let Kind::After(from) = &request.kind {
            self.tail_unless_known(&request.target, from.msgid.clone());
            self.close_gap(&key, &request.target);
        }
        if matches!(request.kind, Kind::Latest | Kind::After(_)) {
            out.push(self.filled(&request.target));
        }
        self.settled(&key, out);
    }

    /// `FAIL CHATHISTORY <code> <subcommand> <target> :why`. A msgid the
    /// server can't find is asked for again by its time, once; after that
    /// the window stops waiting, and the server buffer says why.
    fn on_fail(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let by_label = msg.tag("label").and_then(|l| self.labels.remove(l));
        let key = by_label.or_else(|| msg.params.get(3).map(|t| t.to_ascii_lowercase()).filter(|k| self.inflight.contains_key(k)));
        let Some(key) = key else { return };
        let Some(Inflight { request, retried }) = self.inflight.remove(&key) else { return };
        self.labels.retain(|_, k| *k != key);
        let again = match &request.kind {
            Kind::Before(a) if a.msgid.is_some() && !retried => Some(Kind::Before(a.by_time())),
            Kind::After(a) if a.msgid.is_some() && !retried => Some(Kind::After(a.by_time())),
            _ => None,
        };
        if let Some(kind) = again {
            self.request(&request.target, kind, out);
            if let Some(i) = self.inflight.get_mut(&key) {
                i.retried = true;
            }
            return;
        }
        let text = format!("Can't get history for {}: {}", request.target, last(msg));
        let line = self.line(msg, now, LineKind::Error, None, text, false);
        out.push(self.event_line(Target::Server, line));
        if key == history::TARGETS {
            return;
        }
        // A gap given up on stays open: the next connection asks again.
        if matches!(request.kind, Kind::Before(_)) {
            self.gave_up.insert(key.clone());
        }
        out.push(self.history_event(&request.target, Vec::new(), false));
        if matches!(request.kind, Kind::Latest | Kind::After(_)) {
            out.push(self.filled(&request.target));
        }
        self.settled(&key, out);
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
        Line { seq: self.seq, time, msgid, kind, nick: nick.map(str::to_owned), text, own, delivery: Delivery::Sent, boundary: false, replayed: false }
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

    /// `line`, said by `msg` in `target`, as an event, with a msgid made up
    /// from what it says (`msgid::made_up`) when that is the only way its
    /// live and replayed copies get one key, so they are one line and it
    /// can be cached:
    /// - on a playback network (ZNC), always, the server's msgid replaced:
    ///   ZNC passes it on live lines but replays without it (ruling Q1);
    /// - else without chathistory, on a line the server gave none.
    ///
    /// Never in the server buffer, and never without server-time: our
    /// clock differs between copies.
    fn stamped(&self, msg: &Message, target: Target, mut line: Line) -> Output {
        let wanted = self.playback_on() || (line.msgid.is_none() && !self.history_on());
        if wanted && !matches!(target, Target::Server) && server_time(msg).is_some() {
            line.msgid = Some(crate::msgid::made_up(&target, &line));
        }
        self.event_line(target, line)
    }

    fn is_me(&self, nick: &str) -> bool {
        nick.eq_ignore_ascii_case(&self.nick)
    }

    /// A channel or a nick, as a buffer target.
    fn dest(&self, name: &str) -> Target {
        if self.isupport.is_channel(name) { Target::Channel(name.to_owned()) } else { Target::Query(name.to_owned()) }
    }

    fn history_event(&self, target: &str, lines: Vec<Line>, start: bool) -> Output {
        self.page_event(target, lines, start, false)
    }

    /// A page from before every line shown (`Event::History::older`).
    fn page_event(&self, target: &str, lines: Vec<Line>, start: bool, older: bool) -> Output {
        Output::Event(Event::History { network: self.network.clone(), target: self.dest(target), lines, start, older })
    }

    /// Every live line shown widens the book, so the next request starts
    /// past what is held; while its target's gap is filling, it waits for
    /// the gap to close. History moves the book in `finish_page`, which
    /// knows which end a page is next to.
    fn note(&mut self, out: &[Output]) {
        for o in out {
            let Output::Event(Event::Line { target, line, .. }) = o else { continue };
            let Some(name) = target.name() else { continue };
            if line.own && self.unopened_service(target) {
                continue;
            }
            let query = matches!(target, Target::Query(_));
            match self.gaps.get_mut(&name.to_ascii_lowercase()) {
                Some(pending) => {
                    self.book.open(name, query);
                    if line.msgid.is_some() && pending.as_ref().is_none_or(|p| line.time >= p.time) {
                        *pending = Some(Anchor::of(line));
                    }
                }
                None => self.book.note(name, query, std::slice::from_ref(line)),
            }
            self.keep_live(target, line);
        }
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
        if own && self.unopened_service(&target) {
            return;
        }
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
        out.push(self.stamped(msg, target, line));
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
        let me = self.is_me(&nick);
        let line = self.line(msg, now, LineKind::Join, Some(&nick), join_text(&nick), me);
        out.push(self.stamped(msg, Target::Channel(chan.clone()), line));
        if me {
            self.book.open(&chan, false);
            self.backfill(&chan, out);
        }
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
        let text = part_text(&nick, msg.params.get(1).map(String::as_str));
        let line = self.line(msg, now, LineKind::Part, Some(&nick), text, self.is_me(&nick));
        out.push(self.stamped(msg, Target::Channel(chan.clone()), line));
        self.left(&chan, &nick, out);
    }

    fn on_kick(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let [chan, victim, rest @ ..] = msg.params.as_slice() else { return };
        let (chan, victim) = (chan.clone(), victim.clone());
        let by = msg.nick().unwrap_or("").to_owned();
        let text = kick_text(&victim, &by, rest.first().map(String::as_str));
        // Worded from the one who leaves, as a part or a quit is, so the
        // `<--` always points at the first nick.
        let line = self.line(msg, now, LineKind::Kick, Some(&victim), text, self.is_me(&victim));
        out.push(self.stamped(msg, Target::Channel(chan.clone()), line));
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
        let text = quit_text(&nick, &reason);
        if !self.is_me(&nick) {
            out.push(self.peer(&nick, PeerChange::Quit(reason)));
        }
        for chan in self.channels_with(&nick) {
            self.members_of(&chan).remove(&nick.to_ascii_lowercase());
            let line = self.line(msg, now, LineKind::Quit, Some(&nick), text.clone(), false);
            out.push(self.stamped(msg, Target::Channel(chan), line));
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
            let line = self.line(msg, now, LineKind::Nick, Some(&old), nick_text(&old, &new), own);
            out.push(self.stamped(msg, Target::Channel(chan), line));
        }
    }

    fn on_topic(&mut self, msg: &Message, now: DateTime<Utc>, out: &mut Vec<Output>) {
        let [chan, topic, ..] = msg.params.as_slice() else { return };
        let (chan, topic) = (chan.clone(), topic.clone());
        let by = msg.nick().unwrap_or("").to_owned();
        out.push(Output::Event(Event::Topic { network: self.network.clone(), channel: chan.clone(), topic: Some(topic.clone()).filter(|t| !t.is_empty()) }));
        let line = self.line(msg, now, LineKind::Topic, Some(&by), topic_text(&by, &topic), self.is_me(&by));
        out.push(self.stamped(msg, Target::Channel(chan), line));
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
        let line = self.line(msg, now, LineKind::Mode, Some(&by), mode_text(&by, &modes), own);
        out.push(self.stamped(msg, dest, line));
    }

    pub fn command(&mut self, cmd: Command, now: DateTime<Utc>) -> Vec<Output> {
        // A raw line is the user's own: whatever it says goes.
        let raw = matches!(cmd, Command::Raw { .. });
        let out = self.run(cmd, now);
        // Our own lines, too: one without a msgid is a hole in the cache.
        self.note(&out);
        // Never with `out` in the message: it can hold AUTHENTICATE.
        debug_assert!(raw || safe_to_send(&out), "a made-up msgid was about to go to the server");
        out
    }

    fn run(&mut self, cmd: Command, now: DateTime<Utc>) -> Vec<Output> {
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
            Command::Older { target, .. } => {
                let mut out = Vec::new();
                self.older(&target, &mut out);
                out
            }
            Command::Open { target, .. } => {
                let mut out = Vec::new();
                self.open(&target, &mut out);
                out
            }
            Command::MarkRead { target, time, .. } => {
                if self.caps.is_enabled(READ_MARKER) {
                    vec![send("MARKREAD", [target, history::timestamp(time)])]
                } else {
                    Vec::new()
                }
            }
            Command::Reconnect { .. } => Vec::new(),
        }
    }

    fn local(&mut self, kind: LineKind, text: String, now: DateTime<Utc>) -> Line {
        self.seq += 1;
        Line { seq: self.seq, time: now, msgid: None, kind, nick: None, text, own: true, delivery: Delivery::Sent, boundary: false, replayed: false }
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
        // Writing to a services query is the user opening it: its echo
        // (and later lines of ours there) show in it.
        if self.unopened_service(&dest) {
            self.book.open(target, true);
        }
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
pub(crate) mod tests {
    use super::*;
    use crate::history::Book;

    pub(crate) fn now() -> DateTime<Utc> {
        "2026-10-22T12:00:00Z".parse().unwrap()
    }

    pub(crate) fn cfg() -> SessionConfig {
        SessionConfig {
            nickname: "alice".into(),
            alt_nicks: vec!["alice2".into()],
            username: "alice".into(),
            realname: "Alice".into(),
            password: None,
            sasl: None,
            channels: vec!["#test".into()],
            bridges: vec![],
            chathistory: true,
            history_page: None,
        }
    }

    pub(crate) fn feed(s: &mut Session, line: &str) -> Vec<Output> {
        s.handle(Message::parse(line).unwrap(), now())
    }

    pub(crate) fn sent(out: &[Output]) -> Vec<String> {
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
        assert_eq!(sent(&feed(&mut s, ":srv CAP * LS :ergo.chat/nope")), ["CAP END"]);
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
    fn every_batch_is_tracked_with_its_nesting() {
        let mut s = multiline("max-bytes=4096");
        feed(&mut s, "@label=hl1 :srv BATCH +lr labeled-response");
        feed(&mut s, "@batch=lr :srv BATCH +h chathistory #test");
        assert_eq!(s.batches.enclosing("h", "chathistory").as_deref(), Some("h"));
        assert_eq!(s.batches.label("h").as_deref(), Some("hl1"));
        feed(&mut s, ":srv BATCH -h");
        assert_eq!(s.batches.get("h"), None);
        assert!(s.batches.get("lr").is_some());
        // A new connection forgets them all.
        s.start();
        assert_eq!(s.batches.get("lr"), None);
    }

    /// Registered with history on: batch, labeled-response and chathistory
    /// (plus `extra` caps), a page of 20, the server's `CHATHISTORY=limit`,
    /// and `book` carried in. What registration sends is dropped.
    pub(crate) fn historied(extra: &str, limit: usize, book: Book) -> Session {
        let mut s = Session::new("net", SessionConfig { history_page: Some(20), ..cfg() }).with_book(book);
        s.start();
        let ls = format!("message-tags server-time echo-message batch labeled-response draft/chathistory {extra}");
        let ack: Vec<&str> = ls.split_whitespace().map(|c| c.split_once('=').map_or(c, |(k, _)| k)).collect();
        feed(&mut s, &format!(":srv CAP * LS :{}", ls.trim()));
        feed(&mut s, &format!(":srv CAP alice ACK :{}", ack.join(" ")));
        feed(&mut s, ":srv 001 alice :hi");
        feed(&mut s, &format!(":srv 005 alice CHANTYPES=# CHATHISTORY={limit} :are supported"));
        // End of MOTD asks for TARGETS: nobody wrote, so each test starts
        // with nothing in flight.
        let out = feed(&mut s, ":srv 376 alice :End of MOTD");
        let targets = out.iter().find_map(|o| match o {
            Output::Send(m) if m.command == "CHATHISTORY" && m.params.first().map(String::as_str) == Some("TARGETS") => m.tag("label").map(str::to_owned),
            _ => None,
        });
        if let Some(label) = targets {
            feed(&mut s, &format!("@label={label} :srv ACK"));
        }
        s
    }

    /// The one CHATHISTORY line in `out`, without its label, and the label.
    pub(crate) fn asked(out: &[Output]) -> (String, String) {
        let ms: Vec<&Message> = out
            .iter()
            .filter_map(|o| match o {
                Output::Send(m) if m.command == "CHATHISTORY" => Some(m),
                _ => None,
            })
            .collect();
        let [m] = ms.as_slice() else { panic!("want one CHATHISTORY in {out:?}") };
        let label = m.tag("label").unwrap_or_default().to_owned();
        let bare = Message { tags: m.tags.iter().filter(|(k, _)| k != "label").cloned().collect(), ..(*m).clone() };
        (bare.to_wire(), label)
    }

    pub(crate) fn histories(out: &[Output]) -> Vec<(Target, Vec<Line>, bool)> {
        out.iter()
            .filter_map(|o| match o {
                Output::Event(Event::History { target, lines, start, .. }) => Some((target.clone(), lines.clone(), *start)),
                _ => None,
            })
            .collect()
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    fn older(s: &mut Session, target: &str) -> Vec<Output> {
        s.command(Command::Older { network: "net".into(), target: target.into() }, now())
    }

    #[test]
    fn our_join_asks_for_the_latest_page_with_a_label() {
        let mut s = historied("", 50, Book::default());
        let out = feed(&mut s, ":alice!a@h JOIN #test");
        let (wire, label) = asked(&out);
        assert_eq!(wire, "CHATHISTORY LATEST #test * 20");
        assert!(label.starts_with("hl"), "{label:?}");
        // The page is the server's limit when that is smaller.
        let mut s = historied("", 5, Book::default());
        assert_eq!(asked(&feed(&mut s, ":alice!a@h JOIN #test")).0, "CHATHISTORY LATEST #test * 5");
    }

    #[test]
    fn a_history_batch_is_one_page_of_replayed_lines() {
        let mut s = historied("", 50, Book::default());
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        feed(&mut s, &format!("@label={label} :srv BATCH +h chathistory #test"));
        for (i, text) in ["one", "two"].iter().enumerate() {
            let out = feed(&mut s, &format!("@batch=h;time=2026-10-22T10:00:0{i}.000Z;msgid=m{i} :bob!b@h PRIVMSG #test :{text}"));
            assert!(out.is_empty(), "history is not news: {out:?}");
        }
        let got = histories(&feed(&mut s, ":srv BATCH -h"));
        let [(target, lines, start)] = got.as_slice() else { panic!("{got:?}") };
        assert_eq!((target, *start), (&Target::Channel("#test".into()), false));
        assert_eq!(texts(lines), ["one", "two"]);
        assert!(lines.iter().all(|l| l.replayed));
        // Scrolling up asks for what is before the oldest.
        assert_eq!(asked(&older(&mut s, "#test")).0, "CHATHISTORY BEFORE #test msgid=m0 20");
    }

    #[test]
    fn pages_in_one_millisecond_still_walk_back() {
        let mut s = historied("", 2, Book::default());
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        let page = |s: &mut Session, label: &str, r: &str, ids: [&str; 2]| {
            feed(s, &format!("@label={label} :srv BATCH +{r} chathistory #test"));
            for id in ids {
                feed(s, &format!("@batch={r};time=2026-10-22T10:00:00.000Z;msgid={id} :bob!b@h PRIVMSG #test :{id}"));
            }
            feed(s, &format!(":srv BATCH -{r}"));
        };
        page(&mut s, &label, "a", ["n2", "n3"]);
        let (wire, label) = asked(&older(&mut s, "#test"));
        assert_eq!(wire, "CHATHISTORY BEFORE #test msgid=n2 2");
        page(&mut s, &label, "b", ["n0", "n1"]);
        // Same time as everything held: still the page before n0, not n2 again.
        assert_eq!(asked(&older(&mut s, "#test")).0, "CHATHISTORY BEFORE #test msgid=n0 2");
        assert_eq!(s.book().newest("#test").and_then(|a| a.msgid.as_deref()), Some("n3"));
    }

    #[test]
    fn one_request_at_a_time_per_target() {
        let mut s = historied("", 50, Book::default());
        feed(&mut s, ":alice!a@h JOIN #test");
        assert!(sent(&older(&mut s, "#TEST")).is_empty());
    }

    #[test]
    fn a_reconnect_fills_the_gap_page_by_page() {
        let mut s = historied("", 2, Book::default());
        feed(&mut s, "@time=2026-10-22T10:00:00.000Z;msgid=m0 :bob!b@h PRIVMSG #test :before the drop");
        let book = s.take_book();
        // The next connection carries the book, and asks only for what is newer.
        let mut s = historied("", 2, book);
        let (wire, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        assert_eq!(wire, "CHATHISTORY AFTER #test msgid=m0 2");
        feed(&mut s, &format!("@label={label} :srv BATCH +h chathistory #test"));
        feed(&mut s, "@batch=h;time=2026-10-22T10:01:00.000Z;msgid=m1 :bob!b@h PRIVMSG #test :gap one");
        feed(&mut s, "@batch=h;time=2026-10-22T10:02:00.000Z;msgid=m2 :bob!b@h PRIVMSG #test :gap two");
        // A live line lands while the gap is filling: the next page still
        // starts where this one ended, not after the live line.
        feed(&mut s, "@time=2026-10-22T11:00:00.000Z;msgid=m9 :bob!b@h PRIVMSG #test :live");
        let out = feed(&mut s, ":srv BATCH -h");
        assert_eq!(texts(&histories(&out)[0].1), ["gap one", "gap two"]);
        // A full page: there may be more.
        let (wire, label) = asked(&out);
        assert_eq!(wire, "CHATHISTORY AFTER #test msgid=m2 2");
        feed(&mut s, &format!("@label={label} :srv BATCH +h2 chathistory #test"));
        feed(&mut s, "@batch=h2;time=2026-10-22T10:03:00.000Z;msgid=m3 :bob!b@h PRIVMSG #test :gap three");
        let out = feed(&mut s, ":srv BATCH -h2");
        assert_eq!(texts(&histories(&out)[0].1), ["gap three"]);
        // A short page: the gap is closed.
        assert!(sent(&out).is_empty(), "{out:?}");
    }

    #[test]
    fn an_empty_page_before_the_oldest_is_the_start() {
        let mut s = historied("", 50, Book::default());
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        // A labeled ACK is an empty answer.
        let got = histories(&feed(&mut s, &format!("@label={label} :srv ACK")));
        assert_eq!(got, [(Target::Channel("#test".into()), vec![], true)]);
        // So is an empty batch.
        let (_, label) = asked(&older(&mut s, "#test"));
        feed(&mut s, &format!("@label={label} :srv BATCH +e chathistory #test"));
        assert_eq!(histories(&feed(&mut s, ":srv BATCH -e")), [(Target::Channel("#test".into()), vec![], true)]);
    }

    #[test]
    fn a_msgid_the_server_cannot_find_is_asked_again_by_time_once() {
        let mut book = Book::default();
        book.note("#test", false, &[Line { msgid: Some("gone".into()), time: "2026-10-22T10:00:00Z".parse().unwrap(), ..local_line() }]);
        let mut s = historied("", 50, book);
        let (wire, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        assert_eq!(wire, "CHATHISTORY AFTER #test msgid=gone 20");
        let out = feed(&mut s, &format!("@label={label} :srv FAIL CHATHISTORY MESSAGE_ERROR AFTER #test :Unknown msgid"));
        let (wire, label) = asked(&out);
        assert_eq!(wire, "CHATHISTORY AFTER #test timestamp=2026-10-22T10:00:00.000Z 20");
        let out = feed(&mut s, &format!("@label={label} :srv FAIL CHATHISTORY MESSAGE_ERROR AFTER #test :Still no"));
        // Given up: an empty page (not the start), a note, nothing sent.
        assert_eq!(histories(&out), [(Target::Channel("#test".into()), vec![], false)]);
        assert!(lines(&out).iter().any(|(t, l)| *t == Target::Server && l.text.contains("Still no")), "{out:?}");
        assert!(sent(&out).is_empty());
    }

    #[test]
    fn history_off_asks_for_nothing_and_says_so_at_once() {
        let mut s = Session::new("net", SessionConfig { chathistory: false, ..cfg() });
        s.start();
        feed(&mut s, ":srv CAP * LS :batch labeled-response draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :batch labeled-response draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        assert!(sent(&feed(&mut s, ":alice!a@h JOIN #test")).is_empty());
        // Nothing more will come, so the window says it is the start.
        assert_eq!(histories(&older(&mut s, "#test")), [(Target::Channel("#test".into()), vec![], true)]);
    }

    #[test]
    fn a_multiline_message_in_history_is_one_line() {
        let mut s = historied("draft/multiline=max-bytes=4096", 1, Book::default());
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        feed(&mut s, &format!("@label={label} :srv BATCH +h chathistory #test"));
        feed(&mut s, "@batch=h;time=2026-10-22T10:00:00.000Z;msgid=ml1 :bob!b@h BATCH +ml draft/multiline #test");
        feed(&mut s, "@batch=ml :bob!b@h PRIVMSG #test :first");
        feed(&mut s, "@batch=ml :bob!b@h PRIVMSG #test :second");
        assert!(feed(&mut s, ":srv BATCH -ml").is_empty());
        let got = histories(&feed(&mut s, ":srv BATCH -h"));
        let lines = &got[0].1;
        assert_eq!(texts(lines), ["first\nsecond"]);
        assert_eq!((lines[0].msgid.as_deref(), lines[0].replayed), (Some("ml1"), true));
    }

    #[test]
    fn a_multiline_message_past_the_batch_limit_is_still_history() {
        let mut s = historied("draft/multiline=max-bytes=4096", 50, Book::default());
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        feed(&mut s, &format!("@label={label} :srv BATCH +h chathistory #test"));
        for i in 1..crate::batch::MAX_OPEN {
            feed(&mut s, &format!(":srv BATCH +n{i} netsplit a b"));
        }
        // The tracker is full, so this one isn't tracked, but it is still
        // collected, and still part of the page around it.
        feed(&mut s, "@batch=h;time=2026-10-22T10:00:00.000Z;msgid=ml1 :bob!b@h BATCH +ml draft/multiline #test");
        assert_eq!(s.batches.get("ml"), None);
        assert!(feed(&mut s, "@batch=ml :bob!b@h PRIVMSG #test :first").is_empty());
        assert!(feed(&mut s, "@batch=ml :bob!b@h PRIVMSG #test :second").is_empty());
        assert!(feed(&mut s, ":srv BATCH -ml").is_empty());
        let got = histories(&feed(&mut s, ":srv BATCH -h"));
        assert_eq!(texts(&got[0].1), ["first\nsecond"]);
        // A history batch past the limit isn't a page: its lines are shown live.
        feed(&mut s, ":srv BATCH +h2 netsplit a b");
        // A history batch past the limit is still a page: replayed, never news.
        feed(&mut s, ":srv BATCH +h3 chathistory #test");
        assert_eq!(s.batches.get("h3"), None);
        assert!(feed(&mut s, "@batch=h3;time=2026-10-22T09:00:00.000Z;msgid=x :bob!b@h PRIVMSG #test :untracked").is_empty());
        let got = histories(&feed(&mut s, ":srv BATCH -h3"));
        assert_eq!(texts(&got[0].1), ["untracked"]);
        assert!(got[0].1[0].replayed);
    }

    /// A book holding `#test` up to `m0`, at 10:00.
    fn held() -> Book {
        let mut book = Book::default();
        book.note("#test", false, &[Line { msgid: Some("m0".into()), time: "2026-10-22T10:00:00Z".parse().unwrap(), ..local_line() }]);
        book
    }

    /// `ids` as one `chathistory` batch `r` for `#test`, a minute apart
    /// from 10:01, answering `label` (none: unlabeled).
    fn page(s: &mut Session, label: Option<&str>, r: &str, ids: &[&str]) -> Vec<Output> {
        let tag = label.map_or(String::new(), |l| format!("@label={l} "));
        feed(s, &format!("{tag}:srv BATCH +{r} chathistory #test"));
        for (i, id) in ids.iter().enumerate() {
            feed(s, &format!("@batch={r};time=2026-10-22T10:{:02}:00.000Z;msgid={id} :bob!b@h PRIVMSG #test :{id}", i + 1));
        }
        feed(s, &format!(":srv BATCH -{r}"))
    }

    fn newest(s: &Session) -> Option<String> {
        s.book().newest("#test").and_then(|a| a.msgid.clone())
    }

    const OUR_JOIN: &str = "@time=2026-10-22T11:00:00.000Z;msgid=j1 :alice!a@h JOIN #test";

    /// Registered with a query to bob held up to q0, with `line` (from bob)
    /// arriving before the end of the MOTD.
    fn query_line_before_motd(caps: &str, line: &str) -> (Session, Vec<Output>) {
        let mut book = Book::default();
        book.note("bob", true, &[Line { msgid: Some("q0".into()), time: "2026-10-22T10:00:00Z".parse().unwrap(), ..local_line() }]);
        let mut s = Session::new("net", cfg()).with_book(book);
        s.start();
        feed(&mut s, &format!(":srv CAP * LS :{caps}"));
        feed(&mut s, &format!(":srv CAP alice ACK :{caps}"));
        feed(&mut s, ":srv 001 alice :hi");
        feed(&mut s, line);
        let out = feed(&mut s, ":srv 376 alice :End of MOTD");
        (s, out)
    }

    #[test]
    fn a_query_line_before_registration_ends_waits_for_the_gap() {
        let line = "@time=2026-10-22T11:00:00.000Z;msgid=q1 :bob!b@h PRIVMSG alice :early";
        let (_, out) = query_line_before_motd("message-tags server-time batch draft/chathistory", line);
        let wanted: Vec<String> = sent(&out).into_iter().filter(|m| m.starts_with("CHATHISTORY AFTER")).collect();
        assert_eq!(wanted, ["CHATHISTORY AFTER bob msgid=q0 100"]);
        // Without history nothing fills it: the line is held at once.
        let (s, _) = query_line_before_motd("message-tags server-time batch", line);
        assert_eq!(s.book().newest("bob").and_then(|a| a.msgid.as_deref()), Some("q1"));
    }

    fn say_without_echo(s: &mut Session) {
        feed(s, ":srv CAP alice DEL :echo-message");
        s.command(Command::Say { network: "net".into(), target: "#test".into(), text: "hi".into() }, now());
    }

    /// The live line l1, at 11:05, and how the cache is told it meets
    /// what it holds.
    fn l1_kept(s: &mut Session) -> Below {
        feed(s, "@time=2026-10-22T11:05:00.000Z;msgid=l1 :bob!b@h PRIVMSG #test :l1");
        let last = s.kept().pop().expect("l1 kept");
        assert_eq!(last.lines[0].msgid.as_deref(), Some("l1"));
        last.below
    }

    #[test]
    fn a_hole_while_the_gap_fills_is_not_filled_in_by_the_page() {
        let mut s = historied("", 2, held()).keeping();
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        say_without_echo(&mut s);
        page(&mut s, Some(&label), "a", &["m1"]);
        assert_eq!(l1_kept(&mut s), Below::Breaks);
    }

    #[test]
    fn a_hole_while_the_latest_page_comes_is_not_filled_in_by_it() {
        let mut s = historied("", 2, Book::default()).keeping();
        let (_, label) = asked(&older(&mut s, "#test"));
        say_without_echo(&mut s);
        page(&mut s, Some(&label), "a", &["m1"]);
        assert_eq!(l1_kept(&mut s), Below::Breaks);
        // Without the hole, the page is the live end.
        let mut s = historied("", 2, Book::default()).keeping();
        let (_, label) = asked(&older(&mut s, "#test"));
        page(&mut s, Some(&label), "a", &["m1"]);
        assert_eq!(l1_kept(&mut s), Below::Joins("m1".into()));
    }

    #[test]
    fn a_page_past_its_cap_is_kept_as_cut_off() {
        // The latest page, three items for a limit of two: what was dropped
        // is next to the live end.
        let mut s = historied("", 2, Book::default()).keeping();
        let (_, label) = asked(&older(&mut s, "#test"));
        page(&mut s, Some(&label), "a", &["m1", "m2", "m3"]);
        assert_eq!(l1_kept(&mut s), Below::Breaks);
        // A page after, past its cap: it can't be said to join either.
        let mut s = historied("", 2, held()).keeping();
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        s.kept();
        page(&mut s, Some(&label), "a", &["m1", "m2", "m3"]);
        let runs: Vec<(Below, Above)> = s.kept().into_iter().map(|e| (e.below, e.above)).collect();
        assert_eq!(runs, [(Below::Breaks, Above::Breaks)]);
    }

    #[test]
    fn cached_lines_are_a_page_the_book_learns() {
        let mut s = Session::new("net", cfg());
        let line = Line { msgid: Some("c1".into()), replayed: true, ..local_line() };
        let out = s.cached("#test", vec![line.clone()]);
        assert_eq!(histories(&out), [(Target::Channel("#test".into()), vec![line], false)]);
        assert_eq!(s.book().oldest("#test").and_then(|a| a.msgid.as_deref()), Some("c1"));
    }

    #[test]
    fn cached_lines_move_only_the_oldest_and_leave_requests_alone() {
        let mut s = historied("", 2, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        // Stored before m0 in the same second: still older, so it is the oldest now.
        let tied = Line { msgid: Some("c0".into()), time: "2026-10-22T10:00:00Z".parse().unwrap(), replayed: true, ..local_line() };
        let out = s.cached("#test", vec![tied]);
        assert!(sent(&out).is_empty(), "{out:?}");
        assert_eq!(s.book().oldest("#test").and_then(|a| a.msgid.as_deref()), Some("c0"));
        assert_eq!(newest(&s).as_deref(), Some("m0"));
        // The gap's request is still the one out: its short page closes the
        // gap, and our join (live meanwhile) is the newest.
        page(&mut s, Some(&label), "a", &["m1"]);
        assert_eq!(newest(&s).as_deref(), Some("j1"));
        assert_eq!(asked(&older(&mut s, "#test")).0, "CHATHISTORY BEFORE #test msgid=c0 2");
    }

    #[test]
    fn a_gap_cut_off_mid_fill_is_asked_for_again() {
        let mut s = historied("", 2, held());
        let (wire, label) = asked(&feed(&mut s, OUR_JOIN));
        assert_eq!(wire, "CHATHISTORY AFTER #test msgid=m0 2");
        // A live line while the gap fills doesn't close it either.
        feed(&mut s, "@time=2026-10-22T11:01:00.000Z;msgid=live :bob!b@h PRIVMSG #test :live");
        assert_eq!(asked(&page(&mut s, Some(&label), "a", &["m1", "m2"])).0, "CHATHISTORY AFTER #test msgid=m2 2");
        // The drop: our join and the live line are past what is filled.
        s.end(now());
        let mut s = historied("", 2, s.take_book());
        assert_eq!(asked(&feed(&mut s, OUR_JOIN)).0, "CHATHISTORY AFTER #test msgid=m2 2");
    }

    #[test]
    fn a_gap_that_fails_stays_open_and_one_that_fills_takes_the_live_lines() {
        let mut s = historied("", 2, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        let (_, label) = asked(&feed(&mut s, &format!("@label={label} :srv FAIL CHATHISTORY MESSAGE_ERROR AFTER #test :no")));
        feed(&mut s, &format!("@label={label} :srv FAIL CHATHISTORY MESSAGE_ERROR AFTER #test :no"));
        assert_eq!(newest(&s).as_deref(), Some("m0"));
        // Filled: a short page closes the gap, and our join is the newest.
        let mut s = historied("", 2, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        assert!(sent(&page(&mut s, Some(&label), "a", &["m1"])).is_empty());
        assert_eq!(newest(&s).as_deref(), Some("j1"));
        // So is an empty answer.
        let mut s = historied("", 2, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        feed(&mut s, &format!("@label={label} :srv ACK"));
        assert_eq!(newest(&s).as_deref(), Some("j1"));
    }

    #[test]
    fn pages_without_our_label_answer_nothing() {
        let mut s = historied("", 5, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        // A bouncer's replay, and an answer to someone else's label: shown, but no answer.
        for (r, l) in [("u", None), ("f", Some("theirs"))] {
            let out = page(&mut s, l, r, &["x1", "x2", "x3", "x4", "x5"]);
            assert_eq!(texts(&histories(&out)[0].1), ["x1", "x2", "x3", "x4", "x5"]);
            assert!(sent(&out).is_empty(), "{out:?}");
            assert_eq!(newest(&s).as_deref(), Some("m0"));
        }
        // Ours still finds its request.
        let out = page(&mut s, Some(&label), "a", &["m1"]);
        assert_eq!(histories(&out)[0].0, Target::Channel("#test".into()));
        assert_eq!(newest(&s).as_deref(), Some("j1"));
    }

    #[test]
    fn a_page_nobody_asked_for_widens_only_what_it_touches() {
        let mut s = historied("", 5, Book::default());
        // Nothing held: it is what we hold.
        page(&mut s, None, "u", &["u1", "u2"]);
        assert_eq!(newest(&s).as_deref(), Some("u2"));
        // Starting past the newest: there may be a gap between, so no.
        feed(&mut s, "@batch=z :srv BATCH +z chathistory #test");
        feed(&mut s, "@batch=z;time=2026-10-22T10:30:00.000Z;msgid=far :bob!b@h PRIVMSG #test :far");
        feed(&mut s, ":srv BATCH -z");
        assert_eq!(newest(&s).as_deref(), Some("u2"));
        // Overlapping what is held: yes.
        page(&mut s, None, "o", &["u1", "u2", "u3"]);
        assert_eq!(newest(&s).as_deref(), Some("u3"));
    }

    #[test]
    fn a_page_that_ends_where_it_was_asked_from_is_not_followed() {
        let mut s = historied("", 1, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        feed(&mut s, &format!("@label={label} :srv BATCH +a chathistory #test"));
        feed(&mut s, "@batch=a;time=2026-10-22T10:00:00.000Z;msgid=m0 :bob!b@h PRIVMSG #test :m0");
        assert!(sent(&feed(&mut s, ":srv BATCH -a")).is_empty());
    }

    #[test]
    fn history_needs_server_time() {
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, ":srv CAP * LS :batch draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :batch draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        assert!(sent(&feed(&mut s, ":alice!a@h JOIN #test")).is_empty());
    }

    #[test]
    fn older_after_giving_up_asks_nothing() {
        let mut s = historied("", 5, held());
        let (_, label) = asked(&older(&mut s, "#test"));
        let (_, label) = asked(&feed(&mut s, &format!("@label={label} :srv FAIL CHATHISTORY MESSAGE_ERROR BEFORE #test :no")));
        feed(&mut s, &format!("@label={label} :srv FAIL CHATHISTORY MESSAGE_ERROR BEFORE #test :no"));
        for _ in 0..3 {
            let out = older(&mut s, "#test");
            assert!(sent(&out).is_empty());
            assert_eq!(histories(&out), [(Target::Channel("#test".into()), vec![], false)]);
            assert!(lines(&out).is_empty(), "no note again: {out:?}");
        }
        // A new connection tries again.
        s.start();
        assert!(s.inflight.is_empty() && s.gave_up.is_empty());
    }

    /// A query window on screen that fits asks `Older` as soon as the UI
    /// hears 001. It waits for the end of registration and the gap fill:
    /// the PMs that came while away still arrive, and the page before is
    /// held to the server's limit.
    #[test]
    fn older_at_001_waits_for_the_gap_fill() {
        let mut book = Book::default();
        book.note("bob", true, &[Line { msgid: Some("q0".into()), time: "2026-10-22T10:00:00Z".parse().unwrap(), ..local_line() }]);
        let mut s = Session::new("net", SessionConfig { history_page: Some(20), ..cfg() }).with_book(book);
        s.start();
        let caps = "message-tags server-time batch labeled-response draft/chathistory";
        feed(&mut s, &format!(":srv CAP * LS :{caps}"));
        feed(&mut s, &format!(":srv CAP alice ACK :{caps}"));
        feed(&mut s, ":srv 001 alice :hi");
        assert!(sent(&older(&mut s, "bob")).is_empty(), "nothing before the gap fill");
        feed(&mut s, ":srv 005 alice CHANTYPES=# CHATHISTORY=5 :are supported");
        let out = feed(&mut s, ":srv 376 alice :End of MOTD");
        let asks: Vec<String> = sent(&out).into_iter().filter(|m| m.contains("CHATHISTORY") && !m.contains("TARGETS")).collect();
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert!(asks[0].ends_with("CHATHISTORY AFTER bob msgid=q0 5"), "{asks:?}");
        let label = out
            .iter()
            .find_map(|o| match o {
                Output::Send(m) if m.params.first().map(String::as_str) == Some("AFTER") => m.tag("label").map(str::to_owned),
                _ => None,
            })
            .unwrap();
        feed(&mut s, &format!("@label={label} :srv BATCH +a chathistory bob"));
        feed(&mut s, "@batch=a;time=2026-10-22T10:30:00.000Z;msgid=q1 :bob!b@h PRIVMSG alice :while you were away");
        let out = feed(&mut s, ":srv BATCH -a");
        let got = histories(&out);
        assert_eq!(texts(&got[0].1), ["while you were away"]);
        // Then the page before, at the server's limit.
        assert_eq!(asked(&out).0, "CHATHISTORY BEFORE bob msgid=q0 5");
        assert_eq!(s.book().newest("bob").and_then(|a| a.msgid.as_deref()), Some("q1"));
    }

    /// A gap fill asked while a page before is out is sent once that one
    /// is in, never dropped, and opens its gap only when it goes.
    #[test]
    fn a_request_that_meets_one_in_flight_goes_after_it() {
        let mut s = historied("", 3, held());
        let (wire, label) = asked(&older(&mut s, "#test"));
        assert_eq!(wire, "CHATHISTORY BEFORE #test msgid=m0 3");
        assert!(sent(&feed(&mut s, OUR_JOIN)).is_empty());
        assert!(!s.gaps.contains_key("#test"), "no gap while it waits");
        let out = page(&mut s, Some(&label), "b", &[]);
        assert_eq!(asked(&out).0, "CHATHISTORY AFTER #test msgid=m0 3");
        assert!(s.gaps.contains_key("#test"));
    }

    fn filled(out: &[Output]) -> Vec<Target> {
        out.iter()
            .filter_map(|o| match o {
                Output::Event(Event::Filled { target, .. }) => Some(target.clone()),
                _ => None,
            })
            .collect()
    }

    /// The UI moves no read marker until a window's history is in: the
    /// session says when, after the page.
    #[test]
    fn a_window_is_filled_once_its_latest_page_or_gap_fill_is_in() {
        let chan = || Target::Channel("#test".into());
        let mut s = historied("", 3, Book::default());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        let out = page(&mut s, Some(&label), "l", &["m1"]);
        assert_eq!(filled(&out), [chan()]);
        let at = out.iter().position(|o| matches!(o, Output::Event(Event::Filled { .. }))).unwrap();
        assert!(out[..at].iter().any(|o| matches!(o, Output::Event(Event::History { .. }))), "after the page: {out:?}");
        // A gap fill: only its last page.
        let mut s = historied("", 2, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        let out = page(&mut s, Some(&label), "a", &["m1", "m2"]);
        assert!(filled(&out).is_empty(), "more to come: {out:?}");
        let (_, label) = asked(&out);
        assert_eq!(filled(&feed(&mut s, &format!("@label={label} :srv ACK"))), [chan()]);
        // No history: at once.
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, ":srv CAP * LS :batch");
        feed(&mut s, ":srv CAP alice ACK :batch");
        feed(&mut s, ":srv 001 alice :hi");
        assert_eq!(filled(&feed(&mut s, ":alice!a@h JOIN #test")), [chan()]);
    }

    #[test]
    fn older_while_a_request_is_out_waits_for_it() {
        let mut s = historied("", 2, Book::default());
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        assert!(sent(&older(&mut s, "#test")).is_empty());
        let out = page(&mut s, Some(&label), "a", &["m1", "m2"]);
        assert_eq!(texts(&histories(&out)[0].1), ["m1", "m2"]);
        assert_eq!(asked(&out).0, "CHATHISTORY BEFORE #test msgid=m1 2");
    }

    #[test]
    fn a_page_is_bounded() {
        let mut s = historied("", 2, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        let out = page(&mut s, Some(&label), "a", &["m1", "m2", "m3", "m4", "m5"]);
        assert_eq!(texts(&histories(&out)[0].1), ["m1", "m2"]);
        // Full, so the next page starts after the last one kept.
        assert_eq!(asked(&out).0, "CHATHISTORY AFTER #test msgid=m2 2");
        // One nobody asked for is held to the page size.
        let out = page(&mut s, None, "u", &["x1", "x2", "x3"]);
        assert_eq!(texts(&histories(&out)[0].1), ["x1", "x2"]);
    }

    #[test]
    fn without_labels_a_page_answers_its_targets_request() {
        let mut s = Session::new("net", SessionConfig { history_page: Some(20), ..cfg() });
        s.start();
        feed(&mut s, ":srv CAP * LS :server-time batch draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :server-time batch draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        // The end of the MOTD asks TARGETS; answered, it is out of the way.
        feed(&mut s, ":srv 376 alice :End of MOTD");
        feed(&mut s, ":srv BATCH +t draft/chathistory-targets");
        feed(&mut s, ":srv BATCH -t");
        let out = feed(&mut s, ":alice!a@h JOIN #test");
        assert_eq!(sent(&out), ["CHATHISTORY LATEST #test * 20"]);
        feed(&mut s, ":srv BATCH +e chathistory #test");
        assert_eq!(histories(&feed(&mut s, ":srv BATCH -e")), [(Target::Channel("#test".into()), vec![], true)]);
        assert!(s.inflight.is_empty());
    }

    #[test]
    fn an_unlabeled_fail_is_matched_by_its_target() {
        let mut s = historied("", 5, Book::default());
        asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        let out = feed(&mut s, ":srv FAIL CHATHISTORY INVALID_TARGET LATEST #TEST :nope");
        assert_eq!(histories(&out), [(Target::Channel("#test".into()), vec![], false)]);
        assert!(lines(&out).iter().any(|(t, l)| *t == Target::Server && l.text.contains("nope")));
        assert!(s.inflight.is_empty());
    }

    #[test]
    fn a_page_cut_off_by_the_drop_is_shown_and_kept() {
        let mut s = historied("", 5, held());
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        feed(&mut s, &format!("@label={label} :srv BATCH +a chathistory #test"));
        feed(&mut s, "@batch=a;time=2026-10-22T10:01:00.000Z;msgid=m1 :bob!b@h PRIVMSG #test :m1");
        let got = histories(&s.end(now()));
        assert_eq!(got.len(), 1);
        assert_eq!((texts(&got[0].1), got[0].2), (vec!["m1"], false));
        // What came is held, and no further.
        assert_eq!(newest(&s).as_deref(), Some("m1"));
    }

    #[test]
    fn a_history_batch_nobody_asked_for_is_still_history() {
        let mut s = historied("", 50, Book::default());
        feed(&mut s, ":srv BATCH +z chathistory #other");
        feed(&mut s, "@batch=z;time=2026-10-22T10:00:00.000Z;msgid=z1 :bob!b@h PRIVMSG #other :replayed on join");
        let got = histories(&feed(&mut s, ":srv BATCH -z"));
        assert_eq!(got.len(), 1);
        assert_eq!(texts(&got[0].1), ["replayed on join"]);
    }

    #[test]
    fn open_queries_are_backfilled_once_registered() {
        let mut book = Book::default();
        book.open("bob", true);
        let mut s = Session::new("net", cfg()).with_book(book);
        s.start();
        feed(&mut s, ":srv CAP * LS :server-time batch draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :server-time batch draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        feed(&mut s, ":srv 005 alice CHANTYPES=# CHATHISTORY=50 :are supported");
        let out = feed(&mut s, ":srv 376 alice :End of MOTD");
        let latest: Vec<String> = sent(&out).into_iter().filter(|l| l.contains("LATEST")).collect();
        assert_eq!(latest, ["CHATHISTORY LATEST bob * 50"]);
        // A query opened later is asked for at once.
        let out = s.command(Command::Open { network: "net".into(), target: "carol".into() }, now());
        assert_eq!(sent(&out), ["CHATHISTORY LATEST carol * 50"]);
        assert!(s.book().get("carol").is_some_and(|k| k.query));
    }

    /// A line to build on in tests.
    pub(crate) fn local_line() -> Line {
        Line {
            seq: 0,
            time: now(),
            msgid: None,
            kind: LineKind::Privmsg,
            nick: Some("bob".into()),
            text: String::new(),
            own: false,
            delivery: Delivery::Sent,
            boundary: false,
            replayed: false,
        }
    }

    /// What ZNC with its playback module offers: no chathistory.
    const ZNC: &str = "batch server-time message-tags znc.in/playback znc.in/self-message";

    /// Registered through ZNC with `caps` (all ACKed), `book` carried in,
    /// with the clock at `at`. Gives back what the end of the MOTD sent
    /// (the PLAY).
    fn zncd_at(caps: &str, book: Book, at: DateTime<Utc>) -> (Session, Vec<Output>) {
        let mut s = Session::new("net", cfg()).with_book(book);
        s.start();
        for l in [
            format!(":irc.znc.in CAP * LS :{caps}"),
            format!(":irc.znc.in CAP alice ACK :{caps}"),
            ":irc.znc.in 001 alice :Welcome".to_owned(),
            ":irc.znc.in 005 alice CHANTYPES=# :are supported".to_owned(),
        ] {
            s.handle(Message::parse(&l).unwrap(), at);
        }
        let out = s.handle(Message::parse(":irc.znc.in 376 alice :End of MOTD").unwrap(), at);
        (s, out)
    }

    /// Registered through ZNC with `caps` (all ACKed), `book` carried in.
    /// Gives back what the end of the MOTD sent (the PLAY).
    pub(crate) fn zncd_with(caps: &str, book: Book) -> (Session, Vec<Output>) {
        zncd_at(caps, book, now())
    }

    pub(crate) fn zncd(book: Book) -> (Session, Vec<Output>) {
        zncd_with(ZNC, book)
    }

    /// A recorded transcript's server lines (`> ` marks what the client sent).
    fn recorded(text: &str) -> impl Iterator<Item = &str> {
        text.lines().filter(|l| !l.is_empty() && !l.starts_with("> "))
    }

    fn plays(out: &[Output]) -> Vec<String> {
        sent(out).into_iter().filter(|m| m.starts_with("PRIVMSG *playback")).collect()
    }

    /// When the recordings were made: just after their last replayed line.
    fn recorded_at() -> DateTime<Utc> {
        "2026-11-02T02:28:03.220Z".parse().unwrap()
    }

    #[test]
    fn znc_is_asked_for_everything_after_the_newest_line_held() {
        let (_, out) = zncd(Book::default());
        assert_eq!(plays(&out), ["PRIVMSG *playback :PLAY * 0"]);
        // held(): #test up to 2026-10-22T10:00:00Z.
        let (_, out) = zncd(held());
        assert_eq!(plays(&out), ["PRIVMSG *playback :PLAY * 1792663200"]);
    }

    #[test]
    fn chathistory_wins_over_playback_and_the_config_can_say_no() {
        let (_, out) = zncd_with(&format!("{ZNC} draft/chathistory"), Book::default());
        assert!(plays(&out).is_empty(), "{out:?}");
        assert!(sent(&out).iter().any(|m| m.starts_with("CHATHISTORY TARGETS")), "{out:?}");
        // Without the cap, nothing to ask.
        let (_, out) = zncd_with("batch server-time", Book::default());
        assert!(plays(&out).is_empty(), "{out:?}");
        // History turned off for this server.
        let mut s = Session::new("net", SessionConfig { chathistory: false, ..cfg() });
        s.start();
        feed(&mut s, &format!(":irc.znc.in CAP * LS :{ZNC}"));
        feed(&mut s, &format!(":irc.znc.in CAP alice ACK :{ZNC}"));
        feed(&mut s, ":irc.znc.in 001 alice :Welcome");
        assert!(plays(&feed(&mut s, ":irc.znc.in 376 alice :End of MOTD")).is_empty());
    }

    #[test]
    fn a_playback_batch_is_one_page_of_history() {
        let (mut s, _) = zncd(Book::default());
        feed(&mut s, ":znc.in BATCH +p znc.in/playback #test");
        for t in ["one", "two"] {
            let out = feed(&mut s, &format!("@batch=p;time=2026-10-22T10:01:00.000Z :bob!b@h PRIVMSG #test :{t}"));
            assert!(lines(&out).is_empty(), "history is not news: {out:?}");
        }
        let got = histories(&feed(&mut s, ":znc.in BATCH -p"));
        let [(target, l, start)] = got.as_slice() else { panic!("{got:?}") };
        assert_eq!((target, *start), (&Target::Channel("#test".into()), false));
        assert_eq!(texts(l), ["one", "two"]);
        assert!(l.iter().all(|l| l.replayed));
    }

    #[test]
    fn a_query_replay_opens_its_window_with_both_sides() {
        let (mut s, _) = zncd(Book::default());
        feed(&mut s, ":znc.in BATCH +q znc.in/playback bob");
        feed(&mut s, "@batch=q;time=2026-10-22T10:01:00.000Z :bob!b@h PRIVMSG alice :psst");
        feed(&mut s, "@batch=q;time=2026-10-22T10:02:00.000Z :alice!a@znc.in PRIVMSG bob :hey");
        let got = histories(&feed(&mut s, ":znc.in BATCH -q"));
        let [(target, l, _)] = got.as_slice() else { panic!("{got:?}") };
        assert_eq!(*target, Target::Query("bob".into()));
        assert_eq!(texts(l), ["psst", "hey"]);
        assert_eq!(l.iter().map(|l| l.own).collect::<Vec<_>>(), [false, true]);
    }

    #[test]
    fn without_batches_a_line_timed_at_or_before_the_play_is_history() {
        let (mut s, out) = zncd_with("server-time znc.in/playback", held());
        assert_eq!(plays(&out), ["PRIVMSG *playback :PLAY * 1792663200"]);
        // Anything from before we asked is ZNC's replay, whichever buffer
        // it is from and in whatever order it comes.
        for at in ["2026-10-22T10:30:00.000Z", "2026-10-22T12:00:00.000Z", "2026-10-22T10:00:00.000Z"] {
            let out = feed(&mut s, &format!("@time={at} :bob!b@h PRIVMSG #test :old"));
            assert!(lines(&out).is_empty(), "{out:?}");
            assert_eq!(histories(&out).iter().flat_map(|(_, l, _)| texts(l)).collect::<Vec<_>>(), ["old"]);
            assert!(histories(&out)[0].1[0].replayed);
        }
        let out = feed(&mut s, "@time=2026-10-22T12:00:01.000Z :bob!b@h PRIVMSG #test :new");
        assert_eq!(lines(&out).len(), 1, "{out:?}");
        assert!(histories(&out).is_empty(), "{out:?}");
    }

    #[test]
    fn with_batches_only_the_batch_is_history() {
        // The batch holds the whole replay: a loose line timed in the past
        // (a clock ahead of the server's) is still news.
        let (mut s, _) = zncd(held());
        let out = feed(&mut s, "@time=2026-10-22T11:00:00.000Z :bob!b@h PRIVMSG #test :live");
        assert_eq!(lines(&out).len(), 1, "{out:?}");
    }

    #[test]
    fn before_any_play_old_times_are_not_history() {
        // History off: no PLAY, so no line is taken for its replay.
        let (mut s, _) = zncd_with("server-time", Book::default());
        let out = feed(&mut s, "@time=2026-10-22T10:00:00.000Z :bob!b@h PRIVMSG #test :old but live");
        assert_eq!(lines(&out).len(), 1, "{out:?}");
    }

    /// No event at all in `out`: no line, no history, no window.
    fn silent(out: &[Output]) -> bool {
        out.iter().all(|o| !matches!(o, Output::Event(_)))
    }

    #[test]
    fn our_play_echoed_back_shows_nothing() {
        // The `*playback` query held open, so only the echo guard keeps
        // the echo out (not the rule for services nobody opened).
        let mut book = Book::default();
        book.open("*playback", true);
        let (mut s, _) = zncd_with(&format!("{ZNC} echo-message"), book.clone());
        let out = feed(&mut s, "@time=2026-10-22T12:00:00.000Z :alice!~alice@127.0.0.1 PRIVMSG *playback :PLAY * 0");
        assert!(silent(&out), "{out:?}");
        // Without batches the echo is timed before the cut, and still shows nothing.
        let (mut s, _) = zncd_with("server-time echo-message znc.in/playback", book);
        let out = feed(&mut s, "@time=2026-10-22T11:59:59.000Z :alice!~alice@127.0.0.1 PRIVMSG *playback :PLAY * 0");
        assert!(silent(&out), "{out:?}");
    }

    #[test]
    fn our_play_echoed_back_altered_still_shows_nothing() {
        let mut book = Book::default();
        book.open("*playback", true);
        let (mut s, _) = zncd_with(&format!("{ZNC} echo-message"), book);
        for echo in ["PRIVMSG *playback :PLAY * 0 ", "PRIVMSG *Playback :play * 0", "PRIVMSG *playback :PLAY * 0.000", "NOTICE *playback :PLAY #test 0"] {
            let out = feed(&mut s, &format!("@time=2026-10-22T12:00:00.000Z :alice!~alice@127.0.0.1 {echo}"));
            assert!(silent(&out), "{echo}: {out:?}");
        }
        // With no PLAY out (history off), the user's own `PLAY` to an open
        // `*playback` is an ordinary line.
        let mut book = Book::default();
        book.open("*playback", true);
        let mut s = Session::new("net", SessionConfig { chathistory: false, ..cfg() }).with_book(book);
        s.start();
        feed(&mut s, &format!(":irc.znc.in CAP * LS :{ZNC} echo-message"));
        feed(&mut s, ":irc.znc.in CAP alice ACK :server-time echo-message");
        feed(&mut s, ":irc.znc.in 001 alice :Welcome");
        feed(&mut s, ":irc.znc.in 376 alice :End of MOTD");
        assert_eq!(lines(&feed(&mut s, ":alice!a@h PRIVMSG *playback :PLAY * 0")).len(), 1);
    }

    #[test]
    fn our_own_line_to_a_service_opens_nothing_unless_the_user_opened_it() {
        // Sent from another client (self-message), or echoed: no line, and
        // the book holds no such query.
        let mut s = registered("server-time echo-message");
        for who in ["NickServ", "*status"] {
            let out = feed(&mut s, &format!("@time=2026-10-22T12:00:00.000Z :alice!a@h PRIVMSG {who} :IDENTIFY not-a-real-one"));
            assert!(silent(&out), "{who}: {out:?}");
            assert!(s.book().get(who).is_none(), "{who} was opened");
        }
        // The user writing there opens it: the pending line and its echo show.
        let out = s.command(Command::Say { network: "net".into(), target: "NickServ".into(), text: "HELP".into() }, now());
        assert_eq!(lines(&out).len(), 1);
        assert!(s.book().get("NickServ").is_some_and(|k| k.query));
        let out = feed(&mut s, "@time=2026-10-22T12:00:01.000Z :alice!a@h PRIVMSG NickServ :HELP");
        assert_eq!(lines(&out).iter().map(|(t, l)| (t.clone(), l.text.as_str())).collect::<Vec<_>>(), [(Target::Query("NickServ".into()), "HELP")]);
        // One the user opened shows our lines from elsewhere too.
        let mut s = registered("server-time");
        s.command(Command::Open { network: "net".into(), target: "*status".into() }, now());
        assert_eq!(lines(&feed(&mut s, ":alice!a@h PRIVMSG *status :ListNetworks")).len(), 1);
    }

    #[test]
    fn with_history_off_znc_s_playback_is_not_asked_for() {
        let mut s = Session::new("net", SessionConfig { chathistory: false, ..cfg() });
        s.start();
        let req = sent(&feed(&mut s, &format!(":irc.znc.in CAP * LS :{ZNC}")));
        assert_eq!(req, ["CAP REQ :message-tags server-time batch znc.in/self-message"]);
        // ZNC's own attach replay then comes plain, as news.
        feed(&mut s, ":irc.znc.in CAP alice ACK :message-tags server-time batch znc.in/self-message");
        feed(&mut s, ":irc.znc.in 001 alice :Welcome");
        assert!(plays(&feed(&mut s, ":irc.znc.in 376 alice :End of MOTD")).is_empty());
        let out = feed(&mut s, "@time=2026-10-22T10:00:00.000Z :bob!b@h PRIVMSG #test :old");
        assert_eq!(lines(&out).len(), 1, "{out:?}");
        // History on: asked for.
        let mut s = Session::new("net", cfg());
        s.start();
        let req = sent(&feed(&mut s, &format!(":irc.znc.in CAP * LS :{ZNC}")));
        assert_eq!(req, ["CAP REQ :message-tags server-time batch znc.in/self-message znc.in/playback"]);
    }

    #[test]
    fn empty_playback_batches_show_nothing() {
        let (mut s, _) = zncd(Book::default());
        for r in ["8f5180964ef46eeeae1b9d4f0d5de848", "9f9d51bc70ef21ca5c14f307980a29d8"] {
            assert!(feed(&mut s, &format!(":znc.in BATCH +{r} znc.in/playback #test")).is_empty());
            assert!(feed(&mut s, &format!(":znc.in BATCH -{r}")).is_empty());
        }
    }

    #[test]
    fn a_services_replay_opens_no_window() {
        let (mut s, _) = zncd(Book::default());
        for (r, who) in [("n", "NickServ"), ("z", "*status")] {
            feed(&mut s, &format!(":znc.in BATCH +{r} znc.in/playback {who}"));
            feed(&mut s, &format!("@batch={r};time=2026-10-22T10:01:00.000Z :{who}!s@services NOTICE alice :You are now identified"));
            let out = feed(&mut s, &format!(":znc.in BATCH -{r}"));
            assert!(histories(&out).is_empty(), "{out:?}");
            assert!(s.book().get(who).is_none(), "{who} was opened");
        }
        // Loose, the same.
        let (mut s, _) = zncd_with("server-time znc.in/playback", Book::default());
        let out = feed(&mut s, "@time=2026-10-22T10:01:00.000Z :*status!znc@znc.in PRIVMSG alice :You have no networks");
        assert!(out.iter().all(|o| !matches!(o, Output::Event(_))), "{out:?}");
        // One the user has open shows it.
        let mut book = Book::default();
        book.open("NickServ", true);
        let (mut s, _) = zncd(book);
        feed(&mut s, ":znc.in BATCH +n znc.in/playback NickServ");
        feed(&mut s, "@batch=n;time=2026-10-22T10:01:00.000Z :NickServ!s@services NOTICE alice :hi");
        assert_eq!(histories(&feed(&mut s, ":znc.in BATCH -n")).len(), 1);
    }

    /// Feeds a recorded ZNC replay into a session registered with `caps`
    /// at `at`, and checks it all came as history: `chan` in #test, and
    /// `fact dm` from bob.
    fn znc_replay_is_history(caps: &str, at: DateTime<Utc>, transcript: &str, chan: &[&str]) {
        let (mut s, _) = zncd_at(caps, Book::default(), at);
        let out: Vec<Output> = recorded(transcript).flat_map(|l| s.handle(Message::parse(l).unwrap(), at)).collect();
        let news: Vec<(Target, String)> = lines(&out).into_iter().filter(|(t, _)| *t != Target::Server).map(|(t, l)| (t, l.text)).collect();
        assert!(news.is_empty(), "came as news: {news:?}");
        let got = histories(&out);
        let said = |t: Target| got.iter().filter(|(target, ..)| *target == t).flat_map(|(_, l, _)| texts(l)).collect::<Vec<_>>();
        assert_eq!(said(Target::Channel("#test".into())), chan);
        assert_eq!(said(Target::Query("bob".into())), ["fact dm"]);
        assert!(got.iter().all(|(t, ..)| matches!(t, Target::Channel(c) if c == "#test") || *t == Target::Query("bob".into())), "{got:?}");
        assert!(s.book().get("*playback").is_none());
    }

    #[test]
    fn the_recorded_znc_replay_is_history_not_news() {
        // Batches don't need the clock: ours is days behind the recording.
        let caps = format!("{ZNC} echo-message");
        znc_replay_is_history(&caps, now(), include_str!("../tests/data/znc-1.10.3-play-batch.txt"), &["fact 1", "fact 2", "fact 3"]);
    }

    #[test]
    fn the_recorded_znc_replay_without_batches_is_history_too() {
        let chan = ["fact 1", "fact 2", "fact 3", "fact live", "fact self"];
        znc_replay_is_history("server-time znc.in/playback", recorded_at(), include_str!("../tests/data/znc-1.10.3-play-plain.txt"), &chan);
    }

    #[test]
    fn the_recorded_attach_then_play_reuses_batch_refs() {
        // The real attach: ZNC's caps, its stale-timed burst, our JOIN and
        // one empty batch per buffer. Then the PLAY's replay, in the same refs.
        let mut s = Session::new("net", cfg());
        s.start();
        let at = recorded_at();
        let mut out = Vec::new();
        for l in recorded(include_str!("../tests/data/znc-1.10.3-attach.txt")) {
            out.extend(s.handle(Message::parse(l).unwrap(), at));
        }
        assert_eq!(plays(&out), ["PRIVMSG *playback :PLAY * 0"]);
        assert!(histories(&out).is_empty(), "{out:?}");
        assert!(lines(&out).iter().all(|(t, _)| *t == Target::Server || *t == Target::Channel("#test".into())), "{out:?}");
        let out: Vec<Output> =
            recorded(include_str!("../tests/data/znc-1.10.3-play-batch.txt")).flat_map(|l| s.handle(Message::parse(l).unwrap(), at)).collect();
        assert!(lines(&out).iter().all(|(t, _)| *t == Target::Server), "{out:?}");
        let got = histories(&out);
        let said: Vec<(Target, Vec<&str>)> = got.iter().map(|(t, l, _)| (t.clone(), texts(l))).collect();
        assert_eq!(said, [(Target::Channel("#test".into()), vec!["fact 1", "fact 2", "fact 3"]), (Target::Query("bob".into()), vec!["fact dm"])]);
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

    #[test]
    fn replayed_events_read_as_live_ones_but_change_nothing() {
        let mut s = historied("draft/event-playback", 50, Book::default());
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        feed(&mut s, ":srv 353 alice = #test :alice dan");
        feed(&mut s, &format!("@label={label} :srv BATCH +h chathistory #test"));
        let events = [
            "@batch=h;time=2026-10-22T10:00:00.000Z;msgid=e1 :erin!e@h JOIN #test",
            "@batch=h;time=2026-10-22T10:00:01.000Z;msgid=e2 :erin!e@h NICK erin_",
            "@batch=h;time=2026-10-22T10:00:02.000Z;msgid=e3 :erin_!e@h PART #test :bye",
            "@batch=h;time=2026-10-22T10:00:03.000Z;msgid=e4 :dan!d@h QUIT :gone",
            "@batch=h;time=2026-10-22T10:00:04.000Z;msgid=e5 :op!o@h MODE #test +o dan",
            "@batch=h;time=2026-10-22T10:00:05.000Z;msgid=e6 :op!o@h TOPIC #test :old topic",
            "@batch=h;time=2026-10-22T10:00:06.000Z;msgid=e7 :op!o@h KICK #test dan :out",
            "@batch=h;time=2026-10-22T10:00:07.000Z;msgid=e8 :dan!d@h QUIT :x",
            "@batch=h;time=2026-10-22T10:00:08.000Z;msgid=e9 :alice!a@h PART #test",
        ];
        for e in events {
            assert!(feed(&mut s, e).is_empty(), "{e}");
        }
        let out = feed(&mut s, ":srv BATCH -h");
        assert!(!out.iter().any(|o| matches!(o, Output::Event(Event::Parted { .. } | Event::Peer { .. }))), "{out:?}");
        let got = histories(&out);
        let lines = &got[0].1;
        assert_eq!(
            texts(lines),
            [
                "erin joined",
                "erin is now known as erin_",
                "erin_ left (bye)",
                "dan quit (gone)",
                "op set mode +o dan",
                "op changed the topic to: old topic",
                "dan was kicked by op (out)",
                "dan quit (x)",
                "alice left"
            ]
        );
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [LineKind::Join, LineKind::Nick, LineKind::Part, LineKind::Quit, LineKind::Mode, LineKind::Topic, LineKind::Kick, LineKind::Quit, LineKind::Part]
        );
        assert!(lines[8].own);
        // The past is not now: nobody joined or left our member lists, dan
        // (kicked and quit, back then) is still here, and so are we.
        assert!(s.channels_with("erin").is_empty() && s.channels_with("erin_").is_empty());
        assert_eq!(s.channels_with("dan"), ["#test"]);
        assert_eq!(s.channels_with("alice"), ["#test"]);
    }

    #[test]
    fn targets_open_the_queries_that_were_busy_while_away() {
        let mut book = Book::default();
        book.note("#test", false, &[Line { msgid: Some("c1".into()), time: "2026-10-22T09:00:00Z".parse().unwrap(), ..local_line() }]);
        let mut s = Session::new("net", SessionConfig { history_page: Some(20), ..cfg() }).with_book(book);
        s.start();
        feed(&mut s, ":srv CAP * LS :batch server-time labeled-response draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :batch server-time labeled-response draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        feed(&mut s, ":srv 005 alice CHANTYPES=# CHATHISTORY=50 :are supported");
        let out = feed(&mut s, ":srv 376 alice :End of MOTD");
        // From the newest line held to an hour from now.
        let (wire, label) = asked(&out);
        assert_eq!(wire, "CHATHISTORY TARGETS timestamp=2026-10-22T09:00:00.000Z timestamp=2026-10-22T13:00:00.000Z 20");
        feed(&mut s, &format!("@label={label} :srv BATCH +t draft/chathistory-targets"));
        for t in ["bob 2026-10-22T11:00:00.000Z", "#test 2026-10-22T11:30:00.000Z", "alice 2026-10-22T11:40:00.000Z"] {
            assert!(feed(&mut s, &format!("@batch=t :srv CHATHISTORY TARGETS {t}")).is_empty());
        }
        let out = feed(&mut s, ":srv BATCH -t");
        // Only bob: not a channel, not us.
        assert_eq!(asked(&out).0, "CHATHISTORY LATEST bob * 20");
        assert_eq!(s.book().queries(), ["bob"]);
    }

    #[test]
    fn with_nothing_held_targets_look_back_a_week() {
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, ":srv CAP * LS :batch server-time draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :batch server-time draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        let out = feed(&mut s, ":srv 422 alice :MOTD File is missing");
        assert_eq!(sent(&out), ["CHATHISTORY TARGETS timestamp=2026-10-15T12:00:00.000Z timestamp=2026-10-22T13:00:00.000Z 100"]);
    }

    #[test]
    fn a_server_that_sends_no_motd_end_syncs_at_its_first_other_line() {
        // Some bouncers send neither 376 nor 422; registration still ends.
        let mut s = Session::new("net", cfg());
        s.start();
        feed(&mut s, ":srv CAP * LS :batch server-time draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :batch server-time draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        assert!(sent(&feed(&mut s, ":srv 005 alice CHANTYPES=# :are supported")).is_empty());
        let out = feed(&mut s, ":alice!a@h JOIN #test");
        assert!(sent(&out).contains(&"CHATHISTORY TARGETS timestamp=2026-10-15T12:00:00.000Z timestamp=2026-10-22T13:00:00.000Z 100".to_owned()), "{out:?}");
        // Once synced, a late MOTD end changes nothing.
        assert!(sent(&feed(&mut s, ":srv 376 alice :End of MOTD")).is_empty());
    }

    #[test]
    fn only_our_targets_answer_opens_queries_and_held_ones_are_not_asked_twice() {
        // Answered already (by `historied`): a TARGETS batch now is not ours.
        let mut s = historied("", 50, Book::default());
        feed(&mut s, ":srv BATCH +t draft/chathistory-targets");
        feed(&mut s, "@batch=t :srv CHATHISTORY TARGETS bob 2026-10-22T11:00:00.000Z");
        assert!(sent(&feed(&mut s, ":srv BATCH -t")).is_empty());
        assert!(s.book().queries().is_empty());

        // zoe's query is open, so the end of MOTD synced it with the rest.
        let mut book = Book::default();
        book.open("zoe", true);
        let mut s = Session::new("net", SessionConfig { history_page: Some(20), ..cfg() }).with_book(book);
        s.start();
        feed(&mut s, ":srv CAP * LS :batch server-time labeled-response draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :batch server-time labeled-response draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        let out = feed(&mut s, ":srv 376 alice :End of MOTD");
        let label = |o: &[Output], sub: &str| {
            o.iter().find_map(|o| match o {
                Output::Send(m) if m.params.first().map(String::as_str) == Some(sub) => m.tag("label").map(str::to_owned),
                _ => None,
            })
        };
        let (zoe, targets) = (label(&out, "LATEST").unwrap(), label(&out, "TARGETS").unwrap());
        feed(&mut s, &format!("@label={zoe} :srv ACK"));
        feed(&mut s, &format!("@label={targets} :srv BATCH +t draft/chathistory-targets"));
        feed(&mut s, "@batch=t :srv CHATHISTORY TARGETS Zoe 2026-10-22T11:00:00.000Z");
        assert!(sent(&feed(&mut s, ":srv BATCH -t")).is_empty());
        assert!(s.inflight.is_empty());
    }

    /// Registered with labels and server-time, `page` lines a page: the
    /// session and what the end of MOTD sent (TARGETS among it).
    fn discovering(page: usize, book: Book) -> (Session, Vec<Output>) {
        let mut s = Session::new("net", SessionConfig { history_page: Some(page), ..cfg() }).with_book(book);
        s.start();
        feed(&mut s, ":srv CAP * LS :batch server-time labeled-response draft/chathistory");
        feed(&mut s, ":srv CAP alice ACK :batch server-time labeled-response draft/chathistory");
        feed(&mut s, ":srv 001 alice :hi");
        feed(&mut s, ":srv 005 alice CHANTYPES=# CHATHISTORY=50 :are supported");
        let out = feed(&mut s, ":srv 376 alice :End of MOTD");
        (s, out)
    }

    fn week_back() -> DateTime<Utc> {
        "2026-10-15T12:00:00Z".parse().unwrap()
    }

    /// Every line sent, without its label.
    fn bare(out: &[Output]) -> Vec<String> {
        out.iter()
            .filter_map(|o| match o {
                Output::Send(m) => Some(Message { tags: m.tags.iter().filter(|(k, _)| k != "label").cloned().collect(), ..m.clone() }.to_wire()),
                _ => None,
            })
            .collect()
    }

    /// The label of the CHATHISTORY `sub` request in `out`.
    fn label_of(out: &[Output], sub: &str) -> String {
        out.iter()
            .find_map(|o| match o {
                Output::Send(m) if m.command == "CHATHISTORY" && m.params.first().map(String::as_str) == Some(sub) => m.tag("label").map(str::to_owned),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {sub} in {out:?}"))
    }

    /// Answers the TARGETS in `out` with `names` (`"nick time"`), closed.
    fn answer_targets(s: &mut Session, out: &[Output], names: &[&str]) -> Vec<Output> {
        feed(s, &format!("@label={} :srv BATCH +t draft/chathistory-targets", label_of(out, "TARGETS")));
        for n in names {
            assert!(feed(s, &format!("@batch=t :srv CHATHISTORY TARGETS {n}")).is_empty());
        }
        feed(s, ":srv BATCH -t")
    }

    #[test]
    fn targets_never_open_services_or_malformed_names() {
        let mut book = Book::default();
        book.open("NickServ", true);
        book.open("zoe", true);
        let (mut s, out) = discovering(20, book);
        // A services query the user opened is not backfilled on its own.
        assert!(!bare(&out).iter().any(|l| l.contains("NickServ")), "{out:?}");
        assert!(bare(&out).contains(&"CHATHISTORY LATEST zoe * 20".to_owned()));
        let at = "2026-10-22T11:00:00.000Z";
        let names =
            [format!("nickserv {at}"), format!("ChanServ {at}"), format!("bad\x01nick {at}"), format!("FooServ {at}"), format!("bob {at}"), ":".to_owned()];
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let out = answer_targets(&mut s, &out, &names);
        // No suffix rule: FooServ is someone.
        assert_eq!(bare(&out), ["CHATHISTORY LATEST FooServ * 20", "CHATHISTORY LATEST bob * 20"]);
        assert_eq!(s.book().queries(), ["FooServ", "NickServ", "bob", "zoe"]);
        // Opened by the user, a services query is theirs to read.
        let out = s.command(Command::Open { network: "net".into(), target: "ChanServ".into() }, now());
        assert_eq!(bare(&out), ["CHATHISTORY LATEST ChanServ * 20"]);
    }

    #[test]
    fn markread_from_the_server_is_the_read_marker() {
        let mut s = historied("draft/read-marker", 50, Book::default());
        let out = feed(&mut s, ":srv MARKREAD #test timestamp=2026-10-22T10:00:00.000Z");
        assert_eq!(
            out,
            [Output::Event(Event::ReadMarker {
                network: "net".into(),
                target: Target::Channel("#test".into()),
                time: Some("2026-10-22T10:00:00Z".parse().unwrap())
            })]
        );
        let out = feed(&mut s, ":srv MARKREAD bob *");
        assert_eq!(out, [Output::Event(Event::ReadMarker { network: "net".into(), target: Target::Query("bob".into()), time: None })]);
    }

    #[test]
    fn a_malformed_markread_is_no_time_or_nothing() {
        let mut s = historied("draft/read-marker", 50, Book::default());
        let out = feed(&mut s, ":srv MARKREAD #test timestamp=garbage");
        assert_eq!(out, [Output::Event(Event::ReadMarker { network: "net".into(), target: Target::Channel("#test".into()), time: None })]);
        assert!(feed(&mut s, ":srv MARKREAD #test").is_empty());
    }

    #[test]
    fn marking_read_tells_the_server_only_when_it_takes_markers() {
        let time: DateTime<Utc> = "2026-10-22T10:00:00.5Z".parse().unwrap();
        let mark = Command::MarkRead { network: "net".into(), target: "#test".into(), time };
        let mut s = historied("draft/read-marker", 50, Book::default());
        assert_eq!(sent(&s.command(mark.clone(), now())), ["MARKREAD #test timestamp=2026-10-22T10:00:00.500Z"]);
        let mut s = historied("", 50, Book::default());
        assert!(s.command(mark, now()).is_empty());
    }

    #[test]
    fn a_query_brought_up_to_date_asks_for_its_marker() {
        let mut s = historied("draft/read-marker", 50, Book::default());
        let out = s.command(Command::Open { network: "net".into(), target: "bob".into() }, now());
        let wire = sent(&out);
        assert!(wire.contains(&"MARKREAD bob".to_owned()), "{wire:?}");
        assert!(wire.iter().any(|l| l.ends_with("CHATHISTORY LATEST bob * 20")), "{wire:?}");
        // Without the cap, no MARKREAD.
        let mut s = historied("", 50, Book::default());
        let wire = sent(&s.command(Command::Open { network: "net".into(), target: "bob".into() }, now()));
        assert!(!wire.iter().any(|l| l.starts_with("MARKREAD")), "{wire:?}");
    }

    #[test]
    fn services_queries_the_session_opened_are_not_asked_for_markers() {
        let mut book = Book::default();
        book.open("NickServ", true);
        book.open("bob", true);
        let mut s = historied("draft/read-marker", 50, book);
        // `sync` ran at registration inside `historied`; run it again via a fresh session.
        let mut s2 = Session::new("net", SessionConfig { history_page: Some(20), ..cfg() }).with_book(s.take_book());
        s2.start();
        feed(&mut s2, ":srv CAP * LS :message-tags server-time batch labeled-response draft/chathistory draft/read-marker");
        feed(&mut s2, ":srv CAP alice ACK :message-tags server-time batch labeled-response draft/chathistory draft/read-marker");
        feed(&mut s2, ":srv 001 alice :hi");
        let out = feed(&mut s2, ":srv 005 alice CHANTYPES=# CHATHISTORY=50 :are supported");
        let mut wire = sent(&out);
        wire.extend(sent(&feed(&mut s2, ":srv 376 alice :End of MOTD")));
        assert!(wire.contains(&"MARKREAD bob".to_owned()), "{wire:?}");
        assert!(!wire.iter().any(|l| l.contains("NickServ")), "{wire:?}");
        // Opened by the user, it is theirs.
        let wire = sent(&s2.command(Command::Open { network: "net".into(), target: "NickServ".into() }, now()));
        assert!(wire.contains(&"MARKREAD NickServ".to_owned()), "{wire:?}");
    }

    #[test]
    fn targets_cut_off_by_a_drop_ask_again_from_the_old_point() {
        let mut book = held();
        book.set_discovered("2026-10-22T08:00:00Z".parse().unwrap());
        let (mut s, out) = discovering(20, book);
        assert_eq!(bare(&out), ["CHATHISTORY TARGETS timestamp=2026-10-22T08:00:00.000Z timestamp=2026-10-22T13:00:00.000Z 20"]);
        // The channel catches up past that point; TARGETS never answers.
        let (_, label) = asked(&feed(&mut s, OUR_JOIN));
        page(&mut s, Some(&label), "a", &["m1"]);
        s.end(now());
        let book = s.take_book();
        assert_eq!(book.last_seen(), Some("2026-10-22T11:00:00Z".parse().unwrap()));
        assert_eq!(book.discovered(), Some("2026-10-22T08:00:00Z".parse().unwrap()));

        // The next connection still asks from there, and an answer in full
        // moves the point to when it was asked.
        let (mut s, out) = discovering(20, book);
        assert_eq!(bare(&out), ["CHATHISTORY TARGETS timestamp=2026-10-22T08:00:00.000Z timestamp=2026-10-22T13:00:00.000Z 20"]);
        answer_targets(&mut s, &out, &[]);
        assert_eq!(s.book().discovered(), Some(now()));
        let (_, out) = discovering(20, s.take_book());
        assert_eq!(bare(&out), ["CHATHISTORY TARGETS timestamp=2026-10-22T11:00:00.000Z timestamp=2026-10-22T13:00:00.000Z 20"]);
    }

    #[test]
    fn a_full_targets_answer_asks_on_from_its_last_name() {
        let (mut s, out) = discovering(2, Book::default());
        assert_eq!(bare(&out), ["CHATHISTORY TARGETS timestamp=2026-10-15T12:00:00.000Z timestamp=2026-10-22T13:00:00.000Z 2"]);
        let out = answer_targets(&mut s, &out, &["bob 2026-10-22T11:00:00.000Z", "carol 2026-10-22T11:30:00.000Z"]);
        assert_eq!(
            bare(&out),
            [
                "CHATHISTORY LATEST bob * 2",
                "CHATHISTORY LATEST carol * 2",
                "CHATHISTORY TARGETS timestamp=2026-10-22T11:30:00.000Z timestamp=2026-10-22T13:00:00.000Z 2"
            ]
        );
        // Not all found yet: still where it asked from.
        assert_eq!(s.book().discovered(), Some(week_back()));
        let out = answer_targets(&mut s, &out, &["carol 2026-10-22T11:30:00.000Z"]);
        assert!(bare(&out).is_empty(), "{out:?}");
        assert_eq!(s.book().discovered(), Some(now()));
    }

    #[test]
    fn targets_follow_ups_are_bounded() {
        let (mut s, mut out) = discovering(1, Book::default());
        let mut asked_for = 1;
        for i in 0.. {
            let name = format!("n{i} 2026-10-22T11:{i:02}:00.000Z");
            out = answer_targets(&mut s, &out, &[&name]);
            if !bare(&out).iter().any(|l| l.starts_with("CHATHISTORY TARGETS")) {
                break;
            }
            asked_for += 1;
        }
        assert_eq!(asked_for, history::MAX_TARGETS_PAGES);
        // Given up, not done: the next connection looks from the start again.
        assert_eq!(s.book().discovered(), Some(week_back()));
        assert!(!s.inflight.contains_key(history::TARGETS));
    }

    #[test]
    fn a_full_targets_answer_that_does_not_move_on_stops() {
        let (mut s, out) = discovering(1, Book::default());
        let out = answer_targets(&mut s, &out, &["bob 2026-10-15T12:00:00.000Z"]);
        assert_eq!(bare(&out), ["CHATHISTORY LATEST bob * 1"]);
        assert_eq!(s.book().discovered(), Some(week_back()));
    }

    #[test]
    fn without_chathistory_a_timed_line_gets_a_made_up_msgid() {
        let mut s = registered(ECHO);
        let (t, l) = lines(&feed(&mut s, "@time=2026-10-22T10:00:00.250Z :bob!b@h PRIVMSG #test :hi")).remove(0);
        let id = l.msgid.clone().expect("made up");
        assert!(crate::msgid::is_made_up(&id), "{id}");
        assert_eq!(id, crate::msgid::made_up(&t, &l));
        // Untimed: our clock, which differs between copies, so no id.
        assert_eq!(lines(&feed(&mut s, ":bob!b@h PRIVMSG #test :hi"))[0].1.msgid, None);
        // The server's own msgid stays.
        let srv = lines(&feed(&mut s, "@time=2026-10-22T10:00:00.250Z;msgid=srv1 :bob!b@h PRIVMSG #test :hi")).remove(0).1;
        assert_eq!(srv.msgid.as_deref(), Some("srv1"));
        // Never in the server buffer.
        assert_eq!(lines(&feed(&mut s, "@time=2026-10-22T10:00:00.250Z :srv NOTICE alice :hello"))[0].1.msgid, None);
        // Event lines too: a timed JOIN.
        let join = lines(&feed(&mut s, "@time=2026-10-22T10:00:01.000Z :carol!c@h JOIN #test")).remove(0).1;
        assert!(join.msgid.as_deref().is_some_and(crate::msgid::is_made_up));
    }

    #[test]
    fn with_chathistory_nothing_is_made_up() {
        let mut s = historied("", 50, Book::default());
        let l = lines(&feed(&mut s, "@time=2026-10-22T10:00:00.250Z :bob!b@h PRIVMSG #test :hi")).remove(0).1;
        assert_eq!(l.msgid, None);
    }

    #[test]
    fn a_live_line_and_its_replay_are_one_line() {
        let (mut s, _) = zncd(Book::default());
        let live = feed(&mut s, "@time=2026-10-22T10:01:00.123Z :bob!b@h PRIVMSG #test :same");
        feed(&mut s, ":irc.znc.in BATCH +p znc.in/playback #test");
        feed(&mut s, "@batch=p;time=2026-10-22T10:01:00.123Z :bob!b@h PRIVMSG #test :same");
        let replay = feed(&mut s, ":irc.znc.in BATCH -p");
        assert_eq!(lines(&live)[0].1.msgid, histories(&replay)[0].1[0].msgid);
        let mut store = crate::store::Store::new(["net".to_owned()]);
        for o in live.into_iter().chain(replay) {
            if let Output::Event(e) = o {
                store.apply(e);
            }
        }
        let b = store.buffer_ix(0, &Target::Channel("#test".into()));
        assert_eq!(store.networks[0].buffers[b].lines.iter().filter(|l| l.text == "same").count(), 1);
    }

    /// Ruling Q1: ZNC passes the server's msgid on a live line, but its
    /// replay carries none (Facts row 6). On a playback network the
    /// made-up id always wins, so both copies get one key.
    #[test]
    fn on_znc_the_recorded_live_line_and_its_replay_are_one_line() {
        let at = recorded_at();
        let (mut s, _) = zncd_at(&format!("{ZNC} echo-message"), Book::default(), at);
        let live: Vec<Output> = recorded(include_str!("../tests/data/znc-1.10.3-live.txt")).flat_map(|l| s.handle(Message::parse(l).unwrap(), at)).collect();
        let id = lines(&live)[0].1.msgid.clone().expect("an id");
        assert!(crate::msgid::is_made_up(&id), "the server's msgid was kept: {id}");
        let replay: Vec<Output> =
            recorded(include_str!("../tests/data/znc-1.10.3-play-after-live.txt")).flat_map(|l| s.handle(Message::parse(l).unwrap(), at)).collect();
        let replayed: Vec<Line> = histories(&replay).into_iter().flat_map(|(_, l, _)| l).collect();
        assert!(replayed.iter().all(|l| l.msgid.as_deref().is_some_and(crate::msgid::is_made_up)), "{replayed:?}");
        assert_eq!(replayed.iter().find(|l| l.text == "fact live").and_then(|l| l.msgid.clone()), Some(id));
        let mut store = crate::store::Store::new(["net".to_owned()]);
        for o in live.into_iter().chain(replay) {
            if let Output::Event(e) = o {
                store.apply(e);
            }
        }
        let b = store.buffer_ix(0, &Target::Channel("#test".into()));
        assert_eq!(store.networks[0].buffers[b].lines.iter().filter(|l| l.text == "fact live").count(), 1);
    }

    /// Every stamping site, not just `on_message` and `on_join`: a live
    /// event or action and its copy in ZNC's replay get one made-up id.
    #[test]
    fn a_live_event_or_action_and_its_replay_are_one_line() {
        for (i, (said, kind)) in [
            (":bob!b@h PART #test :bye", LineKind::Part),
            (":bob!b@h QUIT :gone", LineKind::Quit),
            (":bob!b@h NICK bobby", LineKind::Nick),
            (":bob!b@h TOPIC #test :new topic", LineKind::Topic),
            (":bob!b@h PRIVMSG #test :\x01ACTION waves\x01", LineKind::Action),
        ]
        .into_iter()
        .enumerate()
        {
            let (mut s, _) = zncd(Book::default());
            feed(&mut s, OUR_JOIN);
            feed(&mut s, "@time=2026-10-22T10:00:00.000Z :bob!b@h JOIN #test");
            let time = format!("time=2026-10-22T10:01:0{i}.123Z");
            let live = lines(&feed(&mut s, &format!("@{time} {said}")));
            let [(Target::Channel(_), live)] = live.as_slice() else { panic!("{said}: {live:?}") };
            assert_eq!(live.kind, kind, "{said}");
            feed(&mut s, ":znc.in BATCH +p znc.in/playback #test");
            feed(&mut s, &format!("@batch=p;{time} {said}"));
            let replay = histories(&feed(&mut s, ":znc.in BATCH -p"));
            let [(_, replayed, _)] = replay.as_slice() else { panic!("{said}: {replay:?}") };
            let [replayed] = replayed.as_slice() else { panic!("{said}: {replayed:?}") };
            assert!(live.msgid.as_deref().is_some_and(crate::msgid::is_made_up), "{said}: {live:?}");
            assert_eq!((replayed.kind, &replayed.text, &replayed.msgid), (live.kind, &live.text, &live.msgid), "{said}");
        }
    }

    #[test]
    fn the_next_play_starts_at_the_newest_line_shown() {
        let (mut s, _) = zncd(Book::default());
        feed(&mut s, "@time=2026-10-22T10:05:30.500Z :bob!b@h PRIVMSG #test :last");
        let (_, out) = zncd(s.take_book());
        assert_eq!(plays(&out), ["PRIVMSG *playback :PLAY * 1792663530"]);
    }

    #[test]
    fn made_up_ids_are_never_sent() {
        let ask = Output::Send(Message::new("CHATHISTORY", ["AFTER", "#test", "msgid=hirc:0123456789abcdef", "5"]));
        assert!(!safe_to_send(&[ask]));
        let mark = Output::Send(Message::new("MARKREAD", ["#test", "msgid=hirc:0123456789abcdef"]));
        assert!(!safe_to_send(&[mark]));
        let tagged = Output::Send(Message { tags: vec![("+draft/reply".into(), Some("hirc:0123456789abcdef".into()))], ..Message::new("TAGMSG", ["#test"]) });
        assert!(!safe_to_send(&[tagged]));
        // Someone saying the words is not an id.
        assert!(safe_to_send(&[Output::Send(Message::new("PRIVMSG", ["#test", "msgid=hirc:0123456789abcdef"]))]));
        // A server's own id is fine.
        assert!(safe_to_send(&[Output::Send(Message::new("CHATHISTORY", ["AFTER", "#test", "msgid=m1", "5"]))]));
        // A book holding a made-up id asks by time instead.
        let mut book = Book::default();
        book.note("#test", false, &[Line { msgid: Some("hirc:0123456789abcdef".into()), time: "2026-10-22T10:00:00Z".parse().unwrap(), ..local_line() }]);
        let mut s = historied("", 50, book);
        assert_eq!(asked(&feed(&mut s, OUR_JOIN)).0, "CHATHISTORY AFTER #test timestamp=2026-10-22T10:00:00.000Z 20");
    }

    #[test]
    fn a_znc_replay_is_kept_as_one_run_and_live_lines_follow_it() {
        let (s, _) = zncd(Book::default());
        let mut s = s.keeping();
        feed(&mut s, ":irc.znc.in BATCH +p znc.in/playback #test");
        feed(&mut s, "@batch=p;time=2026-10-22T10:01:00.000Z :bob!b@h PRIVMSG #test :r1");
        feed(&mut s, "@batch=p;time=2026-10-22T10:02:00.000Z :bob!b@h PRIVMSG #test :r2");
        feed(&mut s, ":irc.znc.in BATCH -p");
        let kept = s.kept();
        let [run] = kept.as_slice() else { panic!("{kept:?}") };
        assert_eq!(texts(&run.lines), ["r1", "r2"]);
        assert_eq!((&run.below, &run.above), (&Below::Breaks, &Above::Leave));
        let r2 = run.lines[1].msgid.clone().expect("made up");
        feed(&mut s, "@time=2026-10-22T10:03:00.000Z :bob!b@h PRIVMSG #test :live");
        assert_eq!(s.kept()[0].below, Below::Joins(r2));
    }

    #[test]
    fn loose_replayed_lines_follow_on_from_each_other() {
        let (s, _) = zncd_with("server-time znc.in/playback", Book::default());
        let mut s = s.keeping();
        feed(&mut s, ":***!znc@znc.in PRIVMSG #test :Buffer Playback...");
        feed(&mut s, "@time=2026-10-22T10:01:00.000Z :bob!b@h PRIVMSG #test :l1");
        feed(&mut s, "@time=2026-10-22T10:02:00.000Z :bob!b@h PRIVMSG #test :l2");
        let kept = s.kept();
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert_eq!(kept[0].below, Below::Breaks);
        assert_eq!(kept[1].below, Below::Joins(kept[0].lines[0].msgid.clone().unwrap()));
    }

    #[test]
    fn a_services_replay_is_never_kept() {
        let mut book = Book::default();
        book.open("NickServ", true);
        let (s, _) = zncd(book);
        let mut s = s.keeping();
        feed(&mut s, ":irc.znc.in BATCH +n znc.in/playback NickServ");
        feed(&mut s, "@batch=n;time=2026-10-22T10:01:00.000Z :NickServ!s@services NOTICE alice :IDENTIFY ok");
        let shown = feed(&mut s, ":irc.znc.in BATCH -n");
        assert_eq!(histories(&shown).len(), 1, "shown in the open window");
        // But never cached.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let cache = cache::Shared::open(&path).unwrap();
        for e in s.kept() {
            cache.write(e);
        }
        assert!(cache.flush());
        let rows: i64 = rusqlite::Connection::open(&path).unwrap().query_row("SELECT count(*) FROM lines", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 0);
    }

    /// A line the cache can't keep is a hole. A replay after it never
    /// fills it: the next live line still starts a break.
    #[test]
    fn a_replay_never_fills_a_hole() {
        let (s, _) = zncd(Book::default());
        let mut s = s.keeping();
        // Untimed: no made-up id, so the cache can't keep it.
        feed(&mut s, ":bob!b@h PRIVMSG #test :untimed");
        feed(&mut s, ":znc.in BATCH +p znc.in/playback #test");
        feed(&mut s, "@batch=p;time=2026-10-22T10:01:00.000Z :bob!b@h PRIVMSG #test :r1");
        feed(&mut s, ":znc.in BATCH -p");
        // More of the replay still follows on from it.
        feed(&mut s, ":znc.in BATCH +q znc.in/playback #test");
        feed(&mut s, "@batch=q;time=2026-10-22T10:02:00.000Z :bob!b@h PRIVMSG #test :r2");
        feed(&mut s, ":znc.in BATCH -q");
        feed(&mut s, "@time=2026-10-22T10:03:00.000Z :bob!b@h PRIVMSG #test :live");
        let kept = s.kept();
        let [r1, r2, live] = kept.as_slice() else { panic!("{kept:?}") };
        assert_eq!((texts(&r1.lines), &r1.below), (vec!["r1"], &Below::Breaks));
        assert_eq!((texts(&r2.lines), &r2.below), (vec!["r2"], &Below::Joins(r1.lines[0].msgid.clone().unwrap())));
        assert_eq!((texts(&live.lines), &live.below), (vec!["live"], &Below::Breaks));
    }

    /// Pages before what is shown say so, so the store puts them before
    /// the lines they tie with in time. Others don't.
    #[test]
    fn only_pages_before_what_is_shown_are_older() {
        let older = |out: &[Output]| -> Vec<bool> {
            out.iter()
                .filter_map(|o| match o {
                    Output::Event(Event::History { lines, older, .. }) if !lines.is_empty() => Some(*older),
                    _ => None,
                })
                .collect()
        };
        let (mut s, _) = zncd(Book::default());
        feed(&mut s, ":znc.in BATCH +p znc.in/playback #test");
        feed(&mut s, "@batch=p;time=2026-10-22T10:01:00.000Z :bob!b@h PRIVMSG #test :r1");
        assert_eq!(older(&feed(&mut s, ":znc.in BATCH -p")), [false]);
        let cached = s.cached("#test", vec![Line { msgid: Some("c1".into()), ..local_line() }]);
        assert_eq!(older(&cached), [true]);
    }
}
