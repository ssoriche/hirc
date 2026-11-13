//! No line lost or shown twice when hirc reconnects through a bouncer:
//! ZNC (playback module) and soju, each in front of a throwaway Ergo.
//! Inside the flox environment:
//!   scripts/ergo-test.sh setup
//!   cargo test -p hirc-core --features bouncer-tests --test reconnect_bouncer
//!
//! Every test ends the same way: each line bob sent reached us exactly
//! once, in order, in the store and among the events (live or history),
//! and nothing seen before a drop came again. One allowance, for ZNC
//! only: `PLAY` takes whole seconds, so ZNC sends again the lines from
//! the second of the newest line held (`playback::since`). Those may come
//! once more as history, and the store collapses them by msgid.

#![cfg(feature = "bouncer-tests")]

mod common;

use chrono::{DateTime, Utc};
use common::events::{net, next_matching, said_in};
use common::proxy::Proxy as Relay;
use common::soju::{self, Soju};
use common::znc::{self, Znc};
use common::{Ergo, Raw, register, wait_in_channel};
use hirc_core::config::{Config, Server};
use hirc_core::event::{Command, ConnStatus, Event, Target};
use hirc_core::store::{Line, LineKind, Store};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Znc,
    Soju,
}

/// A bouncer in front of its own Ergo, and the config for hirc through it.
struct Rig {
    ergo: Ergo,
    znc: Option<Znc>,
    soju: Option<Soju>,
    /// What hirc connects to: the bouncer's port, or a proxy's.
    port: u16,
    channels: Vec<String>,
}

impl Rig {
    async fn start(kind: Kind) -> Rig {
        let ergo = Ergo::start();
        let (znc, soju) = match kind {
            Kind::Znc => {
                let z = Znc::start(&ergo);
                wait_in_channel(&ergo, znc::USER, "#test").await;
                let port = z.port;
                (Some((z, port)), None)
            }
            Kind::Soju => {
                let s = Soju::start(&ergo).await;
                let port = s.port;
                (None, Some((s, port)))
            }
        };
        let port = znc.as_ref().map(|z| z.1).or(soju.as_ref().map(|s| s.1)).unwrap();
        Rig { ergo, znc: znc.map(|z| z.0), soju: soju.map(|s| s.0), port, channels: vec!["#test".into()] }
    }

    fn kind(&self) -> Kind {
        if self.znc.is_some() { Kind::Znc } else { Kind::Soju }
    }

    fn name(&self) -> &'static str {
        match self.kind() {
            Kind::Znc => "znc",
            Kind::Soju => "soju",
        }
    }

    /// The bouncer's own port, whatever hirc is pointed at.
    fn bouncer_port(&self) -> u16 {
        self.znc.as_ref().map(|z| z.port).or(self.soju.as_ref().map(|s| s.port)).unwrap()
    }

    fn config(&self) -> Config {
        let mut config = Config::default();
        let base = Server {
            nickname: znc::USER.into(),
            server: "127.0.0.1".into(),
            port: Some(self.port),
            use_tls: false,
            channels: self.channels.clone(),
            ..Server::default()
        };
        let server = match self.kind() {
            Kind::Znc => Server { password: Some(znc::pass()), ..base },
            Kind::Soju => Server { username: Some(soju::username()), password: Some(soju::PASSWORD.into()), ..base },
        };
        config.servers.insert(self.name().into(), server);
        config
    }

    /// Waits until the bouncer holds `text` for `target` (`#chan` or a
    /// nick). The line reaching Ergo only proves bob's side of it.
    async fn holds(&self, target: &str, text: &str) {
        if let Some(soju) = &self.soju {
            soju.holds(target, &[text]).await;
            return;
        }
        let znc = self.znc.as_ref().unwrap();
        let mut held = Vec::new();
        for _ in 0..50 {
            let (mut check, _) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &["batch", "server-time", "znc.in/playback"]).await;
            check.send("PRIVMSG *playback :PLAY * 0").await;
            check.send("PING :hirc-done").await;
            held = check.until("hirc-done").await;
            check.send("QUIT").await;
            if held.iter().any(|l| l.contains("PRIVMSG") && l.ends_with(text)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("ZNC never held {text:?}: {held:?}");
    }

    /// Our network joins `chan` through the bouncer (hirc's config).
    fn with_channels(mut self, chans: &[&str]) -> Rig {
        self.channels = chans.iter().map(|c| (*c).to_owned()).collect();
        self
    }
}

async fn join(ergo: &Ergo, nick: &str, chans: &[&str]) -> Raw {
    let mut c = Raw::connect(ergo.plain_port, nick).await;
    for chan in chans {
        c.send(&format!("JOIN {chan}")).await;
        c.expect(" JOIN ").await;
    }
    c
}

async fn say(bob: &mut Raw, target: &str, texts: &[&str]) {
    for t in texts {
        bob.send(&format!("PRIVMSG {target} :{t}")).await;
    }
}

/// Waits until the server has handled everything `c` sent.
async fn synced(c: &mut Raw) {
    c.send("PING :synced").await;
    c.expect("synced").await;
}

/// One line bob sent to us, as it arrived.
#[derive(Clone, Debug)]
struct Arrival {
    target: String,
    text: String,
    live: bool,
    time: DateTime<Utc>,
}

/// Every line of bob's that arrived, live or as history, in the order it
/// came, plus the newest time seen at each drop.
#[derive(Default)]
struct Tape {
    all: Vec<Arrival>,
    cuts: Vec<DateTime<Utc>>,
}

fn from_bob(l: &Line) -> bool {
    l.kind == LineKind::Privmsg && l.nick.as_deref() == Some("bob")
}

impl Tape {
    fn note(&mut self, e: &Event) {
        let mut push = |target: &Target, l: &Line, live: bool| {
            if from_bob(l) {
                self.all.push(Arrival { target: target.name().unwrap_or_default().to_ascii_lowercase(), text: l.text.clone(), live, time: l.time });
            }
        };
        match e {
            Event::Line { target, line, .. } => push(target, line, true),
            Event::History { target, lines, .. } => lines.iter().for_each(|l| push(target, l, false)),
            _ => {}
        }
    }

    fn of(&self, target: &str, text: &str) -> Vec<&Arrival> {
        self.all.iter().filter(|a| a.target.eq_ignore_ascii_case(target) && a.text == text).collect()
    }

    fn mark_drop(&mut self) {
        self.cuts.extend(self.all.iter().map(|a| a.time).max());
    }
}

/// hirc under test: its events applied to a store and noted on a tape.
struct Run {
    tx: UnboundedSender<Command>,
    rx: UnboundedReceiver<Event>,
    store: Store,
    tape: Tape,
    network: &'static str,
}

impl Run {
    fn start(rig: &Rig) -> Run {
        let (tx, rx) = net(rig.config());
        Run::from(tx, rx, rig.name())
    }

    fn from(tx: UnboundedSender<Command>, rx: UnboundedReceiver<Event>, network: &'static str) -> Run {
        Run { tx, rx, store: Store::new([network.to_owned()]), tape: Tape::default(), network }
    }

    /// Applies events until `f` says so (that one too).
    async fn until(&mut self, mut f: impl FnMut(&Event) -> bool) {
        let (store, tape) = (&mut self.store, &mut self.tape);
        next_matching(&mut self.rx, |e| {
            store.apply(e.clone());
            tape.note(e);
            f(e).then_some(())
        })
        .await;
    }

    async fn joined(&mut self, chan: &str) {
        self.until(|e| matches!(e, Event::Joined { channel, .. } if channel.eq_ignore_ascii_case(chan))).await;
    }

    /// Until every one of `texts` has arrived in `target`, at least once.
    async fn gets(&mut self, target: &str, texts: &[&str]) {
        let (store, tape) = (&mut self.store, &mut self.tape);
        let pending = |tape: &Tape| texts.iter().any(|t| tape.of(target, t).is_empty());
        if !pending(tape) {
            return;
        }
        next_matching(&mut self.rx, |e| {
            store.apply(e.clone());
            tape.note(e);
            (!pending(tape)).then_some(())
        })
        .await;
    }

    /// Until `n` copies of `text` have arrived in `target`.
    async fn gets_n(&mut self, target: &str, text: &str, n: usize) {
        let (store, tape) = (&mut self.store, &mut self.tape);
        if tape.of(target, text).len() >= n {
            return;
        }
        next_matching(&mut self.rx, |e| {
            store.apply(e.clone());
            tape.note(e);
            (tape.of(target, text).len() >= n).then_some(())
        })
        .await;
    }

    /// We go: a Disconnect, and the wait for it to take.
    async fn drop_connection(&mut self) {
        self.tape.mark_drop();
        self.tx.send(Command::Disconnect { network: self.network.into(), reason: None }).unwrap();
        self.until(|e| matches!(e, Event::Status { status: ConnStatus::Offline, .. })).await;
    }

    fn reconnect(&self) {
        self.tx.send(Command::Reconnect { network: self.network.into() }).unwrap();
    }

    /// Waits for the connection to be registered again.
    async fn connected(&mut self) {
        self.until(|e| matches!(e, Event::Status { status: ConnStatus::Connected { .. }, .. })).await;
    }

    /// Nothing marks "no more is coming", so give a stray duplicate time
    /// to show, then take whatever is queued.
    async fn settle(&mut self) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        while let Ok(e) = self.rx.try_recv() {
            self.store.apply(e.clone());
            self.tape.note(&e);
        }
    }

    /// `want` is what bob sent `target`, oldest first: the store holds
    /// exactly that, and each line came the right number of times.
    async fn check(&mut self, kind: Kind, target: &Target, want: &[&str]) {
        self.settle().await;
        assert_eq!(said_in(&mut self.store, target), want, "the store, {target:?}");
        let name = target.name().unwrap();
        let mut texts: Vec<&str> = want.to_vec();
        texts.sort();
        texts.dedup();
        for text in texts {
            let expected = want.iter().filter(|w| **w == text).count();
            let got = self.tape.of(name, text);
            assert!(got.len() >= expected, "{text:?} lost in {name}: {got:?}");
            // ZNC's PLAY rounds down to the second of the newest line held:
            // a line of that second, seen before the drop, may be replayed.
            let again = if kind == Kind::Znc {
                got.iter().filter(|a| !a.live && self.tape.cuts.iter().any(|c| a.time <= *c && a.time.timestamp() == c.timestamp())).count()
            } else {
                0
            };
            assert!(got.len() - expected <= again, "{text:?} came {} times in {name}, wanted {expected} (+{again} allowed): {got:?}", got.len());
        }
    }
}

fn chan(name: &str) -> Target {
    Target::Channel(name.into())
}

fn query(name: &str) -> Target {
    Target::Query(name.into())
}

// 1. Several reconnects in a row, each with new lines in between.

async fn several_reconnects(kind: Kind) {
    let rig = Rig::start(kind).await;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let mut run = Run::start(&rig);
    run.joined("#test").await;
    let mut want: Vec<String> = Vec::new();
    for round in 1..=4 {
        // Live while attached, then lines while away.
        let (live, away) = (format!("live {round}"), [format!("away {round}a"), format!("away {round}b")]);
        say(&mut bob, "#test", &[&live]).await;
        run.gets("#test", &[&live]).await;
        run.drop_connection().await;
        say(&mut bob, "#test", &[&away[0], &away[1]]).await;
        rig.holds("#test", &away[1]).await;
        run.reconnect();
        want.push(live);
        want.extend(away.iter().cloned());
        let w: Vec<&str> = want.iter().map(String::as_str).collect();
        run.gets("#test", &w).await;
    }
    let w: Vec<&str> = want.iter().map(String::as_str).collect();
    run.check(kind, &chan("#test"), &w).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_several_reconnects_in_a_row_lose_and_repeat_nothing() {
    several_reconnects(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_several_reconnects_in_a_row_lose_and_repeat_nothing() {
    several_reconnects(Kind::Soju).await;
}

// 2. Lines in several targets while away: two channels and a query.

async fn several_targets(kind: Kind) {
    let rig = Rig::start(kind).await.with_channels(&["#test", "#other"]);
    let mut bob = join(&rig.ergo, "bob", &["#test", "#other"]).await;
    let mut run = Run::start(&rig);
    run.joined("#test").await;
    run.joined("#other").await;
    wait_in_channel(&rig.ergo, znc::USER, "#other").await;
    say(&mut bob, "#test", &["t before"]).await;
    say(&mut bob, "#other", &["o before"]).await;
    say(&mut bob, "alice", &["q before"]).await;
    run.gets("#test", &["t before"]).await;
    run.gets("#other", &["o before"]).await;
    run.gets("bob", &["q before"]).await;
    run.drop_connection().await;
    // Interleaved, so no target's lines are contiguous.
    for i in 1..=3 {
        say(&mut bob, "#other", &[&format!("o away {i}")]).await;
        say(&mut bob, "alice", &[&format!("q away {i}")]).await;
        say(&mut bob, "#test", &[&format!("t away {i}")]).await;
    }
    synced(&mut bob).await;
    rig.holds("#test", "t away 3").await;
    rig.holds("#other", "o away 3").await;
    rig.holds("bob", "q away 3").await;
    run.reconnect();
    run.gets("#test", &["t away 3"]).await;
    run.gets("#other", &["o away 3"]).await;
    run.gets("bob", &["q away 3"]).await;
    run.check(kind, &chan("#test"), &["t before", "t away 1", "t away 2", "t away 3"]).await;
    run.check(kind, &chan("#other"), &["o before", "o away 1", "o away 2", "o away 3"]).await;
    run.check(kind, &query("bob"), &["q before", "q away 1", "q away 2", "q away 3"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_lines_in_two_channels_and_a_query_while_away_all_come_once() {
    several_targets(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_lines_in_two_channels_and_a_query_while_away_all_come_once() {
    several_targets(Kind::Soju).await;
}

// 3. Lines sent while hirc is mid-reconnect, or leaving.

/// How hirc leaves the bouncer while bob is talking.
#[derive(Clone, Copy)]
enum Leave {
    /// `/reconnect` on a live connection: QUIT, then straight back.
    Reconnect,
    /// `/disconnect`, and a `/reconnect` once bob is done.
    Disconnect,
}

async fn while_bob_talks(kind: Kind, leave: Leave) {
    let rig = Rig::start(kind).await;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let mut run = Run::start(&rig);
    run.joined("#test").await;
    say(&mut bob, "#test", &["before"]).await;
    run.gets("#test", &["before"]).await;
    // (40 lines: under ZNC's 50-line buffer.) bob keeps talking, one line
    // every few ms, as hirc goes: some lines
    // land in the detach, some in the attach and the replay, some after.
    run.tape.mark_drop();
    let talker = tokio::spawn(async move {
        for i in 1..=40 {
            bob.send(&format!("PRIVMSG #test :stream {i}")).await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        bob
    });
    match leave {
        Leave::Reconnect => run.reconnect(),
        Leave::Disconnect => {
            run.tx.send(Command::Disconnect { network: run.network.into(), reason: None }).unwrap();
            run.until(|e| matches!(e, Event::Status { status: ConnStatus::Offline, .. })).await;
        }
    }
    let mut bob = talker.await.unwrap();
    synced(&mut bob).await;
    if matches!(leave, Leave::Disconnect) {
        rig.holds("#test", "stream 40").await;
        run.reconnect();
    }
    let streamed: Vec<String> = (1..=40).map(|i| format!("stream {i}")).collect();
    let mut want = vec!["before"];
    want.extend(streamed.iter().map(String::as_str));
    run.gets("#test", &["stream 40"]).await;
    run.check(kind, &chan("#test"), &want).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_lines_sent_while_we_reconnect_arrive_once() {
    while_bob_talks(Kind::Znc, Leave::Reconnect).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_lines_sent_while_we_reconnect_arrive_once() {
    while_bob_talks(Kind::Soju, Leave::Reconnect).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_lines_sent_while_we_disconnect_arrive_once() {
    while_bob_talks(Kind::Znc, Leave::Disconnect).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_lines_sent_while_we_disconnect_arrive_once() {
    while_bob_talks(Kind::Soju, Leave::Disconnect).await;
}

// 4. A burst in one millisecond around the drop.

async fn burst_around_the_drop(kind: Kind) {
    let rig = Rig::start(kind).await;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let mut run = Run::start(&rig);
    run.joined("#test").await;
    let mut want: Vec<String> = Vec::new();
    // Rounds, as the drop may or may not fall inside the burst: each
    // sends five lines at once and disconnects the moment the first
    // arrives, so a later line of the burst may be seen or not.
    for round in 1..=6 {
        let burst: Vec<String> = (1..=5).map(|i| format!("r{round} burst {i}")).collect();
        let refs: Vec<&str> = burst.iter().map(String::as_str).collect();
        bob.send(&refs.iter().map(|t| format!("PRIVMSG #test :{t}")).collect::<Vec<_>>().join("\r\n")).await;
        run.gets("#test", &[&burst[0]]).await;
        run.drop_connection().await;
        let away: Vec<String> = (1..=3).map(|i| format!("r{round} away {i}")).collect();
        let refs: Vec<&str> = away.iter().map(String::as_str).collect();
        bob.send(&refs.iter().map(|t| format!("PRIVMSG #test :{t}")).collect::<Vec<_>>().join("\r\n")).await;
        synced(&mut bob).await;
        rig.holds("#test", &burst[4]).await;
        rig.holds("#test", &away[2]).await;
        run.reconnect();
        want.extend(burst);
        want.extend(away);
        let w: Vec<&str> = want.iter().map(String::as_str).collect();
        run.gets("#test", &w).await;
    }
    let w: Vec<&str> = want.iter().map(String::as_str).collect();
    run.check(kind, &chan("#test"), &w).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_a_burst_in_one_millisecond_around_the_drop_comes_whole_and_once() {
    burst_around_the_drop(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_a_burst_in_one_millisecond_around_the_drop_comes_whole_and_once() {
    burst_around_the_drop(Kind::Soju).await;
}

// 5. A hirc restart on the same history cache.

async fn restart_with_cache(kind: Kind) {
    let rig = Rig::start(kind).await;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let dir = tempfile::tempdir().unwrap();
    let db: PathBuf = dir.path().join("history.db");
    let (tx, rx, flush) = hirc_core::conn::spawn(rig.config(), Some(db.clone()));
    let mut run = Run::from(tx, rx, rig.name());
    run.joined("#test").await;
    say(&mut bob, "#test", &["seen 1", "seen 2"]).await;
    run.gets("#test", &["seen 1", "seen 2"]).await;
    assert!(tokio::task::spawn_blocking(move || flush.wait()).await.unwrap());
    run.tape.mark_drop();
    let cuts = run.tape.cuts.clone();
    drop(run);
    // hirc is gone: the bouncer detaches and keeps what comes.
    say(&mut bob, "#test", &["down 1", "down 2", "down 3"]).await;
    synced(&mut bob).await;
    rig.holds("#test", "down 3").await;

    let (tx, rx, _flush) = hirc_core::conn::spawn(rig.config(), Some(db.clone()));
    let mut run = Run::from(tx, rx, rig.name());
    run.tape.cuts = cuts;
    // The cache's page comes first, then the replay of what was missed.
    run.gets("#test", &["seen 1", "seen 2", "down 1", "down 2", "down 3"]).await;
    // And live lines after it go on.
    say(&mut bob, "#test", &["live"]).await;
    run.gets("#test", &["live"]).await;
    run.check(kind, &chan("#test"), &["seen 1", "seen 2", "down 1", "down 2", "down 3", "live"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_a_restart_shows_the_cache_and_the_replay_each_line_once() {
    restart_with_cache(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_a_restart_shows_the_cache_and_the_replay_each_line_once() {
    restart_with_cache(Kind::Soju).await;
}

// 6. The bouncer's own upstream connection drops while hirc stays attached.

async fn upstream_drops(kind: Kind) {
    let rig = Rig::start(kind).await;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let mut run = Run::start(&rig);
    run.joined("#test").await;
    say(&mut bob, "#test", &["before"]).await;
    run.gets("#test", &["before"]).await;
    // Another client of ours asks the bouncer to drop its upstream.
    match kind {
        Kind::Znc => {
            let (mut admin, _) = register(rig.bouncer_port(), &znc::pass(), znc::USER, znc::USER, &["server-time"]).await;
            admin.send("PRIVMSG *status :Disconnect").await;
            bob.expect(":alice!").await;
            admin.send("PRIVMSG *status :Connect").await;
        }
        Kind::Soju => {
            // Changing a network's settings makes soju drop and redial it.
            let (mut admin, _) = register(rig.bouncer_port(), soju::PASSWORD, soju::USER, &soju::username(), &["server-time"]).await;
            admin.send("PRIVMSG BouncerServ :network update ergo -realname redial").await;
            // bob sees alice quit, so the upstream really dropped.
            bob.expect(":alice!").await;
        }
    }
    // alice is back in the channel: bob sees her join.
    let back = bob.expect(" JOIN ").await;
    assert!(back.contains(":alice!"), "{back}");
    // hirc sees the rejoin and asks for the channel's latest page. A line
    // that lands while that page is on its way is a race of its own, not
    // the drop's (test 11). Let the page come first. No event marks it,
    // so a short sleep.
    run.settle().await;
    // Lines after the upstream is back, while hirc never detached.
    say(&mut bob, "#test", &["after 1", "after 2"]).await;
    run.gets("#test", &["after 1", "after 2"]).await;
    run.check(kind, &chan("#test"), &["before", "after 1", "after 2"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_its_upstream_dropping_loses_and_repeats_nothing_for_us() {
    upstream_drops(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_its_upstream_dropping_loses_and_repeats_nothing_for_us() {
    upstream_drops(Kind::Soju).await;
}

// 7. ZNC: identical text from one nick in the same second across the gap.

/// Waits for a moment early in a second, so two lines 30 ms apart share it.
async fn early_in_a_second() {
    while Utc::now().timestamp_subsec_millis() > 400 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn identical_lines(kind: Kind) {
    let rig = Rig::start(kind).await;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let mut run = Run::start(&rig);
    run.joined("#test").await;
    // Early in a second, so the three lines share it, 30 ms apart or more:
    // distinct milliseconds, which is all a made-up id has to tell them by.
    early_in_a_second().await;
    say(&mut bob, "#test", &["same"]).await;
    run.gets_n("#test", "same", 1).await;
    run.drop_connection().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    say(&mut bob, "#test", &["same"]).await;
    synced(&mut bob).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    say(&mut bob, "#test", &["same"]).await;
    synced(&mut bob).await;
    // The held copies: wait for the bouncer to have both.
    rig.holds("#test", "same").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    run.reconnect();
    run.gets_n("#test", "same", 3).await;
    run.check(kind, &chan("#test"), &["same", "same", "same"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_identical_lines_in_one_second_across_the_gap_stay_separate() {
    identical_lines(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_identical_lines_in_one_second_across_the_gap_stay_separate() {
    identical_lines(Kind::Soju).await;
}

// 8. Reconnecting again before the replay of the last one is done.

async fn back_to_back(kind: Kind) {
    let rig = Rig::start(kind).await;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let mut run = Run::start(&rig);
    run.joined("#test").await;
    say(&mut bob, "#test", &["before"]).await;
    run.gets("#test", &["before"]).await;
    run.drop_connection().await;
    let away: Vec<String> = (1..=10).map(|i| format!("away {i}")).collect();
    for t in &away {
        say(&mut bob, "#test", &[t]).await;
    }
    synced(&mut bob).await;
    rig.holds("#test", "away 10").await;
    // Three connections in a row; each is dropped as soon as it registers,
    // before its replay can be done.
    for _ in 0..3 {
        run.reconnect();
        run.connected().await;
    }
    let mut want = vec!["before"];
    want.extend(away.iter().map(String::as_str));
    run.gets("#test", &want).await;
    run.check(kind, &chan("#test"), &want).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_reconnecting_again_before_the_replay_is_done_loses_nothing() {
    back_to_back(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_reconnecting_again_before_the_replay_is_done_loses_nothing() {
    back_to_back(Kind::Soju).await;
}

// 9. The connection dies without a QUIT (a cut TCP stream), and hirc's own
// retry brings it back.

/// A TCP forwarder to the bouncer that can cut every connection at once:
/// a network dropping, not hirc leaving.
struct Proxy {
    port: u16,
    cut: tokio::sync::watch::Sender<u32>,
}

impl Proxy {
    async fn start(to: u16) -> Proxy {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (cut, watch) = tokio::sync::watch::channel(0u32);
        tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else { return };
                // A clone starts at the version seen so far: mark it seen, or a
                // new connection looks cut already.
                let mut watch = watch.clone();
                watch.borrow_and_update();
                tokio::spawn(async move {
                    let Ok(mut outbound) = tokio::net::TcpStream::connect(("127.0.0.1", to)).await else { return };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                        _ = watch.changed() => {}
                    }
                });
            }
        });
        Proxy { port, cut }
    }

    fn cut(&self) {
        self.cut.send_modify(|n| *n += 1);
    }
}

async fn cut_stream(kind: Kind) {
    let mut rig = Rig::start(kind).await;
    let proxy = Proxy::start(rig.bouncer_port()).await;
    rig.port = proxy.port;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let mut run = Run::start(&rig);
    run.joined("#test").await;
    say(&mut bob, "#test", &["before"]).await;
    run.gets("#test", &["before"]).await;
    let mut want = vec!["before".to_owned()];
    for round in 1..=2 {
        run.tape.mark_drop();
        // The network goes: no QUIT from us. hirc retries after a second,
        // so bob's lines come in the gap, or live once it is back.
        proxy.cut();
        run.until(|e| matches!(e, Event::Status { status: ConnStatus::Disconnected { .. }, .. })).await;
        let away = [format!("cut {round} a"), format!("cut {round} b")];
        say(&mut bob, "#test", &[&away[0], &away[1]]).await;
        synced(&mut bob).await;
        rig.holds("#test", &away[1]).await;
        want.extend(away);
        let w: Vec<&str> = want.iter().map(String::as_str).collect();
        run.gets("#test", &w).await;
    }
    let w: Vec<&str> = want.iter().map(String::as_str).collect();
    run.check(kind, &chan("#test"), &w).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_a_cut_connection_is_retried_and_loses_nothing() {
    cut_stream(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_a_cut_connection_is_retried_and_loses_nothing() {
    cut_stream(Kind::Soju).await;
}

// 10. A restart onto a cache when a query and a channel were last seen at
// different times (ZNC's PLAY takes one time for every buffer).

async fn restart_two_buffers(kind: Kind) {
    let rig = Rig::start(kind).await.with_channels(&["#test", "#other"]);
    let mut bob = join(&rig.ergo, "bob", &["#test", "#other"]).await;
    let dir = tempfile::tempdir().unwrap();
    let db: PathBuf = dir.path().join("history.db");
    let (tx, rx, flush) = hirc_core::conn::spawn(rig.config(), Some(db.clone()));
    let mut run = Run::from(tx, rx, rig.name());
    run.joined("#test").await;
    run.joined("#other").await;
    wait_in_channel(&rig.ergo, znc::USER, "#other").await;
    // #other's last line is old; #test's newer. PLAY's one time is the
    // newest held anywhere, so what #other gets later must still be
    // after it.
    say(&mut bob, "#other", &["o old"]).await;
    run.gets("#other", &["o old"]).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    say(&mut bob, "#test", &["t new"]).await;
    run.gets("#test", &["t new"]).await;
    assert!(tokio::task::spawn_blocking(move || flush.wait()).await.unwrap());
    run.tape.mark_drop();
    let cuts = run.tape.cuts.clone();
    drop(run);
    say(&mut bob, "#other", &["o down"]).await;
    say(&mut bob, "#test", &["t down"]).await;
    synced(&mut bob).await;
    rig.holds("#other", "o down").await;
    rig.holds("#test", "t down").await;
    let (tx, rx, _flush) = hirc_core::conn::spawn(rig.config(), Some(db.clone()));
    let mut run = Run::from(tx, rx, rig.name());
    run.tape.cuts = cuts;
    run.gets("#other", &["o old", "o down"]).await;
    run.gets("#test", &["t new", "t down"]).await;
    run.check(kind, &chan("#other"), &["o old", "o down"]).await;
    run.check(kind, &chan("#test"), &["t new", "t down"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_a_restart_with_buffers_last_seen_at_different_times_loses_nothing() {
    restart_two_buffers(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_a_restart_with_buffers_last_seen_at_different_times_loses_nothing() {
    restart_two_buffers(Kind::Soju).await;
}

// 11. A line that comes live while our first ask for history (soju's
// LATEST, ZNC's PLAY) is on its way: the bouncer holds the line by the
// time it reads the ask, so its answer brings the line again. A slow
// runner meets this unforced; the relay makes it certain.

async fn live_before_the_answer(kind: Kind) {
    let mut rig = Rig::start(kind).await;
    let relay = Relay::start(rig.bouncer_port()).await;
    rig.port = relay.port;
    let mut bob = join(&rig.ergo, "bob", &["#test"]).await;
    let hold = relay.hold_on(match kind {
        Kind::Znc => "PRIVMSG *playback :PLAY",
        Kind::Soju => "CHATHISTORY LATEST #test",
    });
    let mut run = Run::start(&rig);
    hold.reached().await;
    say(&mut bob, "#test", &["early"]).await;
    run.gets("#test", &["early"]).await;
    rig.holds("#test", "early").await;
    hold.release();
    say(&mut bob, "#test", &["after"]).await;
    run.gets("#test", &["after"]).await;
    run.check(kind, &chan("#test"), &["early", "after"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_a_line_live_before_the_play_is_answered_comes_once() {
    live_before_the_answer(Kind::Znc).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_a_line_live_before_the_latest_page_comes_once() {
    live_before_the_answer(Kind::Soju).await;
}
