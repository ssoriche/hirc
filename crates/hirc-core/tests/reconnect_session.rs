//! No line lost or shown twice across a reconnect, at the session and store
//! level: scripted server lines go through a `Session` (as `conn` does), its
//! events into a `Store` (as the UI does), and the buffer is read back. No
//! sockets, no sleeps.
//!
//! The harness plays `conn`'s part: a drop is `Session::end`, a `Disconnected`
//! status, and `take_book` carried to the next `Session`.

use chrono::{DateTime, Utc};
use hirc_core::cache::{Reach, Shared};
use hirc_core::event::{ConnStatus, Event, Target};
use hirc_core::history::{Anchor, Book};
use hirc_core::proto::Message;
use hirc_core::session::{Output, Session, SessionConfig};
use hirc_core::store::{Buffer, Delivery, Line, LineKind, Store};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const CHAN: &str = "#test";

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// The wall clock of a connection: the same for every line, like the unit tests.
fn noon() -> DateTime<Utc> {
    at("2026-10-22T12:00:00Z")
}

/// The time tag of the line `sec` seconds and `ms` milliseconds past 10:00.
fn stamp(sec: u32, ms: u32) -> String {
    format!("2026-10-22T10:{:02}:{:02}.{ms:03}Z", sec / 60, sec % 60)
}

/// One line of a scripted server's history: msgid, time, text.
#[derive(Clone, Copy)]
struct L {
    id: &'static str,
    sec: u32,
    ms: u32,
    text: &'static str,
}

/// A line whose text is its msgid, `sec` seconds past 10:00.
fn l(id: &'static str, sec: u32) -> L {
    L { id, sec, ms: 0, text: id }
}

/// A line at a given millisecond, with its own text.
fn lm(id: &'static str, sec: u32, ms: u32, text: &'static str) -> L {
    L { id, sec, ms, text }
}

impl L {
    fn tags(&self) -> String {
        format!("time={};msgid={}", stamp(self.sec, self.ms), self.id)
    }

    fn privmsg(&self) -> String {
        format!(":bob!b@h PRIVMSG {CHAN} :{}", self.text)
    }
}

/// The CHATHISTORY lines in `out`, as sent (labels and all).
fn history_requests(out: &[Output]) -> Vec<Message> {
    out.iter()
        .filter_map(|o| match o {
            Output::Send(m) if m.command == "CHATHISTORY" && m.params.first().map(String::as_str) != Some("TARGETS") => Some(m.clone()),
            _ => None,
        })
        .collect()
}

fn bare(m: &Message) -> String {
    Message { tags: m.tags.iter().filter(|(k, _)| k != "label").cloned().collect(), ..m.clone() }.to_wire()
}

/// A session and the store its events land in.
struct Client {
    s: Session,
    store: Store,
    /// What this connection asked for, without labels, in order.
    asked: Vec<String>,
    /// The label of the request still out, if any.
    label: Option<String>,
    batches: u32,
    clock: DateTime<Utc>,
}

impl Client {
    /// A session registered at `clock` with chathistory (and `extra` caps),
    /// a page of 20 asked for, and the server's `CHATHISTORY=limit`. `book`
    /// is carried in; the store is the UI's, kept from before.
    fn connect(mut store: Store, book: Book, limit: usize, extra: &str, keep: bool) -> Client {
        let cfg = SessionConfig {
            nickname: "alice".into(),
            alt_nicks: vec!["alice2".into()],
            username: "alice".into(),
            realname: "Alice".into(),
            password: None,
            sasl: None,
            channels: vec![CHAN.into()],
            bridges: vec![],
            chathistory: true,
            history_page: Some(20),
            self_replays: false,
        };
        let mut s = Session::new("net", cfg).with_book(book);
        if keep {
            s = s.keeping();
        }
        store.apply(Event::Status { network: "net".into(), status: ConnStatus::Connecting });
        let mut c = Client { s, store, asked: Vec::new(), label: None, batches: 0, clock: noon() };
        let out = c.s.start();
        c.take(out);
        let ls = format!("message-tags server-time echo-message batch labeled-response draft/chathistory {extra}");
        let ack: Vec<&str> = ls.split_whitespace().map(|cap| cap.split_once('=').map_or(cap, |(k, _)| k)).collect();
        c.feed(&format!(":srv CAP * LS :{}", ls.trim()));
        c.feed(&format!(":srv CAP alice ACK :{}", ack.join(" ")));
        c.feed(":srv 001 alice :hi");
        c.feed(&format!(":srv 005 alice CHANTYPES=# CHATHISTORY={limit} :are supported"));
        // The end of the MOTD asks for TARGETS: answer it with nothing, so
        // each test starts with nothing in flight.
        let out = c.feed(":srv 376 alice :End of MOTD");
        let targets = out.iter().find_map(|o| match o {
            Output::Send(m) if m.command == "CHATHISTORY" && m.params.first().map(String::as_str) == Some("TARGETS") => m.tag("label").map(str::to_owned),
            _ => None,
        });
        if let Some(label) = targets {
            c.feed(&format!("@label={label} :srv ACK"));
        }
        c.asked.clear();
        c
    }

    fn fresh(limit: usize) -> Client {
        Client::connect(Store::new(["net".to_string()]), Book::default(), limit, "", false)
    }

    /// The UI's side of `out`: events into the store. Remembers what was
    /// asked for. Gives `out` back.
    fn take(&mut self, out: Vec<Output>) -> Vec<Output> {
        for o in &out {
            if let Output::Event(e) = o {
                self.store.apply(e.clone());
            }
        }
        for m in history_requests(&out) {
            self.asked.push(bare(&m));
            self.label = m.tag("label").map(str::to_owned);
        }
        out
    }

    fn feed(&mut self, line: &str) -> Vec<Output> {
        let out = self.s.handle(Message::parse(line).unwrap(), self.clock);
        self.take(out)
    }

    /// Our own join, as the server stamps it: before every line the tests
    /// use, with a msgid of its own (a line with none is a hole in the
    /// cache).
    fn join(&mut self) -> Vec<Output> {
        static JOINS: AtomicU32 = AtomicU32::new(0);
        let n = JOINS.fetch_add(1, Ordering::Relaxed);
        self.feed(&format!("@time=2026-10-22T09:00:00.{:03}Z;msgid=join0-{n} :alice!a@h JOIN #test", n % 1000))
    }

    /// `join`, stamped `sec` seconds past 10:00 (the milliseconds differ per
    /// join, as two joins in one millisecond would be one line).
    fn join_at(&mut self, sec: u32) -> Vec<Output> {
        static JOINS: AtomicU32 = AtomicU32::new(0);
        let n = JOINS.fetch_add(1, Ordering::Relaxed);
        let time = stamp(sec, n % 1000);
        self.feed(&format!("@time={time};msgid=join{n} :alice!a@h JOIN #test"))
    }

    /// A line said live.
    fn live(&mut self, line: L) -> Vec<Output> {
        self.feed(&format!("@{} {}", line.tags(), line.privmsg()))
    }

    /// The open batch's label tag, and its reference.
    fn open(&mut self) -> String {
        self.batches += 1;
        let r = format!("b{}", self.batches);
        let label = self.label.clone().expect("a request is out");
        self.feed(&format!("@label={label} :srv BATCH +{r} chathistory {CHAN}"));
        r
    }

    fn item(&mut self, r: &str, line: L) {
        self.feed(&format!("@batch={r};{} {}", line.tags(), line.privmsg()));
    }

    fn close(&mut self, r: &str) -> Vec<Output> {
        self.feed(&format!(":srv BATCH -{r}"))
    }

    /// The server answers the request that is out with `lines`.
    fn reply(&mut self, lines: &[L]) -> Vec<Output> {
        let r = self.open();
        for line in lines {
            self.item(&r, *line);
        }
        self.close(&r)
    }

    /// The connection drops: what `conn` does. The store and the book go to
    /// the next connection.
    fn drop_conn(self) -> (Store, Book) {
        self.drop_into(None)
    }

    /// `drop_conn`, writing what the session kept to the cache first, as
    /// `conn` does once the connection is over.
    fn drop_into(mut self, cache: Option<&Shared>) -> (Store, Book) {
        let out = self.s.end(self.clock);
        self.take(out);
        if let Some(cache) = cache {
            for e in self.s.kept() {
                cache.write(e);
            }
            assert!(cache.flush());
        }
        self.store.apply(Event::Status { network: "net".into(), status: ConnStatus::Disconnected { reason: "gone".into(), retry: Duration::from_secs(2) } });
        let book = self.s.take_book();
        (self.store, book)
    }

    /// The channel's lines that people said, in the order shown.
    fn said(&self) -> Vec<String> {
        said(&self.store)
    }

    fn buffer(&self) -> &Buffer {
        buffer(&self.store)
    }
}

fn buffer(store: &Store) -> &Buffer {
    store.networks[0].buffers.iter().find(|b| b.target.same(&Target::Channel(CHAN.into()))).expect("the channel's buffer")
}

fn said(store: &Store) -> Vec<String> {
    buffer(store).lines.iter().filter(|l| l.kind == LineKind::Privmsg && !l.boundary).map(|l| l.text.clone()).collect()
}

fn texts(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

/// Panics unless every msgid in the buffer is held once.
fn assert_unique(store: &Store) {
    let mut ids: Vec<&str> = buffer(store).lines.iter().filter_map(|l| l.msgid.as_deref()).collect();
    let n = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), n, "a msgid is held twice: {:?}", buffer(store).lines);
}

fn is_sorted_by_time(store: &Store) -> bool {
    buffer(store).lines.windows(2).all(|w| w[0].time <= w[1].time)
}

/// A first connection that holds `lines`: the latest page answered with
/// them, as at the first join.
fn first_connection(limit: usize, lines: &[L]) -> Client {
    let mut c = Client::fresh(limit);
    c.join();
    assert_eq!(c.asked, [format!("CHATHISTORY LATEST #test * {}", limit.min(20))]);
    c.reply(lines);
    c
}

// --- 1. A reconnect's gap batch overlaps what is held -----------------------

#[test]
fn gap_batch_overlapping_held_lines_adds_each_line_once() {
    let mut c = first_connection(50, &[l("m1", 0), l("m2", 1), l("m3", 2)]);
    c.live(l("m4", 3));
    c.live(l("m5", 4));
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5"]));
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m5 20"]);
    // The server repeats the last two lines held, then the ones missed.
    let out = c.reply(&[l("m4", 3), l("m5", 4), l("m6", 5), l("m7", 6)]);
    assert!(history_requests(&out).is_empty(), "a short page closes the gap: {out:?}");
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5", "m6", "m7"]));
    assert_unique(&c.store);
    assert!(c.buffer().filled);
}

#[test]
fn a_gap_batch_that_repeats_everything_held_adds_only_what_is_new() {
    let mut c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    c.live(l("m3", 2));
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    c.reply(&[l("m1", 0), l("m2", 1), l("m3", 2), l("m4", 3)]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4"]));
    assert_unique(&c.store);
}

#[test]
fn nothing_missed_while_away_asks_after_the_newest_and_shows_it_once() {
    // Two drops in a row with nothing said between: still one copy of each.
    let c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m2 20"]);
    c.reply(&[]);
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m2 20"]);
    c.reply(&[l("m2", 1)]);
    assert_eq!(c.said(), texts(&["m1", "m2"]));
}

#[test]
fn a_server_that_names_messages_only_by_time_still_gives_each_line_once() {
    // soju says `MSGREFTYPES=timestamp`: the anchor goes as a time, and the
    // page may start at the anchor's own millisecond.
    let mut c = Client::fresh(50);
    c.join();
    c.reply(&[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.feed(":srv 005 alice MSGREFTYPES=timestamp :are supported");
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test timestamp=2026-10-22T10:00:00.999Z 20"]);
    c.reply(&[l("m2", 1), l("m3", 2), l("m4", 3)]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4"]));
    assert_unique(&c.store);
}

// --- 2. The reply is one page short of the gap ------------------------------

#[test]
fn a_full_gap_page_is_followed_by_another_from_its_last_line() {
    // The server's limit (2) is under the 20 asked for.
    let c = first_connection(2, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m2 2"]);
    c.reply(&[l("m3", 2), l("m4", 3)]);
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m4 2");
    assert!(!c.buffer().filled, "the gap is not closed yet");
    c.reply(&[l("m5", 4), l("m6", 5)]);
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m6 2");
    let out = c.reply(&[l("m7", 6)]);
    assert!(history_requests(&out).is_empty(), "a short page closes the gap: {out:?}");
    assert_eq!(c.asked.len(), 3);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5", "m6", "m7"]));
    assert!(c.buffer().filled);
}

#[test]
fn a_gap_that_is_an_exact_number_of_pages_ends_on_the_empty_page() {
    let c = first_connection(2, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    c.reply(&[l("m3", 2), l("m4", 3)]);
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m4 2");
    let out = c.reply(&[]);
    assert!(history_requests(&out).is_empty(), "an empty page closes the gap: {out:?}");
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4"]));
    assert!(c.buffer().filled);
    // The book holds m4 as the newest: the next drop asks from there.
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m4 2"]);
}

#[test]
fn pages_that_each_repeat_the_last_line_of_the_one_before_lose_nothing() {
    let c = first_connection(2, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    // A server counting its page from the anchor itself.
    c.reply(&[l("m2", 1), l("m3", 2)]);
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m3 2");
    c.reply(&[l("m3", 2), l("m4", 3)]);
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m4 2");
    c.reply(&[l("m4", 3)]);
    assert_eq!(c.asked.len(), 3);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4"]));
    assert_unique(&c.store);
}

#[test]
fn a_short_page_in_a_gap_is_never_asked_past_by_the_page_size_alone() {
    // 20 asked for, the server's limit is 2 and it answered 2: full, so ask
    // on, even though 2 is far under the 20 hirc would like.
    let c = first_connection(2, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    let out = c.reply(&[l("m3", 2), l("m4", 3)]);
    assert_eq!(history_requests(&out).len(), 1);
}

// --- 3. Empty replies, and a reply of only what is held ---------------------

#[test]
fn an_empty_gap_batch_changes_nothing_and_closes_the_gap() {
    let c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    let out = c.reply(&[]);
    assert!(history_requests(&out).is_empty());
    assert_eq!(c.said(), texts(&["m1", "m2"]));
    assert!(c.buffer().filled);
    // A line said after it follows on, and the next drop asks after that.
    c.live(l("m3", 9));
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m3 20"]);
}

#[test]
fn a_bare_ack_is_an_empty_gap_reply() {
    let c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    let label = c.label.clone().unwrap();
    c.feed(&format!("@label={label} :srv ACK"));
    assert_eq!(c.said(), texts(&["m1", "m2"]));
    assert!(c.buffer().filled);
    assert_eq!(c.asked.len(), 1, "nothing asked again");
}

#[test]
fn a_reply_whose_newest_line_is_the_newest_held_adds_nothing_and_asks_no_more() {
    let c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    c.reply(&[l("m2", 1)]);
    assert_eq!(c.said(), texts(&["m1", "m2"]));
    assert_eq!(c.asked.len(), 1);
    assert!(c.buffer().filled);
}

#[test]
fn a_full_page_of_only_the_anchor_does_not_loop() {
    // A server whose limit is 1 that answers with the anchor itself: asking
    // from the same line again would bring the same page for ever.
    let c = first_connection(1, &[l("m1", 0)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 1, "", false);
    c.join();
    let out = c.reply(&[l("m1", 0)]);
    assert!(history_requests(&out).is_empty(), "{out:?}");
    assert_eq!(c.asked.len(), 1);
    assert_eq!(c.said(), texts(&["m1"]));
    assert!(c.buffer().filled, "this connection has all it will get");
}

// --- 4. A live line while the gap batch is open -----------------------------

#[test]
fn a_live_line_inside_an_open_gap_batch_is_kept_once_with_the_batch() {
    let c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    let r = c.open();
    c.item(&r, l("m3", 10));
    // Said live while the batch is still open: first a new line, then one
    // the batch is about to hold too.
    let out = c.live(l("m9", 60));
    assert_eq!(out.iter().filter(|o| matches!(o, Output::Event(Event::Line { .. }))).count(), 1, "{out:?}");
    c.live(l("m4", 11));
    c.item(&r, l("m4", 11));
    c.item(&r, l("m5", 12));
    c.close(&r);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5", "m9"]));
    assert_unique(&c.store);
    assert!(is_sorted_by_time(&c.store));
    // The newest held is the live line, not the page's last.
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m9 20"]);
}

#[test]
fn a_live_line_between_full_gap_pages_never_moves_where_the_next_page_starts() {
    let c = first_connection(2, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    let r = c.open();
    c.item(&r, l("m3", 2));
    c.item(&r, l("m4", 3));
    c.live(l("m9", 60));
    c.close(&r);
    // The next page starts at the page's last line, not at the live one:
    // m5..m7 sit between them.
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m4 2");
    c.reply(&[l("m5", 4), l("m6", 5)]);
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m6 2");
    c.live(l("m10", 61));
    c.reply(&[l("m7", 6)]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5", "m6", "m7", "m9", "m10"]));
    assert_unique(&c.store);
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m10 2"]);
}

#[test]
fn a_live_line_that_the_gap_page_holds_too_is_one_line() {
    let c = first_connection(50, &[l("m1", 0)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    c.live(l("m2", 5));
    c.reply(&[l("m2", 5), l("m3", 6)]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3"]));
    assert_unique(&c.store);
}

#[test]
fn a_line_between_the_join_and_the_gap_batch_is_held_when_the_batch_comes() {
    let c = first_connection(50, &[l("m1", 0)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    // The gap is asked for but not answered yet.
    c.live(l("m9", 60));
    c.reply(&[l("m2", 5)]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m9"]));
}

// --- 5. Lines in one millisecond across the boundary ------------------------

#[test]
fn lines_in_one_millisecond_with_different_msgids_across_the_boundary_are_all_kept() {
    let mut c = first_connection(50, &[lm("m1", 0, 0, "a")]);
    c.live(lm("m2", 5, 500, "b"));
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m2 20"]);
    // m2 again, then more lines in its millisecond that were missed.
    c.reply(&[lm("m2", 5, 500, "b"), lm("m3", 5, 500, "c"), lm("m4", 5, 500, "d"), lm("m5", 6, 0, "e")]);
    assert_eq!(c.said(), texts(&["a", "b", "c", "d", "e"]));
    assert_unique(&c.store);
}

#[test]
fn a_millisecond_split_across_two_gap_pages_loses_nothing() {
    let c = first_connection(2, &[lm("m1", 0, 0, "a"), lm("m2", 5, 500, "b")]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    c.reply(&[lm("m3", 5, 500, "c"), lm("m4", 5, 500, "d")]);
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m4 2");
    c.reply(&[lm("m5", 5, 500, "e"), lm("m6", 6, 0, "f")]);
    assert_eq!(c.asked.last().unwrap(), "CHATHISTORY AFTER #test msgid=m6 2");
    c.reply(&[]);
    assert_eq!(c.said(), texts(&["a", "b", "c", "d", "e", "f"]));
    assert_unique(&c.store);
}

#[test]
fn a_live_line_in_the_millisecond_of_the_pages_last_line_is_the_newest() {
    let c = first_connection(50, &[lm("m1", 0, 0, "a")]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    let r = c.open();
    c.item(&r, lm("m3", 7, 0, "page"));
    c.live(lm("m9", 7, 0, "live"));
    c.close(&r);
    let mut got = c.said();
    got.sort();
    assert_eq!(got, texts(&["a", "live", "page"]));
    // Same millisecond: the live one, which arrived last, is where to ask from.
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m9 20"]);
}

#[test]
fn the_same_words_by_the_same_nick_a_millisecond_apart_are_two_lines() {
    let mut c = first_connection(50, &[lm("m1", 0, 0, "lol")]);
    c.live(lm("m2", 0, 1, "lol"));
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    c.reply(&[lm("m1", 0, 0, "lol"), lm("m2", 0, 1, "lol"), lm("m3", 0, 2, "lol")]);
    assert_eq!(c.said(), texts(&["lol", "lol", "lol"]));
}

#[test]
fn the_same_words_by_the_same_nick_in_one_millisecond_are_two_lines() {
    let mut c = first_connection(50, &[lm("m1", 0, 0, "lol"), lm("m2", 0, 0, "lol")]);
    assert_eq!(c.said(), texts(&["lol", "lol"]));
    c.live(lm("m3", 5, 0, "again"));
    c.live(lm("m4", 5, 0, "again"));
    assert_eq!(c.said(), texts(&["lol", "lol", "again", "again"]));
}

// --- 6. Servers without chathistory ------------------------------------------

/// What IRCCloud's bouncer ACKed: batch, but no chathistory and no playback.
const IRCCLOUD: &str = "server-time echo-message cap-notify batch away-notify labeled-response znc.in/self-message";

/// A session through IRCCloud's bouncer with its replay on, attached at
/// `clock`. Its replay comes by itself, loose lines timed with server-time.
fn irccloud(store: Store, book: Book, clock: &str) -> Client {
    let cfg = SessionConfig {
        nickname: "alice".into(),
        alt_nicks: vec![],
        username: "alice".into(),
        realname: "Alice".into(),
        password: None,
        sasl: None,
        channels: vec![CHAN.into()],
        bridges: vec![],
        chathistory: true,
        history_page: Some(20),
        self_replays: true,
    };
    let mut store = store;
    store.apply(Event::Status { network: "net".into(), status: ConnStatus::Connecting });
    let mut c = Client { s: Session::new("net", cfg).with_book(book), store, asked: Vec::new(), label: None, batches: 0, clock: at(clock) };
    let out = c.s.start();
    c.take(out);
    for line in [
        format!(":bnc.irccloud.com CAP * LS :{IRCCLOUD}"),
        format!(":bnc.irccloud.com CAP * ACK :{IRCCLOUD}"),
        ":irc.example 001 alice :Welcome".to_owned(),
        ":irc.example 005 alice CHANTYPES=# :are supported".to_owned(),
        ":irc.example 376 alice :End of /MOTD".to_owned(),
        ":alice!a@h JOIN #test".to_owned(),
    ] {
        c.feed(&line);
    }
    assert!(c.asked.is_empty(), "IRCCloud replays by itself: nothing is asked for");
    c
}

fn loose(c: &mut Client, time: &str, nick: &str, text: &str) -> Vec<Output> {
    c.feed(&format!("@time={time} :{nick}!b@h PRIVMSG #test :{text}"))
}

#[test]
fn a_bouncer_replay_of_lines_held_from_before_the_drop_adds_no_duplicate() {
    let mut c = irccloud(Store::new(["net".to_string()]), Book::default(), "2026-10-22T12:00:00Z");
    // Lines said while attached (after the attach time: news).
    loose(&mut c, "2026-10-22T12:00:01.000Z", "bob", "l1");
    loose(&mut c, "2026-10-22T12:00:02.000Z", "bob", "l2");
    assert_eq!(c.said(), texts(&["l1", "l2"]));
    let (store, book) = c.drop_conn();

    let mut c = irccloud(store, book, "2026-10-22T13:00:00Z");
    // The replay: what we had already, then what we missed, all before now.
    for (time, text) in [("12:00:01.000", "l1"), ("12:00:02.000", "l2"), ("12:30:00.000", "m1"), ("12:45:00.000", "m2")] {
        let out = loose(&mut c, &format!("2026-10-22T{time}Z"), "bob", text);
        assert!(out.iter().all(|o| !matches!(o, Output::Event(Event::Line { .. }))), "a replay is history, not news: {out:?}");
    }
    loose(&mut c, "2026-10-22T13:00:01.000Z", "bob", "l3");
    assert_eq!(c.said(), texts(&["l1", "l2", "m1", "m2", "l3"]));
    assert_unique(&c.store);
    assert!(is_sorted_by_time(&c.store));
}

#[test]
fn two_replays_in_a_row_still_show_each_line_once() {
    let c = irccloud(Store::new(["net".to_string()]), Book::default(), "2026-10-22T12:00:00Z");
    let (store, book) = c.drop_conn();
    let mut c = irccloud(store, book, "2026-10-22T13:00:00Z");
    loose(&mut c, "2026-10-22T12:10:00.000Z", "bob", "m1");
    let (store, book) = c.drop_conn();
    // The bouncer replays m1 again (not acknowledged by a client id yet).
    let mut c = irccloud(store, book, "2026-10-22T14:00:00Z");
    loose(&mut c, "2026-10-22T12:10:00.000Z", "bob", "m1");
    loose(&mut c, "2026-10-22T13:30:00.000Z", "bob", "m2");
    assert_eq!(c.said(), texts(&["m1", "m2"]));
}

#[test]
fn identical_lines_a_millisecond_apart_are_both_kept_and_both_replayed_once() {
    let mut c = irccloud(Store::new(["net".to_string()]), Book::default(), "2026-10-22T12:00:00Z");
    loose(&mut c, "2026-10-22T12:00:01.100Z", "bob", "ok");
    loose(&mut c, "2026-10-22T12:00:01.600Z", "bob", "ok");
    assert_eq!(c.said(), texts(&["ok", "ok"]));
    let (store, book) = c.drop_conn();

    let mut c = irccloud(store, book, "2026-10-22T13:00:00Z");
    loose(&mut c, "2026-10-22T12:00:01.100Z", "bob", "ok");
    loose(&mut c, "2026-10-22T12:00:01.600Z", "bob", "ok");
    assert_eq!(c.said(), texts(&["ok", "ok"]));
}

#[test]
fn identical_lines_from_different_nicks_in_one_millisecond_are_both_kept() {
    let mut c = irccloud(Store::new(["net".to_string()]), Book::default(), "2026-10-22T12:00:00Z");
    loose(&mut c, "2026-10-22T12:00:01.100Z", "bob", "ok");
    loose(&mut c, "2026-10-22T12:00:01.100Z", "carol", "ok");
    let (store, book) = c.drop_conn();
    let mut c = irccloud(store, book, "2026-10-22T13:00:00Z");
    loose(&mut c, "2026-10-22T12:00:01.100Z", "bob", "ok");
    loose(&mut c, "2026-10-22T12:00:01.100Z", "carol", "ok");
    assert_eq!(c.said(), texts(&["ok", "ok"]));
}

#[test]
fn identical_lines_in_one_second_without_msgids_are_both_kept() {
    let mut c = irccloud(Store::new(["net".to_string()]), Book::default(), "2026-10-22T12:00:00Z");
    loose(&mut c, "2026-10-22T12:00:01Z", "bob", "ok");
    loose(&mut c, "2026-10-22T12:00:01Z", "bob", "ok");
    assert_eq!(c.said(), texts(&["ok", "ok"]));
}

/// What ZNC with its playback module offers: no chathistory.
const ZNC: &str = "batch server-time message-tags znc.in/playback znc.in/self-message";

#[test]
fn a_znc_playback_that_repeats_held_lines_adds_no_duplicate() {
    fn znc(store: Store, book: Book) -> Client {
        let cfg = SessionConfig {
            nickname: "alice".into(),
            alt_nicks: vec![],
            username: "alice".into(),
            realname: "Alice".into(),
            password: None,
            sasl: None,
            channels: vec![CHAN.into()],
            bridges: vec![],
            chathistory: true,
            history_page: Some(20),
            self_replays: false,
        };
        let mut store = store;
        store.apply(Event::Status { network: "net".into(), status: ConnStatus::Connecting });
        let mut c = Client { s: Session::new("net", cfg).with_book(book), store, asked: Vec::new(), label: None, batches: 0, clock: noon() };
        let out = c.s.start();
        c.take(out);
        for line in [
            format!(":irc.znc.in CAP * LS :{ZNC}"),
            format!(":irc.znc.in CAP alice ACK :{ZNC}"),
            ":irc.znc.in 001 alice :Welcome".to_owned(),
            ":irc.znc.in 005 alice CHANTYPES=# :are supported".to_owned(),
        ] {
            c.feed(&line);
        }
        let out = c.feed(":irc.znc.in 376 alice :End of MOTD");
        assert!(
            out.iter().any(|o| matches!(o, Output::Send(m) if m.command == "PRIVMSG" && m.params.first().map(String::as_str) == Some("*playback"))),
            "PLAY asked: {out:?}"
        );
        c.feed(":alice!a@h JOIN #test");
        c
    }
    let mut c = znc(Store::new(["net".to_string()]), Book::default());
    c.feed("@time=2026-10-22T12:00:01.000Z :bob!b@h PRIVMSG #test :l1");
    c.feed("@time=2026-10-22T12:00:02.000Z :bob!b@h PRIVMSG #test :l2");
    let (store, book) = c.drop_conn();

    let mut c = znc(store, book);
    c.feed(":irc.znc.in BATCH +p znc.in/playback #test");
    for (time, text) in [("12:00:01.000", "l1"), ("12:00:02.000", "l2"), ("12:30:00.000", "m1")] {
        c.feed(&format!("@batch=p;time=2026-10-22T{time}Z :bob!b@h PRIVMSG #test :{text}"));
    }
    c.feed(":irc.znc.in BATCH -p");
    c.feed("@time=2026-10-22T13:00:01.000Z :bob!b@h PRIVMSG #test :l3");
    assert_eq!(c.said(), texts(&["l1", "l2", "m1", "l3"]));
    assert_unique(&c.store);
}

// --- 7. A disconnect in the middle of a batch --------------------------------

#[test]
fn a_gap_batch_cut_off_by_the_drop_keeps_its_lines_and_the_gap_is_asked_again_from_them() {
    let c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    let r = c.open();
    c.item(&r, l("m3", 2));
    c.item(&r, l("m4", 3));
    // The drop: the batch never closes.
    assert_eq!(c.said(), texts(&["m1", "m2"]), "a page shows when it closes, or when the drop cuts it");
    let (store, book) = c.drop_conn();
    assert_eq!(said(&store), texts(&["m1", "m2", "m3", "m4"]), "the partial batch's lines are not lost");

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m4 20"], "what was missed is still asked for, from the last line that came");
    c.reply(&[l("m5", 4), l("m6", 5)]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5", "m6"]));
    assert_unique(&c.store);
}

#[test]
fn a_gap_batch_cut_off_before_its_first_line_asks_for_the_same_gap_again() {
    let c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    c.open();
    let (store, book) = c.drop_conn();
    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m2 20"]);
    c.reply(&[l("m3", 2)]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3"]));
}

#[test]
fn a_cut_gap_page_followed_by_a_second_cut_still_loses_nothing() {
    let c = first_connection(2, &[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    c.reply(&[l("m3", 2), l("m4", 3)]);
    // The next page (m5, m6) is cut after m5.
    let r = c.open();
    c.item(&r, l("m5", 4));
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 2, "", false);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m5 2"]);
    c.reply(&[l("m6", 5), l("m7", 6)]);
    c.reply(&[]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5", "m6", "m7"]));
    assert_unique(&c.store);
}

#[test]
fn a_latest_page_cut_off_by_the_drop_leaves_no_hole_after_the_reconnect() {
    // Joined for the first time: the server's newest lines are m81..m100
    // (a page is 20). The drop cuts the batch after m82. By the time we are
    // back the server has m81..m110, and its newest page is m91..m110.
    let mut c = Client::fresh(20);
    c.join();
    assert_eq!(c.asked, ["CHATHISTORY LATEST #test * 20"]);
    let r = c.open();
    c.item(&r, l("m81", 81));
    c.item(&r, l("m82", 82));
    let (store, book) = c.drop_conn();
    assert_eq!(said(&store), texts(&["m81", "m82"]));

    let mut c = Client::connect(store, book, 20, "", false);
    c.join();
    // Whatever it asks for, m83..m90 must come, or be asked for once the
    // user scrolls: each line the server has must reach the buffer.
    let first = c.asked.clone();
    let seq = [
        l("m83", 83),
        l("m84", 84),
        l("m85", 85),
        l("m86", 86),
        l("m87", 87),
        l("m88", 88),
        l("m89", 89),
        l("m90", 90),
        l("m91", 91),
        l("m92", 92),
        l("m93", 93),
        l("m94", 94),
        l("m95", 95),
        l("m96", 96),
        l("m97", 97),
        l("m98", 98),
        l("m99", 99),
        l("m100", 100),
        l("m101", 101),
        l("m102", 102),
        l("m103", 103),
        l("m104", 104),
        l("m105", 105),
        l("m106", 106),
        l("m107", 107),
        l("m108", 108),
        l("m109", 109),
        l("m110", 110),
    ];
    // Answer as a real server would: AFTER m82 gives the lines past it,
    // LATEST gives the newest 20.
    let page = if first[0].starts_with("CHATHISTORY AFTER") { &seq[..20] } else { &seq[seq.len() - 20..] };
    c.reply(page);
    let want: Vec<String> = (81..=100).map(|n| format!("m{n}")).collect();
    let got = c.said();
    assert!(want.iter().all(|w| got.contains(w)), "asked {first:?}; held {got:?}");
}

#[test]
fn a_multiline_message_cut_off_by_the_drop_is_whole_after_the_gap_fill() {
    let c = first_connection(50, &[l("m1", 0)]);
    let (store, book) = c.drop_conn();

    let mut c = Client::connect(store, book, 50, "draft/multiline=max-bytes=4096", false);
    c.join();
    c.live(l("m2", 5));
    // A multiline message opens inside the page and the connection drops
    // after its first line.
    let r = c.open();
    c.feed(&format!("@batch={r};time={};msgid=ml1 :bob!b@h BATCH +ml draft/multiline {CHAN}", stamp(9, 0)));
    c.feed("@batch=ml :bob!b@h PRIVMSG #test :first");
    let (store, book) = c.drop_conn();
    let partial = said(&store);
    assert_eq!(partial.iter().filter(|t| t.starts_with("first")).count(), 1, "shown once: {partial:?}");

    let mut c = Client::connect(store, book, 50, "draft/multiline=max-bytes=4096", false);
    c.join();
    // The server now gives it whole.
    let r = c.open();
    c.feed(&format!("@batch={r};time={};msgid=ml1 :bob!b@h BATCH +ml draft/multiline {CHAN}", stamp(9, 0)));
    c.feed("@batch=ml :bob!b@h PRIVMSG #test :first");
    c.feed("@batch=ml :bob!b@h PRIVMSG #test :second");
    c.feed(":bob!b@h BATCH -ml");
    c.close(&r);
    let got = c.said();
    assert_eq!(got.iter().filter(|t| t.contains("first")).count(), 1, "{got:?}");
    assert!(got.iter().any(|t| t == "first\nsecond"), "the whole message replaces the cut one: {got:?}");
}

// --- 8. The store across the boundary ---------------------------------------

fn line(sec: u32, ms: u32, id: Option<&str>, text: &str) -> Line {
    Line {
        seq: 0,
        time: at(&stamp(sec, ms)),
        msgid: id.map(Into::into),
        kind: LineKind::Privmsg,
        nick: Some("bob".into()),
        text: text.into(),
        own: false,
        delivery: Delivery::Sent,
        boundary: false,
        replayed: false,
        via: None,
    }
}

fn history(lines: Vec<Line>, older: bool) -> Event {
    Event::History { network: "net".into(), target: Target::Channel(CHAN.into()), lines, start: false, older }
}

fn live_line(line: Line) -> Event {
    Event::Line { network: "net".into(), target: Target::Channel(CHAN.into()), line }
}

#[test]
fn a_drop_leaves_one_boundary_line_in_each_window_we_were_in() {
    let mut store = Store::new(["net".to_string()]);
    store.clock = Some(at("2026-10-22T10:30:00Z"));
    store.apply(Event::Status { network: "net".into(), status: ConnStatus::Connected { nick: "alice".into() } });
    store.apply(Event::Joined { network: "net".into(), channel: CHAN.into() });
    store.apply(Event::Joined { network: "net".into(), channel: "#other".into() });
    store.apply(Event::Parted { network: "net".into(), channel: "#other".into() });
    store.apply(live_line(Line { msgid: Some("m1".into()), ..line(0, 0, None, "a") }));
    let down = || Event::Status { network: "net".into(), status: ConnStatus::Disconnected { reason: "gone".into(), retry: Duration::from_secs(2) } };
    store.apply(down());
    let boundaries = |store: &Store, b: usize| store.networks[0].buffers[b].lines.iter().filter(|l| l.boundary).count();
    // The server buffer, the channel we were in; not the one we had left.
    assert_eq!((boundaries(&store, 0), boundaries(&store, 1), boundaries(&store, 2)), (1, 1, 0));
    // The reconnect says nothing in the buffers; a second drop adds a second.
    store.apply(Event::Status { network: "net".into(), status: ConnStatus::Connecting });
    store.apply(Event::Status { network: "net".into(), status: ConnStatus::Connected { nick: "alice".into() } });
    assert_eq!((boundaries(&store, 0), boundaries(&store, 1)), (1, 1));
    store.apply(Event::Joined { network: "net".into(), channel: CHAN.into() });
    store.apply(down());
    assert_eq!((boundaries(&store, 0), boundaries(&store, 1)), (2, 2));
}

#[test]
fn gap_lines_land_around_the_boundary_by_time_and_none_is_lost_or_repeated() {
    let mut c = first_connection(50, &[l("m1", 0), l("m2", 1)]);
    c.store.clock = Some(at("2026-10-22T10:30:00Z"));
    c.live(l("m3", 2));
    let (store, book) = c.drop_conn();
    assert_eq!(buffer(&store).lines.iter().filter(|l| l.boundary).count(), 1);

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    // Missed while away: some before the drop was noticed (10:30), some after.
    c.reply(&[l("m3", 2), l("m4", 20 * 60), l("m5", 40 * 60)]);
    let b = c.buffer();
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5"]));
    assert!(is_sorted_by_time(&c.store));
    // The boundary stays where its time puts it: between m4 and m5.
    let at_boundary = b.lines.iter().position(|l| l.boundary).unwrap();
    assert_eq!(b.lines[at_boundary - 1].text, "m4");
    assert_eq!(b.lines[at_boundary + 1].text, "m5");
    assert_unique(&c.store);
}

#[test]
fn a_page_older_than_what_is_held_lands_above_it_in_order() {
    let mut store = Store::new(["net".to_string()]);
    store.apply(live_line(line(10, 0, Some("m10"), "ten")));
    store.apply(live_line(line(11, 0, Some("m11"), "eleven")));
    store.apply(history(vec![line(1, 0, Some("m1"), "one"), line(5, 0, Some("m5"), "five")], true));
    store.apply(history(vec![line(0, 0, Some("m0"), "zero"), line(1, 0, Some("m1"), "one")], true));
    assert_eq!(said(&store), texts(&["zero", "one", "five", "ten", "eleven"]));
    assert_unique(&store);
}

#[test]
fn an_older_page_goes_before_what_it_ties_with_and_a_newer_one_after() {
    let mut store = Store::new(["net".to_string()]);
    store.apply(live_line(line(10, 500, Some("m3"), "c")));
    // A page before, tied in time with the held line: before it.
    store.apply(history(vec![line(10, 500, Some("m1"), "a"), line(10, 500, Some("m2"), "b")], true));
    // A page after, tied again: after.
    store.apply(history(vec![line(10, 500, Some("m4"), "d"), line(10, 500, Some("m5"), "e")], false));
    assert_eq!(said(&store), texts(&["a", "b", "c", "d", "e"]));
}

#[test]
fn a_live_line_and_its_replayed_copy_are_one_whichever_comes_first() {
    let mut store = Store::new(["net".to_string()]);
    store.apply(live_line(line(5, 0, Some("m5"), "x")));
    store.apply(history(vec![Line { replayed: true, ..line(5, 0, Some("m5"), "x") }], false));
    store.apply(history(vec![Line { replayed: true, ..line(6, 0, Some("m6"), "y") }], false));
    store.apply(live_line(line(6, 0, Some("m6"), "y")));
    assert_eq!(said(&store), texts(&["x", "y"]));
    // And a replayed line is never news.
    assert_eq!(buffer(&store).unread, 1, "only the live x counted; y was already held when it came live");
}

#[test]
fn a_line_we_sent_that_never_got_its_echo_stays_marked_and_the_gap_page_adds_the_real_one() {
    let mut c = first_connection(50, &[l("m1", 0)]);
    // Sent, echo pending.
    c.store.apply(Event::Line {
        network: "net".into(),
        target: Target::Channel(CHAN.into()),
        line: Line { own: true, delivery: Delivery::Pending, nick: Some("alice".into()), text: "hello".into(), ..line(5, 0, None, "") },
    });
    let (store, book) = c.drop_conn();
    let unsent = buffer(&store).lines.iter().filter(|l| l.delivery == Delivery::Unsent).count();
    assert_eq!(unsent, 1, "the connection dropped before the echo");

    let mut c = Client::connect(store, book, 50, "", false);
    c.join();
    // The line had gone out after all: it is in the gap page, from alice.
    let r = c.open();
    c.feed(&format!("@batch={r};{} :alice!a@h PRIVMSG #test :hello", l("m2", 5).tags()));
    c.close(&r);
    let hellos: Vec<&Line> = c.buffer().lines.iter().filter(|l| l.text == "hello").collect();
    // Both stay: the unsent mark says it may not have gone out, the server's
    // copy says it did. Neither is hidden, neither is shown as news twice.
    assert_eq!(hellos.len(), 2, "{hellos:?}");
    assert_eq!(hellos.iter().filter(|l| l.delivery == Delivery::Unsent).count(), 1);
    assert_eq!(hellos.iter().filter(|l| l.msgid.as_deref() == Some("m2")).count(), 1);
}

// --- 9. The cache across a restart ------------------------------------------

/// The cached msgids of the channel, past breaks, without our own joins.
fn cached_ids(cache: &Shared) -> Vec<String> {
    cache.read(|c| c.latest_in("net", CHAN, 1000, Reach::All)).unwrap().into_iter().filter(|l| l.kind == LineKind::Privmsg).filter_map(|l| l.msgid).collect()
}

#[test]
fn lines_cached_before_the_drop_merge_with_the_gap_fill_once() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Shared::open(&dir.path().join("history.db")).unwrap();

    // Before the drop: the latest page, our join, then live lines. All written.
    let mut c = Client::connect(Store::new(["net".to_string()]), Book::default(), 50, "", true);
    c.join_at(300);
    c.reply(&[l("m1", 0), l("m2", 1)]);
    c.live(l("m3", 400));
    c.live(l("m4", 401));
    c.drop_into(Some(&cache));
    assert_eq!(cached_ids(&cache), texts(&["m1", "m2", "m3", "m4"]));

    // A restart: nothing in memory. The cache opens the window as `conn`'s
    // seed does: its latest page, down to the first break (our join), and
    // the book is told what it holds.
    let cached = cache.read(|c| c.latest("net", CHAN, 100)).unwrap();
    let ids: Vec<&str> = cached.iter().filter(|l| l.kind == LineKind::Privmsg).filter_map(|l| l.msgid.as_deref()).collect();
    assert_eq!(ids, ["m3", "m4"], "the run follows on from the join, which the latest page did not reach");
    let mut book = Book::default();
    book.reach(CHAN, false, cached.first().map(Anchor::of).as_ref(), cached.last().map(Anchor::of).as_ref());
    let mut store = Store::new(["net".to_string()]);
    store.apply(Event::History { network: "net".into(), target: Target::Channel(CHAN.into()), lines: cached, start: false, older: false });
    assert_eq!(said(&store), texts(&["m3", "m4"]));

    let mut c = Client::connect(store, book, 50, "", true);
    c.join_at(900);
    assert_eq!(c.asked, ["CHATHISTORY AFTER #test msgid=m4 20"]);
    // The server's reply overlaps the cache by two lines.
    c.reply(&[l("m3", 400), l("m4", 401), l("m5", 500), l("m6", 600)]);
    assert_eq!(c.said(), texts(&["m3", "m4", "m5", "m6"]));
    assert_unique(&c.store);

    // What the gap fill kept merges with the cache: each line once.
    c.drop_into(Some(&cache));
    assert_eq!(cached_ids(&cache), texts(&["m1", "m2", "m3", "m4", "m5", "m6"]));
}

#[test]
fn a_gap_fill_longer_than_a_page_is_cached_whole_and_once() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Shared::open(&dir.path().join("history.db")).unwrap();

    let mut c = Client::connect(Store::new(["net".to_string()]), Book::default(), 2, "", true);
    c.join_at(5);
    c.reply(&[l("m1", 0), l("m2", 1)]);
    let (store, book) = c.drop_into(Some(&cache));

    let mut c = Client::connect(store, book, 2, "", true);
    c.join_at(900);
    c.reply(&[l("m3", 2), l("m4", 3)]);
    c.reply(&[l("m5", 4), l("m6", 5)]);
    c.reply(&[l("m7", 6)]);
    assert_eq!(c.said(), texts(&["m1", "m2", "m3", "m4", "m5", "m6", "m7"]));
    c.drop_into(Some(&cache));
    assert_eq!(cached_ids(&cache), texts(&["m1", "m2", "m3", "m4", "m5", "m6", "m7"]));
}
