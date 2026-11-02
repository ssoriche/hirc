//! hirc-core through real bouncers: ZNC 1.10 with its playback module,
//! and soju, each in front of a throwaway Ergo. Inside the flox
//! environment:
//!   scripts/ergo-test.sh setup
//!   cargo test -p hirc-core --features bouncer-tests --test bouncer
//! `-- --ignored` re-records the transcripts in `tests/data/`.

#![cfg(feature = "bouncer-tests")]

mod common;

use chrono::{DateTime, TimeDelta, Utc};
use common::events::{applied_until, joined, line_in, net, next_matching, offline, said, said_in};
use common::soju::{self, Soju};
use common::znc::{self, Znc};
use common::{Ergo, Raw, record, register, wait_in_channel};
use hirc_core::config::{Config, Server};
use hirc_core::event::{Command, ConnStatus, Event, Target};
use hirc_core::history::timestamp;
use hirc_core::proto::Message;
use hirc_core::store::Store;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// hirc through ZNC, as network `znc`: plaintext on loopback, with
/// `PASS user/network:password`, the fixed test password.
fn via_znc(znc: &Znc) -> Config {
    let mut config = Config::default();
    config.servers.insert(
        "znc".into(),
        Server {
            nickname: znc::USER.into(),
            server: "127.0.0.1".into(),
            port: Some(znc.port),
            use_tls: false,
            password: Some(znc::pass()),
            channels: vec!["#test".into()],
            ..Server::default()
        },
    );
    config
}

/// hirc through soju, as network `soju`: `user/network` binds the
/// connection to its one network.
fn via_soju(soju: &Soju) -> Config {
    let mut config = Config::default();
    config.servers.insert(
        "soju".into(),
        Server {
            nickname: soju::USER.into(),
            username: Some(soju::username()),
            server: "127.0.0.1".into(),
            port: Some(soju.port),
            use_tls: false,
            password: Some(soju::PASSWORD.into()),
            channels: vec!["#test".into()],
            ..Server::default()
        },
    );
    config
}

/// bob, straight on the Ergo behind the bouncer, in `#test`.
async fn join_bob(ergo: &Ergo) -> Raw {
    let mut bob = Raw::connect(ergo.plain_port, "bob").await;
    bob.send("JOIN #test").await;
    bob.expect("JOIN").await;
    bob
}

/// Waits until the server has handled everything `c` sent.
async fn synced(c: &mut Raw) {
    c.send("PING :synced").await;
    c.expect("synced").await;
}

/// Sends `cmd`, then a PING, and gives back every line up to its PONG,
/// with `cmd` first, marked `> `.
async fn ask(c: &mut Raw, cmd: &str) -> Vec<String> {
    c.send(cmd).await;
    c.send("PING :hirc-done").await;
    let mut got = vec![format!("> {cmd}")];
    got.extend(c.until("hirc-done").await);
    got
}

/// The `time` tag of the first line ending in `text`.
fn time_of(lines: &[String], text: &str) -> DateTime<Utc> {
    lines
        .iter()
        .filter(|l| l.ends_with(text))
        .find_map(|l| Message::parse(l).ok()?.tag("time").map(str::to_owned))
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("no timed line saying {text:?} in {lines:?}"))
}

/// Only the `CAP ... LS` lines.
fn cap_ls(lines: &[String]) -> Vec<String> {
    lines.iter().filter(|l| l.contains(" CAP ") && l.contains(" LS ")).cloned().collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn znc_relays_a_line_and_stays_in_the_channel_when_we_go() {
    let ergo = Ergo::start();
    let znc = Znc::start(&ergo);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    let mut bob = join_bob(&ergo).await;
    let (tx, mut rx) = net(via_znc(&znc));
    joined(&mut rx, "#test").await;
    bob.send("PRIVMSG #test :hello through znc").await;
    let got = next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.text == "hello through znc")).await;
    assert_eq!(got.nick.as_deref(), Some("bob"));
    tx.send(Command::Disconnect { network: "znc".into(), reason: None }).unwrap();
    offline(&mut rx).await;
    // A client's QUIT only detaches it: ZNC stays in the channel.
    tokio::time::sleep(Duration::from_millis(500)).await;
    wait_in_channel(&ergo, znc::USER, "#test").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn soju_relays_a_line() {
    let ergo = Ergo::start();
    let soju = Soju::start(&ergo).await;
    let mut bob = join_bob(&ergo).await;
    let (_tx, mut rx) = net(via_soju(&soju));
    joined(&mut rx, "#test").await;
    bob.send("PRIVMSG #test :hello through soju").await;
    let got = next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.text == "hello through soju")).await;
    assert_eq!(got.nick.as_deref(), Some("bob"));
}

/// Spec, ZNC test 1. The first connect: the whole buffer comes as
/// history (no unread counts), and the query window opens from it. Our
/// PLAY, echoed back, opens no `*playback` window.
#[tokio::test(flavor = "multi_thread")]
async fn znc_the_first_connect_replays_the_whole_buffer_as_history() {
    let ergo = Ergo::start();
    let znc = Znc::start(&ergo);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    let mut bob = join_bob(&ergo).await;
    for t in ["b1", "b2", "b3"] {
        bob.send(&format!("PRIVMSG #test :{t}")).await;
    }
    bob.send("PRIVMSG alice :psst").await;
    synced(&mut bob).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (_tx, mut rx) = net(via_znc(&znc));
    let mut store = Store::new(["znc".to_owned()]);
    let (mut chan, mut query) = (Vec::new(), Vec::new());
    while chan.len() < 3 || query.is_empty() {
        let ev = next_matching(&mut rx, |e| Some(e.clone())).await;
        match &ev {
            Event::History { target: Target::Channel(c), lines, .. } if c.eq_ignore_ascii_case("#test") => {
                assert!(lines.iter().all(|l| l.replayed));
                chan.extend(said(lines));
            }
            Event::History { target: Target::Query(q), lines, .. } if q.eq_ignore_ascii_case("bob") => {
                assert!(lines.iter().all(|l| l.replayed));
                query.extend(said(lines));
            }
            Event::History { target, .. } => panic!("history for {target:?}"),
            Event::Line { target, line, .. } => {
                assert!(!["b1", "b2", "b3", "psst"].contains(&line.text.as_str()), "came live: {line:?}");
                assert!(!matches!(target, Target::Query(q) if q.starts_with('*')), "a module's window opened: {line:?}");
            }
            _ => {}
        }
        store.apply(ev);
    }
    assert_eq!(chan, ["b1", "b2", "b3"]);
    assert_eq!(query, ["psst"]);
    for t in [Target::Channel("#test".into()), Target::Query("bob".into())] {
        let b = store.buffer_ix(0, &t);
        let buf = &store.networks[0].buffers[b];
        assert_eq!((buf.unread, buf.mentions), (0, 0), "{t:?} counted history as news");
    }
    let modules: Vec<&Target> = store.networks[0].buffers.iter().map(|b| &b.target).filter(|t| matches!(t, Target::Query(q) if q.starts_with('*'))).collect();
    assert!(modules.is_empty(), "{modules:?}");
}

/// Spec, ZNC test 7, which decides decision 12. A live line and its
/// replayed copy carry one time, to the millisecond, so they get one
/// made-up id. ZNC passes Ergo's msgid on the live copy only (Facts row
/// 6), so the made-up id replaces it (ruling Q1).
#[tokio::test(flavor = "multi_thread")]
async fn znc_a_live_line_and_its_replay_have_one_time() {
    let ergo = Ergo::start();
    let znc = Znc::start(&ergo);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    let mut bob = join_bob(&ergo).await;
    let (tx, mut rx) = net(via_znc(&znc));
    joined(&mut rx, "#test").await;
    bob.send("PRIVMSG #test :same line").await;
    let live = next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.text == "same line")).await;
    tx.send(Command::Disconnect { network: "znc".into(), reason: None }).unwrap();
    offline(&mut rx).await;
    // A hirc that holds nothing asks ZNC for its whole buffer.
    let (_tx, mut rx) = net(via_znc(&znc));
    let replayed = next_matching(&mut rx, |e| match e {
        Event::History { lines, .. } => lines.iter().find(|l| l.text == "same line").cloned(),
        _ => None,
    })
    .await;
    assert_eq!(replayed.time, live.time, "ZNC re-timed the line: decision 12's fallback applies");
    assert!(live.msgid.as_deref().is_some_and(hirc_core::msgid::is_made_up), "{live:?}");
    assert_eq!(replayed.msgid, live.msgid);
}

/// Records what ZNC 1.10.3 with playback really sends, for the unit
/// tests to read. Facts the spec says to check:
/// - the replay's batch type, and its brackets without batches
/// - where PLAY's time cuts, in whole seconds and with a fraction
/// - whether self-messages and live lines carry server-time and msgid
#[tokio::test(flavor = "multi_thread")]
#[ignore = "writes tests/data/znc-1.10.3-*.txt: run by hand when the harness changes"]
async fn record_znc_facts() {
    let ergo = Ergo::start();
    let znc = Znc::start(&ergo);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    let mut bob = join_bob(&ergo).await;
    // Over a second apart, so where PLAY's time cuts shows.
    for i in 1..=3 {
        bob.send(&format!("PRIVMSG #test :fact {i}")).await;
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    bob.send("PRIVMSG alice :fact dm").await;
    synced(&mut bob).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let want = ["batch", "server-time", "message-tags", "echo-message", "cap-notify", "znc.in/playback", "znc.in/self-message"];
    let (mut c, mut attach) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &want).await;
    attach.extend(ask(&mut c, "PING :attached").await);
    record("znc-1.10.3-cap-ls.txt", &cap_ls(&attach));
    record("znc-1.10.3-attach.txt", &attach);
    let play = ask(&mut c, "PRIVMSG *playback :PLAY * 0").await;
    record("znc-1.10.3-play-batch.txt", &play);
    // Where PLAY's time cuts: fact 2's whole second, then its exact time.
    let t2 = time_of(&play, "fact 2");
    record("znc-1.10.3-play-since-secs.txt", &ask(&mut c, &format!("PRIVMSG *playback :PLAY #test {}", t2.timestamp())).await);
    let exact = format!("PRIVMSG *playback :PLAY #test {}.{:03}", t2.timestamp(), t2.timestamp_subsec_millis());
    record("znc-1.10.3-play-since-exact.txt", &ask(&mut c, &exact).await);
    // A live line from Ergo: does ZNC pass on its msgid and time?
    bob.send("PRIVMSG #test :fact live").await;
    record("znc-1.10.3-live.txt", &c.until("fact live").await);
    // Our own line from a second client (self-message), and what that
    // client gets back itself.
    let (mut phone, _) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &want).await;
    phone.send("PRIVMSG #test :fact self").await;
    let mut own = vec!["> (second client) PRIVMSG #test :fact self".to_owned()];
    own.extend(c.until("fact self").await);
    own.push("> (the second client's own lines follow)".to_owned());
    own.extend(ask(&mut phone, "PING :echo").await);
    record("znc-1.10.3-self-message.txt", &own);
    // The live lines once more, replayed: timed as they were live?
    record("znc-1.10.3-play-after-live.txt", &ask(&mut c, "PRIVMSG *playback :PLAY #test 0").await);
    // Without batches: the brackets around each buffer.
    let (mut plain, _) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &["server-time", "znc.in/playback"]).await;
    ask(&mut plain, "PING :attached").await;
    record("znc-1.10.3-play-plain.txt", &ask(&mut plain, "PRIVMSG *playback :PLAY * 0").await);
    // Without the playback cap: ZNC's own replay on attach (out of scope;
    // kept for reference).
    let (mut old, mut seen) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &["batch", "server-time"]).await;
    seen.extend(ask(&mut old, "PING :attached").await);
    record("znc-1.10.3-attach-without-playback.txt", &seen);
    // Without batches or the playback cap: the only replay that carries
    // ZNC's `***` brackets (PLAY without batches has none; out of scope
    // too, kept for reference).
    let (mut bare, mut seen) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &["server-time"]).await;
    seen.extend(ask(&mut bare, "PING :attached").await);
    record("znc-1.10.3-attach-plain.txt", &seen);
}

/// Records what soju 0.11.1 really sends: its caps, its CHATHISTORY limit
/// (005), LATEST past the limit, TARGETS, MARKREAD, and a live line.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "writes tests/data/soju-0.11.1-*.txt: run by hand when the harness changes"]
async fn record_soju_facts() {
    let ergo = Ergo::start();
    let soju = Soju::start(&ergo).await;
    let mut bob = join_bob(&ergo).await;
    for i in 1..=3 {
        bob.send(&format!("PRIVMSG #test :fact {i}")).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    bob.send("PRIVMSG alice :fact dm").await;
    synced(&mut bob).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let want = [
        "batch",
        "server-time",
        "message-tags",
        "echo-message",
        "cap-notify",
        "labeled-response",
        "draft/chathistory",
        "chathistory",
        "draft/event-playback",
        "draft/read-marker",
        "znc.in/playback",
        "znc.in/self-message",
    ];
    let (mut c, mut attach) = register(soju.port, soju::PASSWORD, soju::USER, &soju::username(), &want).await;
    attach.extend(ask(&mut c, "PING :attached").await);
    record("soju-0.11.1-cap-ls.txt", &cap_ls(&attach));
    record("soju-0.11.1-attach.txt", &attach);
    let now = Utc::now();
    let (from, to) = (timestamp(now - TimeDelta::hours(1)), timestamp(now + TimeDelta::hours(1)));
    let mut h = Vec::new();
    for cmd in [
        "CHATHISTORY LATEST #test * 100".to_owned(),
        "CHATHISTORY LATEST #test * 100000".to_owned(),
        format!("CHATHISTORY TARGETS {from} {to} 100"),
        "CHATHISTORY LATEST bob * 100".to_owned(),
        "MARKREAD #test".to_owned(),
    ] {
        h.extend(ask(&mut c, &cmd).await);
    }
    record("soju-0.11.1-chathistory.txt", &h);
    // A live line from Ergo, then the same line from history: one msgid?
    bob.send("PRIVMSG #test :fact live").await;
    let mut live = c.until("fact live").await;
    live.extend(ask(&mut c, "CHATHISTORY LATEST #test * 1").await);
    record("soju-0.11.1-live.txt", &live);
}

/// Asks for older lines of `target` on `network` until the start of its
/// history, applying everything that comes to `store`.
async fn scroll_to_the_top(tx: &UnboundedSender<Command>, rx: &mut UnboundedReceiver<Event>, store: &mut Store, network: &str, target: &Target) {
    let name = target.name().unwrap_or_default().to_owned();
    loop {
        tx.send(Command::Older { network: network.into(), target: name.clone() }).unwrap();
        let mut start = false;
        applied_until(rx, store, |e| match e {
            Event::History { target: t, lines, start: s, .. } if t.same(target) && (*s || !lines.is_empty()) => {
                start = *s;
                true
            }
            _ => false,
        })
        .await;
        if start {
            return;
        }
    }
}

/// Spec, ZNC test 6. ZNC kept only its last five lines while hirc was
/// away. Its replay starts past what the cache holds, so its first line
/// is a break. After a restart, scrolling up still reads past the break
/// to the lines from before.
#[tokio::test(flavor = "multi_thread")]
async fn znc_an_overflowed_buffer_is_a_break_that_scrolling_reads_past() {
    let ergo = Ergo::start();
    let znc = Znc::with_buffer(&ergo, 5);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    let mut bob = join_bob(&ergo).await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.db");
    let mut config = via_znc(&znc);
    config.servers.get_mut("znc").unwrap().history_page_size = Some(3);
    let (tx, mut rx, flush) = hirc_core::conn::spawn(config.clone(), Some(db.clone()));
    joined(&mut rx, "#test").await;
    for i in 1..=3 {
        bob.send(&format!("PRIVMSG #test :seen {i}")).await;
    }
    next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.text == "seen 3")).await;
    tx.send(Command::Disconnect { network: "znc".into(), reason: None }).unwrap();
    offline(&mut rx).await;
    for i in 1..=8 {
        bob.send(&format!("PRIVMSG #test :away {i}")).await;
    }
    synced(&mut bob).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(Command::Reconnect { network: "znc".into() }).unwrap();
    next_matching(&mut rx, |e| matches!(e, Event::History { lines, .. } if lines.iter().any(|l| l.text == "away 8")).then_some(())).await;
    assert!(tokio::task::spawn_blocking(move || flush.wait()).await.unwrap());
    drop(tx);
    let brk =
        |text: &str| -> bool { rusqlite::Connection::open(&db).unwrap().query_row("SELECT brk FROM lines WHERE text = ?1", [text], |r| r.get(0)).unwrap() };
    // ZNC's five lines began at "away 4": past "seen 3", the newest held.
    assert!(brk("away 4"));
    assert!(!brk("away 5"));

    let chan = Target::Channel("#test".into());
    let (tx, mut rx, _flush) = hirc_core::conn::spawn(config, Some(db.clone()));
    let mut store = Store::new(["znc".to_owned()]);
    applied_until(&mut rx, &mut store, |e| matches!(e, Event::Status { status: ConnStatus::Connected { .. }, .. })).await;
    scroll_to_the_top(&tx, &mut rx, &mut store, "znc", &chan).await;
    assert_eq!(said_in(&mut store, &chan), ["seen 1", "seen 2", "seen 3", "away 4", "away 5", "away 6", "away 7", "away 8"]);
}

/// Polls ZNC, with a second client, until its playback for `target` holds
/// a line matching `want` (a substring of a raw line), for up to 10 s.
/// `synced` only proves a line reached Ergo, not that ZNC has buffered it.
/// Gives back that playback.
async fn znc_holds(znc: &Znc, target: &str, want: &str) -> Vec<String> {
    let mut held = Vec::new();
    for _ in 0..50 {
        let (mut check, _) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &["batch", "server-time", "znc.in/playback"]).await;
        held = ask(&mut check, &format!("PRIVMSG *playback :PLAY {target} 0")).await;
        check.send("QUIT").await;
        if held.iter().any(|l| !l.starts_with("> ") && l.contains(want)) {
            return held;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("ZNC never held {want:?} for {target}: {held:?}");
}

/// Spec, ZNC test 2. A reconnect brings only what was missed, as history,
/// and each line once.
#[tokio::test(flavor = "multi_thread")]
async fn znc_a_reconnect_brings_only_what_was_missed_and_each_line_once() {
    let ergo = Ergo::start();
    let znc = Znc::start(&ergo);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    let mut bob = join_bob(&ergo).await;
    let chan = Target::Channel("#test".into());
    let (tx, mut rx) = net(via_znc(&znc));
    let mut store = Store::new(["znc".to_owned()]);
    applied_until(&mut rx, &mut store, |e| matches!(e, Event::Joined { .. })).await;
    bob.send("PRIVMSG #test :before").await;
    applied_until(&mut rx, &mut store, |e| line_in(e, "#test").is_some_and(|l| l.text == "before")).await;
    tx.send(Command::Disconnect { network: "znc".into(), reason: None }).unwrap();
    applied_until(&mut rx, &mut store, |e| matches!(e, Event::Status { status: ConnStatus::Offline, .. })).await;
    for i in 1..=3 {
        bob.send(&format!("PRIVMSG #test :missed {i}")).await;
    }
    znc_holds(&znc, "#test", "missed 3").await;
    tx.send(Command::Reconnect { network: "znc".into() }).unwrap();
    // Every #test text that arrives after the reconnect, live or as history.
    let (mut live, mut played) = (Vec::new(), Vec::new());
    let mut note = |e: &Event| match e {
        Event::History { target, lines, .. } if target.same(&chan) => played.extend(said(lines)),
        _ => live.extend(line_in(e, "#test").map(|l| l.text)),
    };
    applied_until(&mut rx, &mut store, |e| {
        note(e);
        matches!(e, Event::History { lines, .. } if lines.iter().any(|l| l.text == "missed 3"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(e) = rx.try_recv() {
        note(&e);
        store.apply(e);
    }
    // (Our own re-join shows live, as a notice; chat must not.)
    assert!(!live.iter().any(|t| t == "before" || t.starts_with("missed")), "came live: {live:?}");
    // PLAY takes whole seconds from the newest line held, so ZNC sends that
    // one line again (`playback::since`); the store collapses it by msgid.
    // Nothing else from before the drop may come back, and nothing twice.
    played.sort();
    let boundary = played.iter().filter(|t| *t == "before").count();
    assert!(boundary <= 1, "history after the reconnect: {played:?}");
    played.retain(|t| t != "before");
    assert_eq!(played, ["missed 1", "missed 2", "missed 3"], "history after the reconnect");
    assert_eq!(said_in(&mut store, &chan), ["before", "missed 1", "missed 2", "missed 3"]);
}

/// Spec, ZNC test 3. A line we send from another client shows as our own
/// (self-message).
#[tokio::test(flavor = "multi_thread")]
async fn znc_a_line_from_our_other_client_shows_as_our_own() {
    let ergo = Ergo::start();
    let znc = Znc::start(&ergo);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    let (_tx, mut rx) = net(via_znc(&znc));
    joined(&mut rx, "#test").await;
    let (mut phone, _) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &["server-time"]).await;
    phone.send("PRIVMSG #test :from my phone").await;
    let got = next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.text == "from my phone")).await;
    assert!(got.own, "{got:?}");
    assert_eq!(got.nick.as_deref(), Some(znc::USER));
    assert!(!got.replayed);
}

/// Spec, ZNC test 4. A services buffer in the replay opens no window, and
/// none is cached.
///
/// Real ZNC 1.10.3 never holds a services line here: Ergo's NickServ
/// answers with NOTICEs, which ZNC does not buffer, and ZNC keeps no copy
/// of our own queries. What it does hold is the (empty) NickServ buffer
/// that talking to it creates, and PLAY brackets it like any other. That
/// is what this test replays. It does not exercise the services filter:
/// an empty batch is dropped before that check. A services line inside
/// the brackets is covered by the session unit tests, from recorded lines.
#[tokio::test(flavor = "multi_thread")]
async fn znc_a_services_buffer_in_the_replay_opens_no_window_and_is_never_cached() {
    let ergo = Ergo::start();
    let znc = Znc::start(&ergo);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    // Another client of ours talks to NickServ while hirc is away.
    let (mut phone, _) = register(znc.port, &znc::pass(), znc::USER, znc::USER, &["server-time"]).await;
    phone.send("PRIVMSG NickServ :HELP").await;
    synced(&mut phone).await;
    phone.send("QUIT").await;
    let mut bob = join_bob(&ergo).await;
    bob.send("PRIVMSG #test :after services").await;
    // ZNC holds a NickServ buffer, so hirc's PLAY gets it.
    znc_holds(&znc, "*", "znc.in/playback NickServ").await;
    znc_holds(&znc, "#test", "after services").await;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.db");
    let (_tx, mut rx, flush) = hirc_core::conn::spawn(via_znc(&znc), Some(db.clone()));
    let mut seen = Vec::new();
    next_matching(&mut rx, |e| {
        seen.push(e.clone());
        matches!(e, Event::History { lines, .. } if lines.iter().any(|l| l.text == "after services")).then_some(())
    })
    .await;
    // ZNC plays the buffers back in its own order: let the rest land.
    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(e) = rx.try_recv() {
        seen.push(e);
    }
    let services = |t: &Target| matches!(t, Target::Query(q) if q.eq_ignore_ascii_case("NickServ"));
    assert!(
        !seen.iter().any(|e| matches!(e, Event::History { target, .. } | Event::Line { target, .. } if services(target))),
        "a NickServ window opened: {seen:?}"
    );
    assert!(tokio::task::spawn_blocking(move || flush.wait()).await.unwrap());
    let cached = hirc_core::cache::Cache::open(&db).unwrap().targets("znc").unwrap();
    assert!(!cached.iter().any(|(t, _)| t.eq_ignore_ascii_case("nickserv")), "{cached:?}");
}

/// Spec, ZNC test 5. A cache round trip across a restart: the window
/// opens on the cache, and scrolling up reads the rest of the cached ZNC
/// lines.
#[tokio::test(flavor = "multi_thread")]
async fn znc_a_restart_scrolls_up_through_the_cached_lines() {
    let ergo = Ergo::start();
    let znc = Znc::start(&ergo);
    wait_in_channel(&ergo, znc::USER, "#test").await;
    let mut bob = join_bob(&ergo).await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("history.db");
    let mut config = via_znc(&znc);
    config.servers.get_mut("znc").unwrap().history_page_size = Some(2);
    let (tx, mut rx, flush) = hirc_core::conn::spawn(config.clone(), Some(db.clone()));
    joined(&mut rx, "#test").await;
    for i in 1..=5 {
        bob.send(&format!("PRIVMSG #test :m{i}")).await;
    }
    next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.text == "m5")).await;
    assert!(tokio::task::spawn_blocking(move || flush.wait()).await.unwrap());
    drop(tx);

    let chan = Target::Channel("#test".into());
    let (tx, mut rx, _flush) = hirc_core::conn::spawn(config, Some(db.clone()));
    // Before any connection: the cache's latest page.
    let first = next_matching(&mut rx, |e| Some(e.clone())).await;
    let Event::History { lines, .. } = &first else { panic!("first event {first:?}") };
    assert_eq!(said(lines), ["m4", "m5"]);
    assert!(lines.iter().all(|l| l.replayed));
    let mut store = Store::new(["znc".to_owned()]);
    store.apply(first);
    applied_until(&mut rx, &mut store, |e| matches!(e, Event::Status { status: ConnStatus::Connected { .. }, .. })).await;
    scroll_to_the_top(&tx, &mut rx, &mut store, "znc", &chan).await;
    assert_eq!(said_in(&mut store, &chan), ["m1", "m2", "m3", "m4", "m5"]);
}
