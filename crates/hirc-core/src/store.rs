//! The lines the UI shows, per network and buffer. Lines stay sorted by
//! server time (arrival order breaks ties), and a msgid seen twice is kept
//! once, so a live copy and a replayed copy of a message collapse. So is a
//! line that says what one held says, at the same millisecond, under
//! another msgid: a network whose history source changed (ZNC's made-up
//! ids, then soju's or Ergo's own) gives one line two.

use crate::event::{ConnStatus, Event, PeerChange, Target};
use crate::multiline::Limits;
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
    /// From history (a `chathistory` batch, or the cache), not said just
    /// now: never news, and unread only past the read marker.
    pub replayed: bool,
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

/// Where a buffer's older history stands. Scrolling to the top asks for more.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryState {
    #[default]
    Idle,
    /// A page of older lines is on its way.
    Loading,
    /// The server has nothing older.
    Start,
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
    /// For channels: who is in it, as the server last said
    /// (`Event::Members`).
    pub members: Vec<String>,
    /// For channels: are we in it right now.
    pub joined: bool,
    pub unread: usize,
    /// Unread lines meant for us: a message that names us, or anything
    /// said in a query (irssi's highlight activity).
    pub mentions: usize,
    /// Read up to here (draft/read-marker): the newest of the server's
    /// marker and what we last marked.
    pub read: Option<DateTime<Utc>>,
    /// The server has told this connection where its marker is. Until
    /// then hirc doesn't send its own, which could mark unread history
    /// read before the "new" divider shows.
    pub read_synced: bool,
    /// This connection's history for it is in (`Event::Filled`). Until
    /// then a marker sent could mark lines read that are still coming.
    pub filled: bool,
    pub history: HistoryState,
    msgids: HashSet<String>,
    /// What each line with a msgid says (`msgid::made_up` of it: its time,
    /// kind, nick and text), whatever its msgid.
    said: HashSet<String>,
}

impl Buffer {
    pub fn new(target: Target) -> Buffer {
        Buffer {
            target,
            lines: Vec::new(),
            topic: None,
            members: Vec::new(),
            joined: false,
            unread: 0,
            mentions: 0,
            read: None,
            read_synced: false,
            filled: false,
            history: HistoryState::Idle,
            msgids: HashSet::new(),
            said: HashSet::new(),
        }
    }

    /// Where the line went, or `None` if we already have it: its msgid,
    /// or (both with a msgid) a line that says the same at the same
    /// millisecond. Two such lines from one nick become one (accepted, as
    /// for made-up ids). A line without a msgid (ours, pending) is never
    /// matched by what it says.
    pub fn insert(&mut self, line: Line) -> Option<usize> {
        if let Some(id) = &line.msgid {
            let said = crate::msgid::made_up(&self.target, &line);
            if self.msgids.contains(id) || self.said.contains(&said) {
                return None;
            }
            self.msgids.insert(id.clone());
            self.said.insert(said);
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

    /// Insert `line`, and count it as unread (and a mention) when it is
    /// news. `None` when its msgid was already here.
    fn add_counted(&mut self, line: Line, me: Option<&str>) -> Option<usize> {
        let news = is_news(self, &line);
        let mention = news && meant_for(&self.target, &line, me);
        let ix = self.insert(line)?;
        if news {
            self.unread += 1;
        }
        if mention {
            self.mentions += 1;
        }
        Some(ix)
    }

    /// Count unread lines and mentions again from the read marker: every
    /// line past it that `counts`. Without a marker the counts stand.
    pub fn recount(&mut self, me: Option<&str>) {
        let Some(read) = self.read else { return };
        let fresh: Vec<&Line> = self.lines.iter().filter(|l| counts(l) && l.time > read).collect();
        self.mentions = fresh.iter().filter(|l| meant_for(&self.target, l, me)).count();
        self.unread = fresh.len();
    }
}

/// Whether `line` counts toward a window's unread lines: something someone
/// else said.
pub fn counts(line: &Line) -> bool {
    !line.own && matches!(line.kind, LineKind::Privmsg | LineKind::Action | LineKind::Notice)
}

/// Whether `line` adds to `buf`'s unread lines as it lands: anything that
/// `counts`, except history at or before the read marker (or with no
/// marker known), which the user has already had the chance to see.
pub fn is_news(buf: &Buffer, line: &Line) -> bool {
    counts(line) && (!line.replayed || buf.read.is_some_and(|r| line.time > r))
}

/// Whether `line` in `target` is meant for us: anything in a query, or a
/// message that names `me`.
fn meant_for(target: &Target, line: &Line, me: Option<&str>) -> bool {
    matches!(target, Target::Query(_)) || me.is_some_and(|me| crate::display::mentions(&line.text, me))
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
    /// What one `draft/multiline` batch may hold on this connection;
    /// `None` when the server takes none.
    pub multiline: Option<Limits>,
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
    /// A page of history went into one buffer: each line's index as it
    /// went in, in order (duplicates of lines already here are left out).
    History {
        network: usize,
        buffer: usize,
        at: Vec<usize>,
    },
    /// The read marker of one buffer changed.
    Read {
        network: usize,
        buffer: usize,
    },
}

/// The first seq a line arriving gets; older pages count down from it.
const ARRIVAL: u64 = u64::MAX / 4;

#[derive(Debug, Default)]
pub struct Store {
    pub networks: Vec<Network>,
    local_seq: u64,
    /// Numbers lines as they arrive, up from `ARRIVAL`. Sessions restart
    /// theirs on every connection and cached lines have none, so the
    /// store's are the ones the UI keys on (the "new" divider, the
    /// selection).
    arrival: u64,
    /// How many seqs below `ARRIVAL` older pages have taken: each one's
    /// lines come below every line before it, so on a tie in time they go
    /// before what was shown.
    floor: u64,
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
            multiline: None,
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
            replayed: false,
        };
        self.networks[network].buffers[buffer].insert(line).unwrap_or(0)
    }

    /// The user has read `(network, buffer)` up to `time` (hirc is about
    /// to tell the server): nothing there is unread now.
    pub fn mark_read(&mut self, network: usize, buffer: usize, time: DateTime<Utc>) {
        let buf = &mut self.networks[network].buffers[buffer];
        buf.read = Some(buf.read.map_or(time, |r| r.max(time)));
        buf.unread = 0;
        buf.mentions = 0;
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
                    net.multiline = None;
                    for b in &mut net.buffers {
                        b.joined = false;
                        // A request that was out is gone with the socket,
                        // and the next server will say its marker again.
                        b.read_synced = false;
                        b.filled = false;
                        if b.history == HistoryState::Loading {
                            b.history = HistoryState::Idle;
                        }
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
            Event::Multiline { network, limits } => {
                let n = self.network_ix(&network);
                self.networks[n].multiline = limits;
                Applied::Changed
            }
            Event::Peer { network, nick, change } => {
                let n = self.network_ix(&network);
                self.networks[n].see(&nick, change);
                Applied::Changed
            }
            Event::Filled { network, target } => {
                let n = self.network_ix(&network);
                let b = self.buffer_ix(n, &target);
                self.networks[n].buffers[b].filled = true;
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
            Event::Members { network, channel, nicks } => {
                let n = self.network_ix(&network);
                let b = self.buffer_ix(n, &Target::Channel(channel));
                self.networks[n].buffers[b].members = nicks;
                Applied::Changed
            }
            Event::Line { network, target, mut line } => {
                let n = self.network_ix(&network);
                let b = self.buffer_ix(n, &target);
                self.arrival += 1;
                line.seq = ARRIVAL + self.arrival;
                let net = &mut self.networks[n];
                let me = net.nick.as_deref();
                let buf = &mut net.buffers[b];
                if let Some(removed) = buf.pending_for(&line) {
                    buf.lines.remove(removed);
                    let index = buf.insert(line);
                    return Applied::Replaced { network: n, buffer: b, removed, index };
                }
                match buf.add_counted(line, me) {
                    Some(index) => Applied::Line { network: n, buffer: b, index },
                    None => Applied::Nothing,
                }
            }
            Event::History { network, target, lines, start, older } => {
                let n = self.network_ix(&network);
                let b = self.buffer_ix(n, &target);
                let net = &mut self.networks[n];
                let me = net.nick.as_deref();
                let buf = &mut net.buffers[b];
                let mut at = Vec::new();
                // An older page takes seqs below every line so far, in its
                // own order: on a tie in time it goes before them.
                let below = older.then(|| {
                    self.floor += lines.len() as u64;
                    ARRIVAL - self.floor
                });
                // Never matched against our pending lines: an old line of
                // ours with the same text is not the echo of a new one.
                for (i, mut line) in lines.into_iter().enumerate() {
                    line.seq = match below {
                        Some(base) => base + i as u64,
                        None => {
                            self.arrival += 1;
                            ARRIVAL + self.arrival
                        }
                    };
                    at.extend(buf.add_counted(line, me));
                }
                buf.history = match (start, buf.history) {
                    (true, _) => HistoryState::Start,
                    (false, HistoryState::Loading) => HistoryState::Idle,
                    (false, state) => state,
                };
                Applied::History { network: n, buffer: b, at }
            }
            Event::ReadMarker { network, target, time } => {
                let n = self.network_ix(&network);
                let b = self.buffer_ix(n, &target);
                let net = &mut self.networks[n];
                let me = net.nick.as_deref();
                let buf = &mut net.buffers[b];
                if let Some(t) = time {
                    buf.read = Some(buf.read.map_or(t, |r| r.max(t)));
                }
                buf.read_synced = true;
                buf.recount(me);
                Applied::Read { network: n, buffer: b }
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
            replayed: false,
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
    fn a_line_held_under_another_msgid_is_the_same_line() {
        // A network whose history source changes: ZNC's replay (made-up
        // ids), then soju's or Ergo's pages (their own), then ZNC again.
        let chan = Target::Channel("#a".into());
        let made_up = |l: Line| Line { msgid: Some(crate::msgid::made_up(&chan, &l)), ..l };
        let mut s = Store::new(["net".to_string()]);
        let page = |lines: Vec<Line>| Event::History { network: "net".into(), target: chan.clone(), lines, start: false, older: false };
        s.apply(page(vec![made_up(line(0, 1, None, "one")), made_up(line(0, 2, None, "two"))]));
        s.apply(page(vec![line(0, 1, Some("real1"), "one"), line(0, 2, Some("real2"), "two"), line(0, 3, Some("real3"), "three")]));
        s.apply(page(vec![made_up(line(0, 2, None, "two")), made_up(line(0, 3, None, "three")), made_up(line(0, 4, None, "four"))]));
        // And live, under the server's id again.
        s.apply(Event::Line { network: "net".into(), target: chan.clone(), line: line(0, 4, Some("real4"), "four") });
        let texts: Vec<_> = s.networks[0].buffers[1].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["one", "two", "three", "four"]);
        // Said again later, or by another nick, or another kind: new lines.
        let b = &mut s.networks[0].buffers[1];
        assert!(b.insert(line(0, 5, Some("real5"), "four")).is_some());
        assert!(b.insert(Line { nick: Some("carol".into()), ..line(0, 4, Some("real6"), "four") }).is_some());
        assert!(b.insert(Line { kind: LineKind::Notice, ..line(0, 4, Some("real7"), "four") }).is_some());
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
    fn members_are_kept_per_channel() {
        let mut s = Store::new(["net".to_string()]);
        s.apply(Event::Members { network: "net".into(), channel: "#a".into(), nicks: vec!["alice".into(), "bob".into()] });
        let b = s.buffer_ix(0, &Target::Channel("#A".into()));
        assert_eq!(s.networks[0].buffers[b].members, ["alice", "bob"]);
        s.apply(Event::Members { network: "net".into(), channel: "#a".into(), nicks: vec![] });
        assert!(s.networks[0].buffers[b].members.is_empty());
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

    fn old(seq: u64, secs: i64, msgid: &str, text: &str) -> Line {
        Line { replayed: true, ..line(seq, secs, Some(msgid), text) }
    }

    fn page(target: &str, lines: Vec<Line>, start: bool) -> Event {
        Event::History { network: "net".into(), target: Target::Channel(target.into()), lines, start, older: false }
    }

    #[test]
    fn history_goes_in_by_time_and_collapses_with_live_copies() {
        let mut s = Store::new(["net".to_string()]);
        let chan = || Target::Channel("#a".into());
        s.apply(Event::Line { network: "net".into(), target: chan(), line: line(1, 10, Some("live"), "live one") });
        let applied = s.apply(page("#a", vec![old(1, 5, "h1", "older"), old(2, 10, "live", "live one"), old(3, 12, "h3", "newer")], false));
        // "live" was already here; the others go in around it.
        assert_eq!(applied, Applied::History { network: 0, buffer: 1, at: vec![0, 2] });
        let texts: Vec<_> = s.networks[0].buffers[1].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["older", "live one", "newer"]);
        // Seqs are the store's own, so no two lines share one.
        let mut seqs: Vec<_> = s.networks[0].buffers[1].lines.iter().map(|l| l.seq).collect();
        seqs.sort();
        seqs.dedup();
        assert_eq!(seqs.len(), 3);
        // A live line is restamped too, not left with the session's seq.
        s.apply(Event::Line { network: "net".into(), target: chan(), line: line(1, 20, Some("l2"), "later") });
        let last = s.networks[0].buffers[1].lines.last().unwrap().seq;
        assert!(last > *seqs.last().unwrap());
    }

    #[test]
    fn replayed_lines_are_unread_only_past_the_read_marker() {
        let mut s = Store::new(["net".to_string()]);
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Connected { nick: "me".into() } });
        // No marker known: history is not news, even when it names us.
        s.apply(page("#a", vec![old(1, 1, "h1", "me: one"), old(2, 2, "h2", "two")], false));
        let b = s.buffer_ix(0, &Target::Channel("#a".into()));
        assert_eq!((s.networks[0].buffers[b].unread, s.networks[0].buffers[b].mentions), (0, 0));
        // The server's marker says the first was read: the rest counts.
        let at = s.networks[0].buffers[b].lines[0].time;
        assert_eq!(
            s.apply(Event::ReadMarker { network: "net".into(), target: Target::Channel("#a".into()), time: Some(at) }),
            Applied::Read { network: 0, buffer: b }
        );
        let buf = &s.networks[0].buffers[b];
        assert_eq!((buf.unread, buf.mentions, buf.read, buf.read_synced), (1, 0, Some(at), true));
        assert_eq!(buf.first_unread(), Some(1));
        // A later page past the marker counts as it lands, mentions too.
        s.apply(page("#a", vec![old(3, 3, "h3", "me: three")], false));
        let buf = &s.networks[0].buffers[b];
        assert_eq!((buf.unread, buf.mentions), (2, 1));
        // An older marker never moves it back.
        s.apply(Event::ReadMarker { network: "net".into(), target: Target::Channel("#a".into()), time: Some(at - chrono::TimeDelta::seconds(5)) });
        assert_eq!(s.networks[0].buffers[b].read, Some(at));
        // `*` (no marker yet) leaves the counts alone.
        s.apply(Event::ReadMarker { network: "net".into(), target: Target::Channel("#a".into()), time: None });
        assert_eq!(s.networks[0].buffers[b].unread, 2);
        // A replayed line exactly at the marker is read.
        s.apply(page("#a", vec![old(4, 1, "h4", "at the marker")], false));
        assert_eq!(s.networks[0].buffers[b].unread, 2);
        // A live line still counts once a marker exists.
        s.apply(Event::Line { network: "net".into(), target: Target::Channel("#a".into()), line: line(1, 30, Some("l"), "live") });
        assert_eq!(s.networks[0].buffers[b].unread, 3);
    }

    #[test]
    fn a_star_marker_still_marks_the_buffer_synced() {
        let mut s = Store::new(["net".to_string()]);
        let b = s.buffer_ix(0, &Target::Channel("#a".into()));
        s.apply(Event::ReadMarker { network: "net".into(), target: Target::Channel("#a".into()), time: None });
        let buf = &s.networks[0].buffers[b];
        assert_eq!((buf.read_synced, buf.read), (true, None));
    }

    #[test]
    fn marking_read_moves_the_marker_and_clears_the_counts() {
        let mut s = Store::new(["net".to_string()]);
        s.apply(Event::Line { network: "net".into(), target: Target::Query("bob".into()), line: line(1, 1, Some("q1"), "hi") });
        let b = s.buffer_ix(0, &Target::Query("bob".into()));
        assert_eq!(s.networks[0].buffers[b].mentions, 1);
        s.mark_read(0, b, at(1));
        let buf = &s.networks[0].buffers[b];
        assert_eq!((buf.read, buf.unread, buf.mentions), (Some(at(1)), 0, 0));
        s.mark_read(0, b, at(0));
        assert_eq!(s.networks[0].buffers[b].read, Some(at(1)));
    }

    #[test]
    fn a_page_ends_loading_and_an_empty_one_can_be_the_start() {
        let mut s = Store::new(["net".to_string()]);
        let b = s.buffer_ix(0, &Target::Channel("#a".into()));
        s.networks[0].buffers[b].history = HistoryState::Loading;
        s.apply(page("#a", vec![old(1, 1, "h1", "x")], false));
        assert_eq!(s.networks[0].buffers[b].history, HistoryState::Idle);
        s.apply(page("#a", vec![], true));
        assert_eq!(s.networks[0].buffers[b].history, HistoryState::Start);
        // A later page that is not the start leaves it there.
        s.apply(page("#a", vec![old(2, 2, "h2", "y")], false));
        assert_eq!(s.networks[0].buffers[b].history, HistoryState::Start);
    }

    #[test]
    fn a_drop_stops_loading_and_forgets_the_server_marker() {
        let mut s = Store::new(["net".to_string()]);
        let b = s.buffer_ix(0, &Target::Channel("#a".into()));
        s.apply(Event::ReadMarker { network: "net".into(), target: Target::Channel("#a".into()), time: Some(at(1)) });
        s.networks[0].buffers[b].history = HistoryState::Loading;
        s.apply(Event::Status { network: "net".into(), status: ConnStatus::Disconnected { reason: "x".into(), retry: std::time::Duration::from_secs(2) } });
        let buf = &s.networks[0].buffers[b];
        assert_eq!((buf.history, buf.read_synced, buf.read), (HistoryState::Idle, false, Some(at(1))));
    }

    fn older_page(target: &str, lines: Vec<Line>) -> Event {
        Event::History { network: "net".into(), target: Target::Channel(target.into()), lines, start: false, older: true }
    }

    /// A millisecond can hold many lines (a netsplit's QUITs, a bot, a
    /// burst replayed), and a page can end inside one. A page before what
    /// is shown arrives later but goes before every line it ties with, in
    /// its own order; a newer page and a live line still go after them.
    #[test]
    fn an_older_page_goes_before_the_lines_it_ties_with() {
        let mut s = Store::new(["net".to_string()]);
        let chan = || Target::Channel("#a".into());
        s.apply(page("#a", vec![old(0, 5, "n1", "n1"), old(0, 5, "n2", "n2")], false));
        s.apply(older_page("#a", vec![old(0, 5, "o1", "o1"), old(0, 5, "o2", "o2")]));
        s.apply(older_page("#a", vec![old(0, 5, "p1", "p1"), old(0, 5, "p2", "p2")]));
        s.apply(page("#a", vec![old(0, 5, "x", "x")], false));
        s.apply(Event::Line { network: "net".into(), target: chan(), line: line(0, 5, Some("y"), "y") });
        let b = s.buffer_ix(0, &chan());
        let texts: Vec<_> = s.networks[0].buffers[b].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["p1", "p2", "o1", "o2", "n1", "n2", "x", "y"]);
    }
}
