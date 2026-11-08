//! No line is lost, or shown twice, when the connection to a real Ergo
//! drops and comes back. Run with:
//!   scripts/ergo-test.sh setup
//!   cargo test -p hirc-core --features ergo-tests --test reconnect
//!
//! hirc connects through a relay (`common::proxy`) that can cut the
//! connection with no QUIT. Every test ends in `T::settle`, which checks
//! that each line bob sent reached hirc exactly once, and that the store
//! shows them in the order they were sent.

#![cfg(feature = "ergo-tests")]

mod common;

use common::events::said_in;
use common::proxy::Proxy;
use common::{Ergo, Raw, register_account, register_always_on};
use hirc_core::config::{Config, Sasl, Server};
use hirc_core::conn::Flush;
use hirc_core::event::{Command, ConnStatus, Event, Target};
use hirc_core::store::{LineKind, Store};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// A server that keeps 100 messages per page (tests/ergo/ircd.yaml.in).
const SERVER_LIMIT: usize = 100;

/// hirc as alice, through the relay at `port`.
fn config(port: u16, channels: &[&str], page: Option<usize>, ping: Option<u64>) -> Config {
    let mut config = Config::default();
    config.servers.insert(
        "test".into(),
        Server {
            nickname: "alice".into(),
            server: "127.0.0.1".into(),
            port: Some(port),
            use_tls: false,
            channels: channels.iter().map(|c| c.to_string()).collect(),
            history_page_size: page,
            ping_time: ping,
            ping_timeout: ping,
            sasl: Some(Sasl::Plain { username: "alice".into(), password: None, password_file: None, password_command: Some("printf hunter22".into()) }),
            ..Server::default()
        },
    );
    config
}

/// Where a line to `buffer` goes: a channel, or alice herself.
fn route(buffer: &str) -> &str {
    if buffer.starts_with('#') { buffer } else { "alice" }
}

fn target(buffer: &str) -> Target {
    if buffer.starts_with('#') { Target::Channel(buffer.into()) } else { Target::Query(buffer.into()) }
}

fn texts(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}{i}")).collect()
}

/// A line hirc showed, live or from history.
struct Got {
    buffer: String,
    text: String,
    live: bool,
}

/// One test: a server, bob talking in it, and hirc as alice watched
/// through its events.
struct T {
    ergo: Ergo,
    proxy: Proxy,
    bob: Raw,
    always_on: bool,
    channels: Vec<String>,
    tx: UnboundedSender<Command>,
    rx: UnboundedReceiver<Event>,
    flush: Option<Flush>,
    store: Store,
    /// Every line shown, as it came.
    got: Vec<Got>,
    seen: HashSet<(String, String)>,
    /// What had been shown when the network last came back.
    before: HashSet<(String, String)>,
    /// Lines shown before a drop that history then brought again.
    refetched: Vec<String>,
    /// What bob (or someone) sent, per buffer, in order.
    sent: Vec<(String, String)>,
    /// Buffers whose history is up to date on this connection.
    filled: HashSet<String>,
    online: bool,
    connects: usize,
    joined: usize,
    sentinels: usize,
    /// `ping_time` and `ping_timeout` for the next hirc, else the defaults.
    ping: Option<u64>,
    /// Why each connection went down, as hirc said.
    drops: Vec<String>,
}

impl T {
    /// The server and bob (in `channels`), with no hirc yet.
    async fn prepare(channels: &[&str], always_on: bool) -> T {
        let ergo = Ergo::start();
        if always_on {
            register_always_on(ergo.plain_port, "alice", "hunter22").await;
        } else {
            register_account(ergo.plain_port, "alice", "hunter22").await;
        }
        let mut bob = Raw::connect(ergo.plain_port, "bob").await;
        for c in channels {
            bob.send(&format!("JOIN {c}")).await;
            bob.expect("JOIN").await;
        }
        let proxy = Proxy::start(ergo.plain_port).await;
        // Placeholders until `spawn_hirc`.
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (_, rx) = tokio::sync::mpsc::unbounded_channel();
        T {
            ergo,
            proxy,
            bob,
            always_on,
            channels: channels.iter().map(|c| c.to_string()).collect(),
            tx,
            rx,
            flush: None,
            store: Store::new(["test".to_string()]),
            got: Vec::new(),
            seen: HashSet::new(),
            before: HashSet::new(),
            refetched: Vec::new(),
            sent: Vec::new(),
            filled: HashSet::new(),
            online: false,
            connects: 0,
            joined: 0,
            sentinels: 0,
            ping: None,
            drops: Vec::new(),
        }
    }

    /// Starts hirc (a fresh one: nothing shown yet).
    fn spawn_hirc(&mut self, page: Option<usize>, cache: Option<PathBuf>) {
        let channels: Vec<&str> = self.channels.iter().map(String::as_str).collect();
        let (tx, rx, flush) = hirc_core::conn::spawn(config(self.proxy.port, &channels, page, self.ping), cache);
        self.tx = tx;
        self.rx = rx;
        self.flush = Some(flush);
        self.store = Store::new(["test".to_string()]);
        self.got.clear();
        self.seen.clear();
        self.before.clear();
        self.filled.clear();
        self.online = false;
        self.joined = 0;
    }

    /// Server and hirc up, every channel joined and its history in.
    async fn start(channels: &[&str], always_on: bool, page: Option<usize>) -> T {
        let mut t = T::prepare(channels, always_on).await;
        t.spawn_hirc(page, None);
        t.ready().await;
        t
    }

    async fn ready(&mut self) {
        let channels = self.channels.clone();
        self.until("hirc joined and filled", |t| t.online && channels.iter().all(|c| t.filled.contains(&c.to_ascii_lowercase()))).await;
    }

    fn take(&mut self, ev: Event) {
        match &ev {
            Event::Status { status: ConnStatus::Connected { .. }, .. } => {
                self.online = true;
                self.connects += 1;
            }
            Event::Status { status: ConnStatus::Disconnected { reason, .. }, .. } => {
                self.online = false;
                self.drops.push(reason.clone());
            }
            Event::Status { status: ConnStatus::Offline, .. } => self.online = false,
            Event::Status { status: ConnStatus::Failed { reason }, .. } => panic!("hirc gave up: {reason}"),
            Event::Joined { .. } => self.joined += 1,
            Event::Filled { target, .. } => {
                if let Some(n) = target.name() {
                    self.filled.insert(n.to_ascii_lowercase());
                }
            }
            Event::Line { target, line, .. } if !line.own && line.kind == LineKind::Privmsg => self.show(target, std::slice::from_ref(line), true),
            Event::History { target, lines, .. } => self.show(target, lines, false),
            _ => {}
        }
        let _ = self.store.apply(ev);
    }

    fn show(&mut self, target: &Target, lines: &[hirc_core::store::Line], live: bool) {
        let Some(name) = target.name() else { return };
        for l in lines.iter().filter(|l| l.kind == LineKind::Privmsg && !l.own) {
            let buffer = name.to_ascii_lowercase();
            if !live && self.before.contains(&(buffer.clone(), l.text.clone())) {
                self.refetched.push(format!("{buffer}: {}", l.text));
            }
            self.seen.insert((buffer.clone(), l.text.clone()));
            self.got.push(Got { buffer, text: l.text.clone(), live });
        }
    }

    /// Applies events until `done`. Panics after 10s of nothing.
    async fn until(&mut self, what: &str, mut done: impl FnMut(&T) -> bool) {
        loop {
            if done(self) {
                return;
            }
            let ev = match tokio::time::timeout(Duration::from_secs(10), self.rx.recv()).await {
                Ok(ev) => ev.expect("core stopped"),
                Err(_) => panic!("timed out waiting for {what}; missing {:?}", self.missing()),
            };
            self.take(ev);
        }
    }

    fn missing(&self) -> Vec<&str> {
        self.sent.iter().filter(|(b, t)| !self.seen.contains(&(b.clone(), t.clone()))).map(|(_, t)| t.as_str()).collect()
    }

    async fn all_seen(&mut self) {
        self.until("every line sent", |t| t.missing().is_empty()).await;
    }

    /// Bob sends `text` to `buffer`.
    async fn say(&mut self, buffer: &str, text: &str) {
        self.bob.send(&format!("PRIVMSG {} :{text}", route(buffer))).await;
        self.sent.push((buffer.to_ascii_lowercase(), text.to_owned()));
    }

    /// Bob sends all of `texts` in one write, so they come in the same
    /// millisecond or close to it.
    async fn burst(&mut self, buffer: &str, texts: &[String]) {
        let lines: Vec<String> = texts.iter().map(|t| format!("PRIVMSG {} :{t}", route(buffer))).collect();
        self.bob.send(&lines.join("\r\n")).await;
        self.sent.extend(texts.iter().map(|t| (buffer.to_ascii_lowercase(), t.clone())));
    }

    /// Everything bob sent is at the server.
    async fn bob_sync(&mut self) {
        self.bob.send("PING :sync").await;
        self.bob.expect("PONG").await;
    }

    /// The network dies: no QUIT, nothing more gets through. Returns once
    /// hirc has seen it, and (for a plain account) the server has too.
    async fn lose_link(&mut self) {
        if !self.always_on {
            // QUIT is only seen by someone who shares a channel with us.
            self.until("alice to be in the channels", |t| t.joined >= 1).await;
        }
        self.proxy.down();
        self.until("hirc to see the drop", |t| !t.online).await;
        if !self.always_on {
            self.bob.expect("QUIT").await;
        }
    }

    /// The network is back; hirc's next try gets through.
    async fn restore(&mut self) {
        self.bob_sync().await;
        self.before = self.seen.clone();
        self.filled.clear();
        self.joined = 0;
        self.proxy.up();
    }

    /// Waits until every line is in, `buffers` are filled, a sentinel line
    /// sent after all that has come round, then checks:
    /// - each line was shown exactly once (live or history, never both)
    /// - the store has them in the order they were sent
    async fn settle(&mut self, buffers: &[&str]) {
        let wanted: Vec<String> = buffers.iter().map(|b| b.to_ascii_lowercase()).collect();
        self.until("every line and every history fill", |t| t.online && t.missing().is_empty() && wanted.iter().all(|b| t.filled.contains(b))).await;
        // What the server holds is all in; a line sent now comes after any
        // straggler on the same connection, so once it is in none is left.
        for b in buffers {
            self.sentinels += 1;
            let s = format!("sentinel {}", self.sentinels);
            self.say(b, &s).await;
        }
        self.all_seen().await;
        while let Ok(ev) = self.rx.try_recv() {
            self.take(ev);
        }
        self.verify();
    }

    fn verify(&mut self) {
        let mut buffers: Vec<String> = self.sent.iter().map(|(b, _)| b.clone()).collect();
        buffers.dedup();
        buffers.sort();
        buffers.dedup();
        for b in buffers {
            let want: Vec<String> = self.sent.iter().filter(|(x, _)| *x == b).map(|(_, t)| t.clone()).collect();
            // What the user sees: each line once, in order.
            assert_eq!(said_in(&mut self.store, &target(&b)), want, "{b}: what the store shows (left) against what was sent (right)");
            // Nothing is fetched twice, and nothing shown before the drop
            // comes again. A line that came live while the gap fill was
            // out may come once more in the fill: the store collapses
            // those by msgid, so that pair is allowed.
            for live in [true, false] {
                let mut shown: Vec<&str> = self.got.iter().filter(|g| g.buffer == b && g.live == live).map(|g| g.text.as_str()).collect();
                shown.sort();
                let n = shown.len();
                shown.dedup();
                assert_eq!(n, shown.len(), "{b}: a line came twice as {}", if live { "live" } else { "history" });
            }
            assert!(self.refetched.is_empty(), "shown before the drop, fetched again: {:?}", self.refetched);
        }
    }

    /// hirc goes away and a new one starts on the same history cache.
    fn restart(&mut self, page: Option<usize>, db: PathBuf) {
        self.spawn_hirc(page, Some(db));
    }

    async fn flushed(&self) {
        let flush = self.flush.clone().unwrap();
        assert!(tokio::task::spawn_blocking(move || flush.wait()).await.unwrap(), "the cache did not write");
    }
}

/// Lines while away: `n` of them in one burst, `page` lines per page.
async fn away_burst(page: Option<usize>, n: usize) {
    let mut t = T::start(&["#test"], false, page).await;
    t.say("#test", "before the drop").await;
    t.all_seen().await;
    t.lose_link().await;
    t.burst("#test", &texts("away ", n)).await;
    t.restore().await;
    t.settle(&["#test"]).await;
    assert_eq!(t.connects, 2, "one reconnect");
}

// 1. The network dies with no QUIT; hirc notices and reconnects on its own.
#[tokio::test(flavor = "multi_thread")]
async fn an_unclean_drop_is_noticed_and_the_gap_filled() {
    let mut t = T::start(&["#test"], false, None).await;
    for i in 0..3 {
        t.say("#test", &format!("before {i}")).await;
    }
    t.all_seen().await;
    t.lose_link().await;
    for i in 0..5 {
        t.say("#test", &format!("away {i}")).await;
    }
    t.restore().await;
    t.settle(&["#test"]).await;
    assert_eq!(t.connects, 2, "one reconnect, with no /reconnect");
}

// 2. More than a page while away, in pages of 20 under a server limit of 100.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_of_many_small_pages_arrives_once() {
    away_burst(Some(20), 250).await;
}

// 2. And at the server's own limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_of_many_server_limit_pages_arrives_once() {
    away_burst(Some(10_000), 2 * SERVER_LIMIT + 50).await;
}

// 2. A gap that is exactly whole pages: the last full page is followed by
// an empty one, which must not lose or repeat anything.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_of_exactly_whole_pages_arrives_once() {
    away_burst(Some(20), 60).await;
    away_burst(Some(10_000), SERVER_LIMIT).await;
}

// 2. One line over a page, and one under.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_a_line_either_side_of_a_page_arrives_once() {
    away_burst(Some(20), 21).await;
    away_burst(Some(20), 19).await;
    away_burst(Some(20), 1).await;
}

// 3. Two channels and a query, each with more than a page while away.
#[tokio::test(flavor = "multi_thread")]
async fn gaps_in_several_targets_at_once_all_fill() {
    let mut t = T::start(&["#a", "#b"], true, Some(5)).await;
    t.say("#a", "a before").await;
    t.say("#b", "b before").await;
    t.say("bob", "q before").await;
    t.all_seen().await;
    t.lose_link().await;
    // Interleaved, so the targets' gaps fill side by side.
    for i in 0..13 {
        t.say("#a", &format!("a away {i}")).await;
        t.say("#b", &format!("b away {i}")).await;
        t.say("bob", &format!("q away {i}")).await;
    }
    t.restore().await;
    t.settle(&["#a", "#b", "bob"]).await;
}

// 3. A query from someone alice had not talked to before the drop is found
// (TARGETS) and filled too.
#[tokio::test(flavor = "multi_thread")]
async fn a_query_first_seen_while_away_is_filled() {
    let mut t = T::start(&["#test"], true, Some(5)).await;
    t.say("#test", "before").await;
    t.all_seen().await;
    t.lose_link().await;
    let mut carol = Raw::connect(t.ergo.plain_port, "carol").await;
    for i in 0..8 {
        carol.send(&format!("PRIVMSG alice :hello {i}")).await;
        t.sent.push(("carol".into(), format!("hello {i}")));
    }
    carol.send("PING :sync").await;
    carol.expect("PONG").await;
    t.say("#test", "away").await;
    t.restore().await;
    // A target hirc never had gets its latest page (5 lines); the rest is
    // what scrolling up asks for.
    t.until("carol's latest page", |t| t.online && t.filled.contains("carol")).await;
    assert_eq!(t.missing(), ["hello 0", "hello 1", "hello 2"]);
    t.tx.send(Command::Older { network: "test".into(), target: "carol".into() }).unwrap();
    // Carol's lines are checked with the rest; the sentinel goes to #test
    // (bob cannot speak as carol), after carol's page and her older page.
    t.settle(&["#test"]).await;
}

// 4. Lines sent after hirc has joined but before its gap fill is asked: they
// come live, and the fill must not bring them again.
#[tokio::test(flavor = "multi_thread")]
async fn lines_during_the_reconnect_come_once_not_live_and_filled() {
    let mut t = T::start(&["#test"], false, Some(10)).await;
    t.say("#test", "before").await;
    t.all_seen().await;
    t.lose_link().await;
    for i in 0..3 {
        t.say("#test", &format!("away {i}")).await;
    }
    let hold = t.proxy.hold_on("CHATHISTORY AFTER #test");
    t.restore().await;
    // hirc is joined and its gap fill is held at the relay.
    hold.reached().await;
    for i in 0..3 {
        t.say("#test", &format!("mid {i}")).await;
    }
    t.bob_sync().await;
    t.until("the mid lines live", |t| t.seen.contains(&("#test".into(), "mid 2".into()))).await;
    hold.release();
    // And right behind the request, racing the answer.
    for i in 0..3 {
        t.say("#test", &format!("race {i}")).await;
    }
    t.settle(&["#test"]).await;
}

// 4. The same, with more live lines than a page while the fill is held.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_flood_during_the_reconnect_comes_once() {
    let mut t = T::start(&["#test"], false, Some(10)).await;
    t.say("#test", "before").await;
    t.all_seen().await;
    t.lose_link().await;
    t.burst("#test", &texts("away ", 12)).await;
    let hold = t.proxy.hold_on("CHATHISTORY AFTER #test");
    t.restore().await;
    hold.reached().await;
    t.burst("#test", &texts("mid ", 25)).await;
    t.bob_sync().await;
    t.until("the mid lines live", |t| t.seen.contains(&("#test".into(), "mid 24".into()))).await;
    hold.release();
    t.settle(&["#test"]).await;
}

// 5. A burst in one write straddles the drop: some arrive live, the rest
// are cut off, all in the same millisecond or close to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_same_millisecond_burst_straddling_the_drop_arrives_once() {
    let mut t = T::start(&["#test"], false, Some(10)).await;
    t.say("#test", "before").await;
    t.all_seen().await;
    t.burst("#test", &texts("b", 40)).await;
    // Some of it is in; the drop comes with the rest still on its way.
    t.until("part of the burst", |t| t.seen.contains(&("#test".into(), "b5".into()))).await;
    t.lose_link().await;
    t.burst("#test", &texts("c", 20)).await;
    t.restore().await;
    t.settle(&["#test"]).await;
}

// 5. The burst is all in before the drop, and the drop is right after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_that_ends_at_the_drop_is_not_fetched_again() {
    let mut t = T::start(&["#test"], false, Some(10)).await;
    t.burst("#test", &texts("b", 30)).await;
    t.all_seen().await;
    t.lose_link().await;
    t.say("#test", "after").await;
    t.restore().await;
    t.settle(&["#test"]).await;
}

// 6. Drop and come back three times, each time settled, lines in each gap.
#[tokio::test(flavor = "multi_thread")]
async fn flapping_three_times_loses_and_repeats_nothing() {
    let mut t = T::start(&["#test"], false, Some(10)).await;
    for round in 0..3 {
        t.say("#test", &format!("r{round} before")).await;
        t.all_seen().await;
        t.lose_link().await;
        t.burst("#test", &texts(&format!("r{round} away "), 4)).await;
        t.restore().await;
        t.settle(&["#test"]).await;
    }
    assert_eq!(t.connects, 4);
}

// 6. Drop again as soon as hirc is back, before its gap fill is known to
// be done, with lines in each gap.
#[tokio::test(flavor = "multi_thread")]
async fn flapping_before_the_gap_fill_is_done_loses_and_repeats_nothing() {
    let mut t = T::start(&["#test"], false, Some(5)).await;
    t.say("#test", "before").await;
    t.all_seen().await;
    for round in 0..3 {
        t.lose_link().await;
        t.burst("#test", &texts(&format!("r{round} away "), 12)).await;
        t.restore().await;
        t.until("hirc back", |t| t.online && t.joined >= 1).await;
    }
    t.settle(&["#test"]).await;
}

// 7. The connection dies as the gap fill is asked: the server gets the
// request, hirc never hears the answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_drop_during_the_gap_fetch_still_fills_the_gap() {
    let mut t = T::start(&["#test"], false, Some(10)).await;
    t.say("#test", "before").await;
    t.all_seen().await;
    t.lose_link().await;
    t.burst("#test", &texts("away ", 5)).await;
    t.proxy.sever_on("CHATHISTORY AFTER #test", 1);
    t.restore().await;
    t.until("the second reconnect", |t| t.connects >= 3).await;
    t.settle(&["#test"]).await;
}

// 7. The drop comes after the first page of a gap of several: the pages in
// are kept, the rest asked on from them, nothing twice.
#[tokio::test(flavor = "multi_thread")]
async fn a_drop_between_the_pages_of_a_gap_resumes_from_the_first_page() {
    let mut t = T::start(&["#test"], false, Some(20)).await;
    t.say("#test", "before").await;
    t.all_seen().await;
    t.lose_link().await;
    t.burst("#test", &texts("away ", 70)).await;
    t.proxy.sever_on("CHATHISTORY AFTER #test", 2);
    t.restore().await;
    t.until("the second reconnect", |t| t.connects >= 3).await;
    t.settle(&["#test"]).await;
}

// 7. The drop comes as the very first page of a new hirc is asked (no
// history yet): the next try asks for it again and shows each line once.
// hirc has noted its own JOIN as the newest line by then, so the next try
// asks for what is after the JOIN and the lines from before are never
// fetched.
#[tokio::test(flavor = "multi_thread")]
async fn a_drop_during_the_first_history_page_shows_each_line_once() {
    let mut t = T::prepare(&["#test"], false).await;
    t.burst("#test", &texts("old ", 6)).await;
    t.bob_sync().await;
    t.proxy.sever_on("CHATHISTORY LATEST #test", 1);
    t.spawn_hirc(Some(10), None);
    t.until("the second connection", |t| t.connects >= 2).await;
    t.ready().await;
    t.say("#test", "new").await;
    t.settle(&["#test"]).await;
}

// 8. hirc quits (the user), lines come while it is gone, more than a page;
// the new hirc shows the cache, then only what came after.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_after_a_gap_of_many_pages_fills_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.db");
    let mut t = T::prepare(&["#test"], false).await;
    t.spawn_hirc(Some(20), Some(db.clone()));
    t.ready().await;
    t.burst("#test", &texts("m", 5)).await;
    t.all_seen().await;
    t.tx.send(Command::Disconnect { network: "test".into(), reason: None }).unwrap();
    t.until("offline", |t| !t.online).await;
    t.bob.expect("QUIT").await;
    t.burst("#test", &texts("gap ", 70)).await;
    t.bob_sync().await;
    t.flushed().await;
    t.restart(Some(20), db);
    t.settle(&["#test"]).await;
    // The cache comes first, and it is the lines from before.
    let first: Vec<&str> = t.got.iter().take(5).map(|g| g.text.as_str()).collect();
    assert_eq!(first, ["m0", "m1", "m2", "m3", "m4"]);
}

// 8. hirc is killed (not asked to quit) while the network is down: the new
// one fills what came meanwhile, from the cache's newest line.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_after_an_unclean_drop_fills_the_gap_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.db");
    let mut t = T::prepare(&["#test"], false).await;
    t.spawn_hirc(Some(20), Some(db.clone()));
    t.ready().await;
    t.burst("#test", &texts("m", 5)).await;
    t.all_seen().await;
    t.lose_link().await;
    t.burst("#test", &texts("gap ", 45)).await;
    t.bob_sync().await;
    t.flushed().await;
    // The old hirc is stopped before the network returns.
    t.tx = tokio::sync::mpsc::unbounded_channel().0;
    // Its core stops, and its events end, once it sees the commands close.
    while tokio::time::timeout(Duration::from_secs(10), t.rx.recv()).await.expect("the old hirc did not stop").is_some() {}
    t.restore().await;
    t.restart(Some(20), db);
    t.settle(&["#test"]).await;
}

// 9. The user's own /reconnect, with lines sent as it happens.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_command_with_lines_in_flight_loses_nothing() {
    let mut t = T::start(&["#test"], false, Some(10)).await;
    t.say("#test", "before").await;
    t.all_seen().await;
    t.tx.send(Command::Reconnect { network: "test".into() }).unwrap();
    for i in 0..30 {
        t.say("#test", &format!("during {i}")).await;
    }
    t.bob_sync().await;
    t.until("the reconnect", |t| t.connects >= 2).await;
    t.settle(&["#test"]).await;
}

// A link that goes dead with nothing closed (a dropped Wi-Fi, a sleeping
// NAT): no end of file, no reset, just silence. hirc hears nothing, pings,
// hears nothing back, gives up on its own, and fills the gap once the link
// is back. One-second `ping_time` and `ping_timeout` keep it short.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_link_is_given_up_on_and_the_gap_filled() {
    let mut t = T::prepare(&["#test"], false).await;
    t.ping = Some(1);
    t.spawn_hirc(None, None);
    t.ready().await;
    t.say("#test", "before the silence").await;
    t.all_seen().await;
    t.proxy.silence();
    for i in 0..5 {
        t.say("#test", &format!("unheard {i}")).await;
    }
    t.until("hirc to give up on the quiet link", |t| !t.online).await;
    assert!(t.drops.iter().any(|r| r.contains("no reply from server")), "dropped for another reason: {:?}", t.drops);
    // The server sees the connection end only now, as hirc hangs up.
    t.bob.expect("QUIT").await;
    t.restore().await;
    t.settle(&["#test"]).await;
    assert_eq!(t.connects, 2, "one reconnect, with no /reconnect");
}
