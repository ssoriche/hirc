//! The lines the UI shows, per network and buffer. Lines stay sorted by
//! server time (arrival order breaks ties), and a msgid seen twice is kept
//! once, so a live copy and a replayed copy of a message collapse.

use crate::event::{ConnStatus, Event, Target};
use chrono::{DateTime, Utc};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Privmsg,
    Notice,
    Action,
    Join,
    Part,
    Quit,
    Nick,
    Kick,
    Topic,
    Mode,
    Server,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// Arrival order, for ties on `time`.
    pub seq: u64,
    /// The `time` tag when there is one, else when it arrived.
    pub time: DateTime<Utc>,
    pub msgid: Option<String>,
    pub kind: LineKind,
    pub nick: Option<String>,
    /// The message, or ready-to-show text for joins, quits and the like.
    pub text: String,
    /// Sent by us.
    pub own: bool,
}

#[derive(Debug)]
pub struct Buffer {
    pub target: Target,
    pub lines: Vec<Line>,
    pub topic: Option<String>,
    /// For channels: are we in it right now.
    pub joined: bool,
    pub unread: usize,
    msgids: HashSet<String>,
}

impl Buffer {
    pub fn new(target: Target) -> Buffer {
        Buffer { target, lines: Vec::new(), topic: None, joined: false, unread: 0, msgids: HashSet::new() }
    }

    /// Where the line went, or `None` if we already have its msgid.
    pub fn insert(&mut self, line: Line) -> Option<usize> {
        if let Some(id) = &line.msgid
            && !self.msgids.insert(id.clone())
        {
            return None;
        }
        let ix = self.lines.partition_point(|l| (l.time, l.seq) <= (line.time, line.seq));
        self.lines.insert(ix, line);
        Some(ix)
    }
}

#[derive(Debug)]
pub struct Network {
    pub name: String,
    pub status: ConnStatus,
    pub nick: Option<String>,
    /// `buffers[0]` is the server buffer. Buffers are never removed, so
    /// indexes stay valid.
    pub buffers: Vec<Buffer>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Nothing,
    /// Something besides a line changed (status, topic, a new buffer).
    Changed,
    Line {
        network: usize,
        buffer: usize,
        index: usize,
    },
}

#[derive(Debug, Default)]
pub struct Store {
    pub networks: Vec<Network>,
    local_seq: u64,
}

impl Store {
    pub fn new(names: impl IntoIterator<Item = String>) -> Store {
        let mut s = Store::default();
        for n in names {
            s.network_ix(&n);
        }
        s
    }

    pub fn network_ix(&mut self, name: &str) -> usize {
        if let Some(i) = self.networks.iter().position(|n| n.name == name) {
            return i;
        }
        self.networks.push(Network { name: name.to_owned(), status: ConnStatus::Idle, nick: None, buffers: vec![Buffer::new(Target::Server)] });
        self.networks.len() - 1
    }

    pub fn buffer_ix(&mut self, network: usize, target: &Target) -> usize {
        let bufs = &mut self.networks[network].buffers;
        if let Some(i) = bufs.iter().position(|b| b.target.same(target)) {
            return i;
        }
        bufs.push(Buffer::new(target.clone()));
        bufs.len() - 1
    }

    /// A line hirc itself writes (errors, notices about input). Returns its index.
    pub fn push_local(&mut self, network: usize, buffer: usize, kind: LineKind, text: String) -> usize {
        self.local_seq += 1;
        let line = Line { seq: u64::MAX / 2 + self.local_seq, time: Utc::now(), msgid: None, kind, nick: None, text, own: true };
        self.networks[network].buffers[buffer].insert(line).unwrap_or(0)
    }

    pub fn apply(&mut self, ev: Event) -> Applied {
        match ev {
            Event::Status { network, status } => {
                let n = self.network_ix(&network);
                let reason = match &status {
                    ConnStatus::Connected { nick } => {
                        self.networks[n].nick = Some(nick.clone());
                        None
                    }
                    ConnStatus::Disconnected { reason } => Some(format!("Disconnected: {reason}")),
                    ConnStatus::Failed { reason } => Some(format!("Stopped: {reason}. Use /reconnect to try again.")),
                    _ => None,
                };
                if matches!(status, ConnStatus::Connecting | ConnStatus::Disconnected { .. } | ConnStatus::Failed { .. }) {
                    // The server forgets our channels with the connection.
                    for b in &mut self.networks[n].buffers {
                        b.joined = false;
                    }
                }
                self.networks[n].status = status;
                match reason {
                    Some(text) => Applied::Line { network: n, buffer: 0, index: self.push_local(n, 0, LineKind::Error, text) },
                    None => Applied::Changed,
                }
            }
            Event::OwnNick { network, nick } => {
                let n = self.network_ix(&network);
                self.networks[n].nick = Some(nick);
                Applied::Changed
            }
            Event::Joined { network, channel } => self.set_joined(&network, channel, true),
            Event::Parted { network, channel } => self.set_joined(&network, channel, false),
            Event::Topic { network, channel, topic } => {
                let n = self.network_ix(&network);
                let b = self.buffer_ix(n, &Target::Channel(channel));
                self.networks[n].buffers[b].topic = topic;
                Applied::Changed
            }
            Event::Line { network, target, line } => {
                let n = self.network_ix(&network);
                let b = self.buffer_ix(n, &target);
                let counts = !line.own && matches!(line.kind, LineKind::Privmsg | LineKind::Action | LineKind::Notice);
                let buf = &mut self.networks[n].buffers[b];
                match buf.insert(line) {
                    Some(index) => {
                        if counts {
                            buf.unread += 1;
                        }
                        Applied::Line { network: n, buffer: b, index }
                    }
                    None => Applied::Nothing,
                }
            }
        }
    }

    fn set_joined(&mut self, network: &str, channel: String, joined: bool) -> Applied {
        let n = self.network_ix(network);
        let b = self.buffer_ix(n, &Target::Channel(channel));
        self.networks[n].buffers[b].joined = joined;
        Applied::Changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn line(seq: u64, secs: i64, msgid: Option<&str>, text: &str) -> Line {
        Line { seq, time: at(secs), msgid: msgid.map(Into::into), kind: LineKind::Privmsg, nick: Some("bob".into()), text: text.into(), own: false }
    }

    #[test]
    fn orders_by_server_time_then_arrival() {
        let mut b = Buffer::new(Target::Channel("#a".into()));
        b.insert(line(1, 10, None, "late"));
        b.insert(line(2, 5, None, "early"));
        b.insert(line(3, 10, None, "late-tie"));
        let texts: Vec<_> = b.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["early", "late", "late-tie"]);
    }

    #[test]
    fn duplicate_msgid_collapses() {
        let mut b = Buffer::new(Target::Channel("#a".into()));
        assert_eq!(b.insert(line(1, 1, Some("x"), "one")), Some(0));
        assert_eq!(b.insert(line(2, 1, Some("x"), "again")), None);
        assert_eq!(b.lines.len(), 1);
    }

    #[test]
    fn apply_routes_lines_creates_buffers_and_counts_unread() {
        let mut s = Store::new(["net".to_string()]);
        let applied = s.apply(Event::Line { network: "net".into(), target: Target::Channel("#A".into()), line: line(1, 1, None, "hi") });
        assert_eq!(applied, Applied::Line { network: 0, buffer: 1, index: 0 });
        // Same channel, different case: same buffer.
        s.apply(Event::Line { network: "net".into(), target: Target::Channel("#a".into()), line: line(2, 2, None, "again") });
        assert_eq!(s.networks[0].buffers.len(), 2);
        assert_eq!(s.networks[0].buffers[1].unread, 2);
        let mut own = line(3, 3, None, "mine");
        own.own = true;
        s.apply(Event::Line { network: "net".into(), target: Target::Channel("#a".into()), line: own });
        assert_eq!(s.networks[0].buffers[1].unread, 2);
    }

    #[test]
    fn status_and_membership_events() {
        let mut s = Store::new(["net".to_string()]);
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Connected { nick: "alice".into() } });
        assert_eq!(s.networks[0].nick.as_deref(), Some("alice"));
        s.apply(Event::Joined { network: "net".into(), channel: "#a".into() });
        assert!(s.networks[0].buffers[1].joined);
        s.apply(Event::Topic { network: "net".into(), channel: "#a".into(), topic: Some("t".into()) });
        assert_eq!(s.networks[0].buffers[1].topic.as_deref(), Some("t"));
        s.apply(Event::Parted { network: "net".into(), channel: "#a".into() });
        assert!(!s.networks[0].buffers[1].joined);
    }

    #[test]
    fn disconnect_reason_lands_in_server_buffer() {
        let mut s = Store::new(["net".to_string()]);
        let applied = s.apply(Event::Status { network: "net".into(), status: ConnStatus::Failed { reason: "SASL failed".into() } });
        assert_eq!(applied, Applied::Line { network: 0, buffer: 0, index: 0 });
        assert_eq!(s.networks[0].buffers[0].lines[0].kind, LineKind::Error);
        assert!(s.networks[0].buffers[0].lines[0].text.contains("SASL failed"));
    }

    #[test]
    fn disconnect_clears_joined_flags() {
        let mut s = Store::new(["net".to_string()]);
        s.apply(Event::Joined { network: "net".into(), channel: "#a".into() });
        assert!(s.networks[0].buffers[1].joined);
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Disconnected { reason: "x".into() } });
        assert!(!s.networks[0].buffers[1].joined);
    }
}
