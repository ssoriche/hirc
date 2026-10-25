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
    /// Unread lines meant for us: a message that names us, or anything
    /// said in a query (irssi's highlight activity).
    pub mentions: usize,
    msgids: HashSet<String>,
}

impl Buffer {
    pub fn new(target: Target) -> Buffer {
        Buffer { target, lines: Vec::new(), topic: None, joined: false, unread: 0, mentions: 0, msgids: HashSet::new() }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    Nothing,
    /// Something besides a line changed (status, topic, a new buffer).
    Changed,
    Line {
        network: usize,
        buffer: usize,
        index: usize,
    },
    /// The same line went into several buffers of one network: each
    /// `(buffer, index)`.
    Lines {
        network: usize,
        at: Vec<(usize, usize)>,
    },
}

#[derive(Debug, Default)]
pub struct Store {
    pub networks: Vec<Network>,
    local_seq: u64,
    /// The time lines hirc writes itself are stamped with; `None` is the
    /// wall clock. Fixtures set it so their screenshots keep one clock.
    pub clock: Option<DateTime<Utc>>,
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
        let line = Line { seq: u64::MAX / 2 + self.local_seq, time: self.clock.unwrap_or_else(Utc::now), msgid: None, kind, nick: None, text, own: true };
        self.networks[network].buffers[buffer].insert(line).unwrap_or(0)
    }

    pub fn apply(&mut self, ev: Event) -> Applied {
        match ev {
            Event::Status { network, status } => {
                let n = self.network_ix(&network);
                if let ConnStatus::Connected { nick } = &status {
                    self.networks[n].nick = Some(nick.clone());
                }
                let said = status_line(&network, &status);
                // Said where the user may be looking: the server buffer, every
                // channel we were in and every query (irssi does the same).
                let to: Vec<usize> = match said {
                    Some(_) => {
                        let bufs = &self.networks[n].buffers;
                        (0..bufs.len()).filter(|&b| b == 0 || bufs[b].joined || matches!(bufs[b].target, Target::Query(_))).collect()
                    }
                    None => Vec::new(),
                };
                if matches!(status, ConnStatus::Connecting) || status.is_down() {
                    // The server forgets our channels with the connection.
                    for b in &mut self.networks[n].buffers {
                        b.joined = false;
                    }
                }
                self.networks[n].status = status;
                match said {
                    Some((kind, text)) => Applied::Lines { network: n, at: to.into_iter().map(|b| (b, self.push_local(n, b, kind, text.clone()))).collect() },
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
                let net = &mut self.networks[n];
                let me = net.nick.as_deref();
                let mention = counts && (matches!(target, Target::Query(_)) || me.is_some_and(|me| crate::display::mentions(&line.text, me)));
                let buf = &mut net.buffers[b];
                match buf.insert(line) {
                    Some(index) => {
                        if counts {
                            buf.unread += 1;
                        }
                        if mention {
                            buf.mentions += 1;
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

/// What a change of connection state says, and how loud: a fault is an
/// error, a quit the user chose is a plain status line. Each says what
/// happens next and how to act on it.
pub fn status_line(network: &str, status: &ConnStatus) -> Option<(LineKind, String)> {
    match status {
        ConnStatus::Disconnected { reason, retry } => {
            Some((LineKind::Error, format!("Disconnected from {network}: {reason}. Reconnecting in {}s — /reconnect to retry now", retry.as_secs().max(1))))
        }
        ConnStatus::Failed { reason } => Some((LineKind::Error, format!("Disconnected from {network}: {reason}. Not retrying — /reconnect to try again"))),
        ConnStatus::Offline => Some((LineKind::Server, format!("Disconnected from {network} — /reconnect to connect again"))),
        _ => None,
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
        assert_eq!(s.networks[0].buffers[1].mentions, 0);
    }

    #[test]
    fn mentions_count_lines_meant_for_us() {
        let mut s = Store::new(["net".to_string()]);
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Connected { nick: "me".into() } });
        let chan = || Target::Channel("#a".into());
        s.apply(Event::Line { network: "net".into(), target: chan(), line: line(1, 1, None, "hello all") });
        s.apply(Event::Line { network: "net".into(), target: chan(), line: line(2, 2, None, "Me: are you there?") });
        s.apply(Event::Line { network: "net".into(), target: chan(), line: line(3, 3, None, "meme time") });
        let b = s.buffer_ix(0, &chan());
        assert_eq!((s.networks[0].buffers[b].unread, s.networks[0].buffers[b].mentions), (3, 1));
        // Everything said in a query is for us.
        s.apply(Event::Line { network: "net".into(), target: Target::Query("bob".into()), line: line(4, 4, None, "psst") });
        let q = s.buffer_ix(0, &Target::Query("bob".into()));
        assert_eq!(s.networks[0].buffers[q].mentions, 1);
    }

    #[test]
    fn local_lines_use_the_store_clock() {
        let mut s = Store::new(["net".to_string()]);
        s.clock = Some(at(42));
        let i = s.push_local(0, 0, LineKind::Server, "note".into());
        assert_eq!(s.networks[0].buffers[0].lines[i].time, at(42));
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
        assert_eq!(applied, Applied::Lines { network: 0, at: vec![(0, 0)] });
        assert_eq!(s.networks[0].buffers[0].lines[0].kind, LineKind::Error);
        assert!(s.networks[0].buffers[0].lines[0].text.contains("SASL failed"));
    }

    #[test]
    fn disconnect_clears_joined_flags() {
        let mut s = Store::new(["net".to_string()]);
        s.apply(Event::Joined { network: "net".into(), channel: "#a".into() });
        assert!(s.networks[0].buffers[1].joined);
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Disconnected { reason: "x".into(), retry: std::time::Duration::from_secs(2) } });
        assert!(!s.networks[0].buffers[1].joined);
    }

    #[test]
    fn a_drop_is_said_in_every_buffer_we_were_using() {
        let mut s = Store::new(["net".to_string()]);
        s.apply(Event::Joined { network: "net".into(), channel: "#in".into() });
        s.apply(Event::Joined { network: "net".into(), channel: "#out".into() });
        s.apply(Event::Parted { network: "net".into(), channel: "#out".into() });
        s.buffer_ix(0, &Target::Query("bob".into()));
        let applied =
            s.apply(Event::Status { network: "net".into(), status: ConnStatus::Disconnected { reason: "x".into(), retry: std::time::Duration::from_secs(2) } });
        // Server, #in and the query; not #out, which we had left.
        assert_eq!(applied, Applied::Lines { network: 0, at: vec![(0, 0), (1, 0), (3, 0)] });
        assert_eq!(s.networks[0].buffers[1].lines[0].text, "Disconnected from net: x. Reconnecting in 2s — /reconnect to retry now");
    }

    #[test]
    fn a_quit_is_not_an_error() {
        let (kind, text) = status_line("net", &ConnStatus::Offline).unwrap();
        assert_eq!((kind, text.as_str()), (LineKind::Server, "Disconnected from net — /reconnect to connect again"));
        let (kind, _) = status_line("net", &ConnStatus::Failed { reason: "SASL failed".into() }).unwrap();
        assert_eq!(kind, LineKind::Error);
        assert_eq!(status_line("net", &ConnStatus::Connecting), None);
    }
}
