//! hirc-core through real bouncers: ZNC 1.10 with its playback module,
//! and soju, each in front of a throwaway Ergo. Inside the flox
//! environment:
//!   scripts/ergo-test.sh setup
//!   cargo test -p hirc-core --features bouncer-tests --test bouncer
//! `-- --ignored` re-records the transcripts in `tests/data/`.

#![cfg(feature = "bouncer-tests")]

mod common;

use chrono::{DateTime, TimeDelta, Utc};
use common::events::{joined, line_in, net, next_matching, offline};
use common::soju::{self, Soju};
use common::znc::{self, Znc};
use common::{Ergo, Raw, record, register, wait_in_channel};
use hirc_core::config::{Config, Server};
use hirc_core::event::Command;
use hirc_core::history::timestamp;
use hirc_core::proto::Message;
use std::time::Duration;

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
