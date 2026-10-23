//! One server's protocol state machine. No sockets: a line goes in, and
//! lines to send plus events for the UI come out. All registration, CAP
//! and routing logic lives here so it can be unit-tested; `conn` just
//! moves bytes.

use crate::caps::Caps;
use crate::event::{Command, ConnStatus, Event, Target};
use crate::isupport::Isupport;
use crate::proto::Message;
use crate::sasl;
#[allow(unused_imports)]
use crate::split;
use crate::store::{Line, LineKind};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Send(Message),
    Event(Event),
    /// Close and don't reconnect until the user asks (bad password, /quit).
    Stop(String),
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
    /// Lowercased channel -> lowercased nicks, to route QUIT and NICK.
    members: HashMap<String, HashSet<String>>,
    seq: u64,
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
        }
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
        let mut out = Vec::new();
        if let Some(p) = &self.cfg.password {
            out.push(send("PASS", [p.as_str()]));
        }
        out.push(send("CAP", ["LS", "302"]));
        out.push(send("NICK", [self.nick.as_str()]));
        out.push(send("USER", [self.cfg.username.as_str(), "0", "*", self.cfg.realname.as_str()]));
        out
    }

    pub fn handle(&mut self, msg: Message, now: DateTime<Utc>) -> Vec<Output> {
        let mut out = Vec::new();
        match msg.command.as_str() {
            "PING" => out.push(send("PONG", msg.params.clone())),
            "CAP" => self.on_cap(&msg, &mut out),
            "AUTHENTICATE" => self.on_authenticate(&msg, &mut out),
            "903" | "907" if self.reg == Reg::Sasl => self.end_cap(&mut out),
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
            // Task 12 adds PRIVMSG, NOTICE, JOIN, PART, KICK, QUIT, NICK,
            // TOPIC, 332, 353, MODE, ERROR and the numeric fallback here.
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
        Line { seq: self.seq, time, msgid: msg.tag("msgid").filter(|m| !m.is_empty()).map(str::to_owned), kind, nick: nick.map(str::to_owned), text, own }
    }

    fn event_line(&self, target: Target, line: Line) -> Output {
        Output::Event(Event::Line { network: self.network.clone(), target, line })
    }
}

impl Session {
    pub fn command(&mut self, _cmd: Command, _now: DateTime<Utc>) -> Vec<Output> {
        Vec::new()
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
}
