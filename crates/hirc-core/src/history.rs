//! CHATHISTORY: what to ask the server for, and what we already hold. Pure:
//! no sockets and no store. `Session` sends what a `Request` says. It keeps
//! a `Book` of the newest and oldest line it has seen per target, which the
//! network task carries from one connection to the next, so a reconnect
//! asks only for the gap and scrolling up asks only for what is older.

use crate::proto::Message;
use crate::store::Line;
use chrono::{DateTime, SecondsFormat, Utc};
use std::collections::{HashMap, HashSet, VecDeque};

/// The cap (its group holds the ratified `chathistory` too).
pub const CAP: &str = "draft/chathistory";
/// The batch type a page of history comes in.
pub const BATCH: &str = "chathistory";
/// The batch type a TARGETS reply comes in.
pub const TARGETS_BATCH: &str = "draft/chathistory-targets";
/// A TARGETS request's target: a name no nick or channel can have, so it
/// never collides with a buffer's request.
pub const TARGETS: &str = "*";
/// Lines in a page when the config sets none.
pub const DEFAULT_PAGE: usize = 100;
/// How far back TARGETS looks when nothing has been seen yet.
pub const DISCOVERY_DAYS: i64 = 7;
/// TARGETS requests per connection at most: a full answer asks on from
/// its last name, so a server always answering in full can't loop us.
pub const MAX_TARGETS_PAGES: usize = 10;
/// Live lines remembered per target (`Known::live`): far more than can
/// come live while one page is on its way.
pub const LIVE_KEPT: usize = 500;

/// The page size: the configured one (else `DEFAULT_PAGE`), never more
/// than the server's `CHATHISTORY` limit, and at least 1.
pub fn page_size(configured: Option<usize>, server: Option<usize>) -> usize {
    configured.unwrap_or(DEFAULT_PAGE).min(server.unwrap_or(usize::MAX)).max(1)
}

/// `timestamp=2026-10-22T10:00:00.000Z`, as CHATHISTORY and MARKREAD want.
pub fn timestamp(time: DateTime<Utc>) -> String {
    format!("timestamp={}", time.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// `timestamp=...` back to a time; `None` for `*` or anything else.
pub fn parse_timestamp(param: &str) -> Option<DateTime<Utc>> {
    let t = param.strip_prefix("timestamp=")?;
    DateTime::parse_from_rfc3339(t).ok().map(|t| t.with_timezone(&Utc))
}

/// A place in a target's history: a message by msgid, else by time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub msgid: Option<String>,
    pub time: DateTime<Utc>,
}

impl Anchor {
    /// A line's place. An empty msgid is no msgid: `msgid=` would name
    /// nothing, so the line is found by its time.
    pub fn of(line: &Line) -> Anchor {
        Anchor { msgid: line.msgid.clone().filter(|m| !m.is_empty()), time: line.time }
    }

    /// From a raw message's `time` and `msgid` tags; `None` without a
    /// usable time.
    pub fn from_tags(msg: &Message) -> Option<Anchor> {
        let time = DateTime::parse_from_rfc3339(msg.tag("time")?).ok()?.with_timezone(&Utc);
        Some(Anchor { msgid: msg.tag("msgid").filter(|m| !m.is_empty()).map(str::to_owned), time })
    }

    /// The same place by time alone, for a server that can't find the msgid.
    /// Coarser: many lines can share one millisecond, and a page from a
    /// time inside such a burst can skip or repeat some of them.
    pub fn by_time(&self) -> Anchor {
        Anchor { msgid: None, time: self.time }
    }

    /// The msgid, when the server gave it. A made-up id (`msgid::made_up`)
    /// is ours alone: no server knows it, and a page never holds it.
    pub fn server_msgid(&self) -> Option<&str> {
        self.msgid.as_deref().filter(|id| !crate::msgid::is_made_up(id))
    }

    /// `msgid=...` when there is a msgid the server gave, else
    /// `timestamp=...`: for a made-up id the server is asked by time.
    pub fn param(&self) -> String {
        match self.server_msgid() {
            Some(id) => format!("msgid={id}"),
            None => timestamp(self.time),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The newest page.
    Latest,
    /// The page before this place (scrolling up).
    Before(Anchor),
    /// The page after this place (the gap since we were last here).
    After(Anchor),
    /// The targets with activity between two times.
    Targets { from: DateTime<Utc>, to: DateTime<Utc> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The channel or nick, as the user knows it; `TARGETS` for TARGETS.
    pub target: String,
    pub kind: Kind,
    pub limit: usize,
}

impl Request {
    pub fn message(&self) -> Message {
        let limit = self.limit.to_string();
        let t = self.target.clone();
        let params: [String; 4] = match &self.kind {
            Kind::Latest => ["LATEST".into(), t, "*".into(), limit],
            Kind::Before(a) => ["BEFORE".into(), t, a.param(), limit],
            Kind::After(a) => ["AFTER".into(), t, a.param(), limit],
            Kind::Targets { from, to } => ["TARGETS".into(), timestamp(*from), timestamp(*to), limit],
        };
        Message::new("CHATHISTORY", params)
    }
}

/// What we hold of one target's history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Known {
    /// As first seen.
    pub name: String,
    /// A nick (a query), not a channel.
    pub query: bool,
    pub newest: Option<Anchor>,
    pub oldest: Option<Anchor>,
    /// The msgids of the newest lines shown live (`LIVE_KEPT` at most).
    /// A page or replay asked for before one came, or asked again after a
    /// drop, can bring it once more: that copy is not shown.
    pub live: Live,
}

/// Recent msgids: a set to look them up, and their order to forget the
/// oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Live {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl Live {
    fn push(&mut self, msgid: &str) {
        if !self.ids.insert(msgid.to_owned()) {
            return;
        }
        self.order.push_back(msgid.to_owned());
        if self.order.len() > LIVE_KEPT
            && let Some(old) = self.order.pop_front()
        {
            self.ids.remove(&old);
        }
    }

    pub fn contains(&self, msgid: &str) -> bool {
        self.ids.contains(msgid)
    }
}

/// Every target's `Known`, by lowercased name (ASCII case-insensitive, as
/// buffers are).
#[derive(Clone, Debug, Default)]
pub struct Book {
    known: HashMap<String, Known>,
    /// Every target with activity up to this time has been found (by
    /// TARGETS answered in full): where the next TARGETS may start.
    discovered: Option<DateTime<Utc>>,
}

impl Book {
    pub fn get(&self, name: &str) -> Option<&Known> {
        self.known.get(&name.to_ascii_lowercase())
    }

    pub fn newest(&self, name: &str) -> Option<&Anchor> {
        self.get(name).and_then(|k| k.newest.as_ref())
    }

    pub fn oldest(&self, name: &str) -> Option<&Anchor> {
        self.get(name).and_then(|k| k.oldest.as_ref())
    }

    fn entry(&mut self, name: &str, query: bool) -> &mut Known {
        let k = self.known.entry(name.to_ascii_lowercase()).or_insert_with(|| Known { name: name.to_owned(), ..Known::default() });
        k.query |= query;
        k
    }

    /// A window for `name` is open (a query the user opened, say), so it
    /// is backfilled on connect even before it has any lines.
    pub fn open(&mut self, name: &str, query: bool) {
        self.entry(name, query);
    }

    /// Lines shown for `name` widen what we hold. Lines without a msgid
    /// (ours waiting for the echo, hirc's own notes) are not the server's,
    /// so they are skipped. Many lines can share a millisecond, so a tie
    /// is settled by order: a live line, arriving last, is the newest; a
    /// replayed one may be from a page before, so it must be newer.
    pub fn note(&mut self, name: &str, query: bool, lines: &[Line]) {
        let k = self.entry(name, query);
        for l in lines.iter().filter(|l| l.msgid.is_some()) {
            if k.newest.as_ref().is_none_or(|a| l.time > a.time || (!l.replayed && l.time == a.time)) {
                k.newest = Some(Anchor::of(l));
            }
            if k.oldest.as_ref().is_none_or(|a| l.time < a.time) {
                k.oldest = Some(Anchor::of(l));
            }
        }
    }

    /// A line with `msgid` was shown live for `name`.
    pub fn shown_live(&mut self, name: &str, query: bool, msgid: &str) {
        self.entry(name, query).live.push(msgid);
    }

    /// Whether a line with `msgid` was shown live for `name`.
    pub fn was_live(&self, name: &str, msgid: &str) -> bool {
        self.get(name).is_some_and(|k| k.live.contains(msgid))
    }

    /// A page's first item (a page before) or last item (a page after)
    /// widens what we hold, even when none of its items show as a line (a
    /// page of reactions), so the next page always starts further on. On
    /// a tie in time the page's end wins: it was asked for past what is
    /// held, so it is further on.
    pub fn reach(&mut self, name: &str, query: bool, first: Option<&Anchor>, last: Option<&Anchor>) {
        let k = self.entry(name, query);
        if let Some(first) = first
            && k.oldest.as_ref().is_none_or(|a| first.time <= a.time)
        {
            k.oldest = Some(first.clone());
        }
        if let Some(last) = last
            && k.newest.as_ref().is_none_or(|a| last.time >= a.time)
        {
            k.newest = Some(last.clone());
        }
    }

    /// Every query's name, sorted (so requests go out in a stable order).
    pub fn queries(&self) -> Vec<String> {
        let mut names: Vec<String> = self.known.values().filter(|k| k.query).map(|k| k.name.clone()).collect();
        names.sort();
        names
    }

    /// The newest time seen anywhere.
    pub fn last_seen(&self) -> Option<DateTime<Utc>> {
        self.known.values().filter_map(|k| k.newest.as_ref().map(|a| a.time)).max()
    }

    /// Up to when every target with activity has been found.
    pub fn discovered(&self) -> Option<DateTime<Utc>> {
        self.discovered
    }

    pub fn set_discovered(&mut self, time: DateTime<Utc>) {
        self.discovered = Some(time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Delivery, LineKind};

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn line(secs: i64, msgid: Option<&str>) -> Line {
        Line {
            seq: 0,
            time: at(secs),
            msgid: msgid.map(Into::into),
            kind: LineKind::Privmsg,
            nick: Some("bob".into()),
            text: "x".into(),
            own: false,
            delivery: Delivery::Sent,
            boundary: false,
            replayed: false,
            via: None,
            reply_to: None,
        }
    }

    #[test]
    fn live_msgids_are_forgotten_oldest_first() {
        let mut book = Book::default();
        for i in 0..=LIVE_KEPT {
            book.shown_live("#a", false, &format!("m{i}"));
        }
        // Seen twice, kept once: it doesn't push another out.
        book.shown_live("#a", false, "m5");
        assert!(!book.was_live("#a", "m0"), "the oldest went");
        assert!(book.was_live("#A", "m1") && book.was_live("#a", &format!("m{LIVE_KEPT}")));
        assert!(!book.was_live("#b", "m1"), "per target");
    }

    #[test]
    fn page_size_is_the_configs_capped_by_the_server() {
        assert_eq!(page_size(None, None), DEFAULT_PAGE);
        assert_eq!(page_size(Some(500), Some(100)), 100);
        assert_eq!(page_size(Some(20), Some(100)), 20);
        assert_eq!(page_size(None, Some(24)), 24);
        assert_eq!(page_size(Some(0), None), 1);
    }

    #[test]
    fn requests_on_the_wire() {
        let wire = |kind| Request { target: "#test".into(), kind, limit: 50 }.message().to_wire();
        assert_eq!(wire(Kind::Latest), "CHATHISTORY LATEST #test * 50");
        let a = Anchor { msgid: Some("m1".into()), time: "2026-10-22T10:00:00Z".parse().unwrap() };
        assert_eq!(wire(Kind::Before(a.clone())), "CHATHISTORY BEFORE #test msgid=m1 50");
        assert_eq!(wire(Kind::After(a.by_time())), "CHATHISTORY AFTER #test timestamp=2026-10-22T10:00:00.000Z 50");
        let targets = Kind::Targets { from: "2026-10-15T12:00:00Z".parse().unwrap(), to: "2026-10-22T13:00:00Z".parse().unwrap() };
        assert_eq!(wire(targets), "CHATHISTORY TARGETS timestamp=2026-10-15T12:00:00.000Z timestamp=2026-10-22T13:00:00.000Z 50");
    }

    #[test]
    fn a_made_up_msgid_is_never_an_anchor_on_the_wire() {
        let a = Anchor { msgid: Some("hirc:0123456789abcdef".into()), time: at(5) };
        assert_eq!(a.param(), timestamp(at(5)));
    }

    #[test]
    fn timestamps_round_trip_and_star_is_none() {
        let t: DateTime<Utc> = "2026-10-22T10:00:00.123Z".parse().unwrap();
        assert_eq!(parse_timestamp(&timestamp(t)), Some(t));
        assert_eq!(parse_timestamp("*"), None);
        assert_eq!(parse_timestamp("timestamp=garbage"), None);
    }

    #[test]
    fn anchors_come_from_tags() {
        let m = Message::parse("@time=2026-10-22T10:00:00.000Z;msgid=m9 :bob!b@h PRIVMSG #a :x").unwrap();
        assert_eq!(Anchor::from_tags(&m), Some(Anchor { msgid: Some("m9".into()), time: "2026-10-22T10:00:00Z".parse().unwrap() }));
        assert_eq!(Anchor::from_tags(&Message::parse("@msgid=m9 :bob!b@h PRIVMSG #a :x").unwrap()), None);
    }

    #[test]
    fn an_empty_msgid_is_never_an_anchor() {
        let a = Anchor::of(&line(5, Some("")));
        assert_eq!(a.msgid, None);
        assert_eq!(a.param(), timestamp(at(5)));
    }

    #[test]
    fn the_book_widens_with_what_is_shown() {
        let mut b = Book::default();
        b.note("#A", false, &[line(5, Some("m5")), line(9, None), line(3, Some("m3"))]);
        // Case-insensitive, and lines without a msgid (ours, waiting) don't count.
        assert_eq!(b.newest("#a").and_then(|a| a.msgid.as_deref()), Some("m5"));
        assert_eq!(b.oldest("#a").and_then(|a| a.msgid.as_deref()), Some("m3"));
        b.note("#a", false, &[line(1, Some("m1"))]);
        assert_eq!(b.oldest("#a").map(|a| a.time), Some(at(1)));
        assert_eq!(b.newest("#a").map(|a| a.time), Some(at(5)));
        // A page's raw ends widen it too (a page of reactions shows no lines).
        b.reach("#a", false, Some(&Anchor { msgid: Some("r0".into()), time: at(0) }), Some(&Anchor { msgid: Some("r7".into()), time: at(7) }));
        assert_eq!((b.oldest("#a").map(|a| a.time), b.newest("#a").map(|a| a.time)), (Some(at(0)), Some(at(7))));
        assert_eq!(b.get("#a").map(|k| k.name.as_str()), Some("#A"));
    }

    #[test]
    fn ties_in_time_go_to_the_page_not_the_replay() {
        // A burst inside one millisecond: only order tells the lines apart.
        let a = |id: &str| Anchor { msgid: Some(id.into()), time: at(5) };
        let mut b = Book::default();
        b.reach("#a", false, Some(&a("n100")), Some(&a("n199")));
        // The page before ends where it began: its first is now the oldest.
        b.reach("#a", false, Some(&a("n0")), None);
        assert_eq!(b.oldest("#a").and_then(|a| a.msgid.as_deref()), Some("n0"));
        assert_eq!(b.newest("#a").and_then(|a| a.msgid.as_deref()), Some("n199"));
        // Its lines are replayed: at the same time, they are not newer.
        let old = Line { replayed: true, ..line(5, Some("n99")) };
        b.note("#a", false, &[old]);
        assert_eq!(b.newest("#a").and_then(|a| a.msgid.as_deref()), Some("n199"));
        // A page after does move the newest on a tie, and so does a live line.
        b.reach("#a", false, None, Some(&a("n299")));
        assert_eq!(b.newest("#a").and_then(|a| a.msgid.as_deref()), Some("n299"));
        b.note("#a", false, &[line(5, Some("live"))]);
        assert_eq!(b.newest("#a").and_then(|a| a.msgid.as_deref()), Some("live"));
        assert_eq!(b.oldest("#a").and_then(|a| a.msgid.as_deref()), Some("n0"));
    }

    #[test]
    fn queries_and_last_seen() {
        let mut b = Book::default();
        assert_eq!(b.last_seen(), None);
        b.open("zoe", true);
        b.note("bob", true, &[line(4, Some("q4"))]);
        b.note("#a", false, &[line(8, Some("c8"))]);
        assert_eq!(b.queries(), ["bob", "zoe"]);
        assert_eq!(b.last_seen(), Some(at(8)));
        assert_eq!(b.newest("zoe"), None);
    }
}
