//! The lines the UI shows, per network and buffer. Lines stay sorted by
//! server time (arrival order breaks ties), and a msgid seen twice is kept
//! once, so a live copy and a replayed copy of a message collapse.

use crate::event::{ConnStatus, Event, PeerChange, Target};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

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
    /// A change of connection state the user chose (a quit): status, not
    /// server chatter and not a fault.
    Status,
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
    /// For a line we sent: whether the server has it yet.
    pub delivery: Delivery,
    /// A change of connection state (a drop, a quit, a failed connect):
    /// the end of one session's lines and the start of the next.
    pub boundary: bool,
}

/// Where a line we sent stands. Everything else is `Sent`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    #[default]
    Sent,
    /// Shown the moment it was sent, waiting for the server's echo
    /// (echo-message), which takes its place.
    Pending,
    /// The connection dropped before the echo came: it may never have
    /// gone out.
    Unsent,
}

/// What we know of another user on a network, for a query's header.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peer {
    pub presence: Presence,
    /// Their away message, while they are marked away.
    pub away: Option<String>,
    /// `user@host`, from the last message that carried it.
    pub host: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Presence {
    /// Nothing seen since we connected.
    #[default]
    Unknown,
    Online,
    /// Quit the network, with their reason (maybe empty).
    Quit(String),
    /// Changed nick: now this one.
    Renamed(String),
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

    /// The pending line our echo `line` confirms: our own line of the same
    /// kind and text still waiting (the oldest, if several).
    fn pending_for(&self, line: &Line) -> Option<usize> {
        if !line.own || line.delivery != Delivery::Sent {
            return None;
        }
        self.lines.iter().position(|l| l.delivery != Delivery::Sent && l.kind == line.kind && l.text == line.text)
    }

    /// Where the unread lines begin: the first of the last `unread`
    /// lines that count as unread (see `counts`). `None` when nothing is
    /// unread, or when every line is, since a marker above them all
    /// would say nothing.
    pub fn first_unread(&self) -> Option<usize> {
        if self.unread == 0 {
            return None;
        }
        let mut left = self.unread;
        let ix = self.lines.iter().rposition(|l| {
            if counts(l) {
                left -= 1;
            }
            left == 0
        })?;
        (ix > 0).then_some(ix)
    }
}

/// Whether `line` counts toward a window's unread lines: something someone
/// else said.
pub fn counts(line: &Line) -> bool {
    !line.own && matches!(line.kind, LineKind::Privmsg | LineKind::Action | LineKind::Notice)
}

#[derive(Debug)]
pub struct Network {
    pub name: String,
    pub status: ConnStatus,
    /// When `status` last changed (by the store's clock): a retry's
    /// countdown runs from here.
    pub since: Option<DateTime<Utc>>,
    pub nick: Option<String>,
    /// `buffers[0]` is the server buffer. Buffers are never removed, so
    /// indexes stay valid.
    pub buffers: Vec<Buffer>,
    /// Other users, by lowercased nick: what this connection has seen of
    /// them.
    pub peers: HashMap<String, Peer>,
}

impl Network {
    /// Whole seconds until a dropped connection is retried, at `now`
    /// (rounded up; 0 once the retry is due). `None` unless retrying.
    pub fn retry_in(&self, now: DateTime<Utc>) -> Option<u64> {
        let ConnStatus::Disconnected { retry, .. } = &self.status else { return None };
        let since = self.since.unwrap_or(now);
        let left = retry.as_millis() as i64 - (now - since).num_milliseconds();
        Some((left.max(0) as u64).div_ceil(1000))
    }

    /// What we know of `nick` (ASCII case-insensitive).
    pub fn peer(&self, nick: &str) -> Option<&Peer> {
        self.peers.get(&nick.to_ascii_lowercase())
    }

    fn see(&mut self, nick: &str, change: PeerChange) {
        let key = nick.to_ascii_lowercase();
        match change {
            PeerChange::Seen { host } => {
                let peer = self.peers.entry(key).or_default();
                peer.presence = Presence::Online;
                if host.is_some() {
                    peer.host = host;
                }
            }
            PeerChange::Away(away) => {
                let peer = self.peers.entry(key).or_default();
                peer.presence = Presence::Online;
                peer.away = away;
            }
            PeerChange::Quit(reason) => {
                let peer = self.peers.entry(key).or_default();
                peer.presence = Presence::Quit(reason);
                peer.away = None;
            }
            PeerChange::Renamed(new) => {
                let old = self.peers.insert(key, Peer { presence: Presence::Renamed(new.clone()), ..Peer::default() }).unwrap_or_default();
                self.peers.insert(new.to_ascii_lowercase(), Peer { presence: Presence::Online, ..old });
            }
        }
    }
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
    /// The server's echo took the place of our pending line: the line at
    /// `removed` went, then the echo went in at `index` (`None` when its
    /// msgid was already here).
    Replaced {
        network: usize,
        buffer: usize,
        removed: usize,
        index: Option<usize>,
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
        self.networks.push(Network {
            name: name.to_owned(),
            status: ConnStatus::Idle,
            since: None,
            nick: None,
            buffers: vec![Buffer::new(Target::Server)],
            peers: HashMap::new(),
        });
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
        let line = Line {
            seq: u64::MAX / 2 + self.local_seq,
            time: self.clock.unwrap_or_else(Utc::now),
            msgid: None,
            kind,
            nick: None,
            text,
            own: true,
            delivery: Delivery::Sent,
            boundary: false,
        };
        self.networks[network].buffers[buffer].insert(line).unwrap_or(0)
    }

    /// `push_local`, for a change of connection state (see `Line::boundary`).
    fn push_boundary(&mut self, network: usize, buffer: usize, kind: LineKind, text: String) -> usize {
        let ix = self.push_local(network, buffer, kind, text);
        if let Some(line) = self.networks[network].buffers[buffer].lines.get_mut(ix) {
            line.boundary = true;
        }
        ix
    }

    pub fn apply(&mut self, ev: Event) -> Applied {
        match ev {
            Event::Status { network, status } => {
                let n = self.network_ix(&network);
                if let ConnStatus::Connected { nick } = &status {
                    self.networks[n].nick = Some(nick.clone());
                }
                // The server buffer is the network's own: its line leaves
                // the name out (the header names it).
                let said = status_line(&network, &status, false);
                let said_here = status_line(&network, &status, true);
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
                    // The server forgets our channels with the connection,
                    // and what we saw of others is no longer news.
                    let net = &mut self.networks[n];
                    net.peers.clear();
                    for b in &mut net.buffers {
                        b.joined = false;
                    }
                }
                if status.is_down() {
                    // An echo that has not come by now never will: say so.
                    for l in self.networks[n].buffers.iter_mut().flat_map(|b| b.lines.iter_mut()) {
                        if l.delivery == Delivery::Pending {
                            l.delivery = Delivery::Unsent;
                        }
                    }
                }
                self.networks[n].status = status;
                self.networks[n].since = Some(self.clock.unwrap_or_else(Utc::now));
                match (said, said_here) {
                    (Some((kind, text)), Some((_, here))) => Applied::Lines {
                        network: n,
                        at: to.into_iter().map(|b| (b, self.push_boundary(n, b, kind, if b == 0 { here.clone() } else { text.clone() }))).collect(),
                    },
                    _ => Applied::Changed,
                }
            }
            Event::OwnNick { network, nick } => {
                let n = self.network_ix(&network);
                self.networks[n].nick = Some(nick);
                Applied::Changed
            }
            Event::Peer { network, nick, change } => {
                let n = self.network_ix(&network);
                self.networks[n].see(&nick, change);
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
                let counts = counts(&line);
                let net = &mut self.networks[n];
                let me = net.nick.as_deref();
                let mention = counts && (matches!(target, Target::Query(_)) || me.is_some_and(|me| crate::display::mentions(&line.text, me)));
                let buf = &mut net.buffers[b];
                if let Some(removed) = buf.pending_for(&line) {
                    buf.lines.remove(removed);
                    let index = buf.insert(line);
                    return Applied::Replaced { network: n, buffer: b, removed, index };
                }
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
/// error, a quit the user chose is a plain status line. Scrollback keeps
/// only what happened and why, which stays true however late it is read;
/// what happens next (a countdown) and what to do live in the header and
/// the input line, which follow the state. In the network's own server
/// buffer (`server`) the name is left out, so the reason comes first.
pub fn status_line(network: &str, status: &ConnStatus, server: bool) -> Option<(LineKind, String)> {
    let from = |word: &str| if server { String::new() } else { format!(" {word} {network}") };
    match status {
        ConnStatus::Disconnected { reason, .. } => Some((LineKind::Error, format!("Disconnected{}: {reason}", from("from")))),
        ConnStatus::Failed { reason } => Some((LineKind::Error, format!("Could not connect{}: {reason}", from("to")))),
        ConnStatus::Offline => Some((LineKind::Status, format!("Disconnected{} (you quit)", from("from")))),
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
        Line {
            seq,
            time: at(secs),
            msgid: msgid.map(Into::into),
            kind: LineKind::Privmsg,
            nick: Some("bob".into()),
            text: text.into(),
            own: false,
            delivery: Delivery::Sent,
            boundary: false,
        }
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
        assert_eq!(s.networks[0].buffers[1].lines[0].text, "Disconnected from net: x");
        assert_eq!(s.networks[0].buffers[0].lines[0].text, "Disconnected: x");
        // Each copy marks where the session ended.
        assert!(s.networks[0].buffers.iter().filter(|b| !b.lines.is_empty()).all(|b| b.lines[0].boundary));
    }

    #[test]
    fn unread_lines_begin_after_the_read_ones() {
        let mut b = Buffer::new(Target::Channel("#a".into()));
        for (i, text) in ["one", "two", "three", "four"].iter().enumerate() {
            b.insert(line(i as u64, i as i64, None, text));
        }
        let mut own = line(9, 9, None, "mine");
        own.own = true;
        b.insert(own);
        assert_eq!(b.first_unread(), None);
        b.unread = 2;
        // The last two counted lines ("three", "four"); our own line is not one.
        assert_eq!(b.first_unread(), Some(2));
        // Everything unread: no marker.
        b.unread = 4;
        assert_eq!(b.first_unread(), None);
    }

    #[test]
    fn a_retry_counts_down_from_the_status_change() {
        let mut s = Store::new(["net".to_string()]);
        s.clock = Some(at(0));
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Disconnected { reason: "x".into(), retry: std::time::Duration::from_secs(42) } });
        let net = &s.networks[0];
        assert_eq!(net.retry_in(at(0)), Some(42));
        assert_eq!(net.retry_in(at(40)), Some(2));
        assert_eq!(net.retry_in(at(60)), Some(0));
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Offline });
        assert_eq!(s.networks[0].retry_in(at(0)), None);
    }

    #[test]
    fn a_quit_is_not_an_error() {
        let (kind, text) = status_line("net", &ConnStatus::Offline, false).unwrap();
        assert_eq!((kind, text.as_str()), (LineKind::Status, "Disconnected from net (you quit)"));
        let (kind, _) = status_line("net", &ConnStatus::Failed { reason: "SASL failed".into() }, false).unwrap();
        assert_eq!(kind, LineKind::Error);
        assert_eq!(status_line("net", &ConnStatus::Connecting, false), None);
        // The server buffer leaves its own name out.
        assert_eq!(status_line("net", &ConnStatus::Offline, true).unwrap().1, "Disconnected (you quit)");
        let drop = ConnStatus::Disconnected { reason: "Ping timeout".into(), retry: std::time::Duration::from_secs(2) };
        assert_eq!(status_line("net", &drop, true).unwrap().1, "Disconnected: Ping timeout");
    }

    fn mine(seq: u64, secs: i64, msgid: Option<&str>, text: &str, delivery: Delivery) -> Line {
        Line { own: true, nick: Some("me".into()), delivery, ..line(seq, secs, msgid, text) }
    }

    #[test]
    fn the_echo_takes_the_pending_lines_place() {
        let mut s = Store::new(["net".to_string()]);
        let chan = || Target::Channel("#a".into());
        let ev = |line| Event::Line { network: "net".into(), target: chan(), line };
        s.apply(ev(line(1, 1, None, "before")));
        assert_eq!(s.apply(ev(mine(2, 2, None, "hi", Delivery::Pending))), Applied::Line { network: 0, buffer: 1, index: 1 });
        s.apply(ev(mine(3, 2, None, "again", Delivery::Pending)));
        // The echo of "hi" replaces it; "again" still waits.
        assert_eq!(s.apply(ev(mine(9, 3, Some("m1"), "hi", Delivery::Sent))), Applied::Replaced { network: 0, buffer: 1, removed: 1, index: Some(2) });
        let lines: Vec<_> = s.networks[0].buffers[1].lines.iter().map(|l| (l.text.as_str(), l.delivery)).collect();
        assert_eq!(lines, [("before", Delivery::Sent), ("again", Delivery::Pending), ("hi", Delivery::Sent)]);
        // Someone else saying the same is no echo.
        assert!(matches!(s.apply(ev(line(10, 4, None, "again"))), Applied::Line { .. }));
        // A drop leaves what never came back marked.
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Disconnected { reason: "x".into(), retry: std::time::Duration::from_secs(2) } });
        assert_eq!(s.networks[0].buffers[1].lines.iter().find(|l| l.text == "again" && l.own).unwrap().delivery, Delivery::Unsent);
    }

    #[test]
    fn peers_follow_what_the_session_saw() {
        let mut s = Store::new(["net".to_string()]);
        let peer = |s: &mut Store, nick: &str, change| s.apply(Event::Peer { network: "net".into(), nick: nick.into(), change });
        peer(&mut s, "Carol", PeerChange::Seen { host: Some("c@h".into()) });
        peer(&mut s, "carol", PeerChange::Away(Some("lunch".into())));
        let carol = s.networks[0].peer("CAROL").unwrap().clone();
        assert_eq!(carol, Peer { presence: Presence::Online, away: Some("lunch".into()), host: Some("c@h".into()) });
        // Seen again (no host on a NAMES reply): still away, host kept.
        peer(&mut s, "carol", PeerChange::Seen { host: None });
        assert_eq!(s.networks[0].peer("carol"), Some(&carol));
        peer(&mut s, "carol", PeerChange::Renamed("carla".into()));
        assert_eq!(s.networks[0].peer("carol").unwrap().presence, Presence::Renamed("carla".into()));
        assert_eq!(s.networks[0].peer("carla").unwrap().away.as_deref(), Some("lunch"));
        peer(&mut s, "carla", PeerChange::Quit("bye".into()));
        assert_eq!(s.networks[0].peer("carla").unwrap(), &Peer { presence: Presence::Quit("bye".into()), away: None, host: Some("c@h".into()) });
        // A new connection knows no one.
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Connecting });
        assert_eq!(s.networks[0].peer("carla"), None);
    }
}
