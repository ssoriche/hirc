//! hirc-core against a real Ergo. Run with:
//!   scripts/ergo-test.sh setup
//!   cargo test -p hirc-core --features ergo-tests --test ergo

#![cfg(feature = "ergo-tests")]

mod common;

use common::{Ergo, Raw, register_account};
use hirc_core::config::{Config, Sasl, Server};
use hirc_core::event::{Command, ConnStatus, Event, Target};
use hirc_core::store::{Delivery, Line, LineKind};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

fn alice(ergo: &Ergo, password_command: &str) -> Config {
    let mut config = Config::default();
    config.servers.insert(
        "test".into(),
        Server {
            nickname: "alice".into(),
            server: "localhost".into(),
            port: Some(ergo.tls_port),
            root_cert_path: Some(ergo.ca.clone()),
            channels: vec!["#test".into()],
            sasl: Some(Sasl::Plain { username: "alice".into(), password: None, password_file: None, password_command: Some(password_command.into()) }),
            ..Server::default()
        },
    );
    config
}

async fn next_matching<T>(rx: &mut UnboundedReceiver<Event>, mut f: impl FnMut(&Event) -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ev = rx.recv().await.expect("core stopped");
            if let Some(t) = f(&ev) {
                return t;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

fn line_in(ev: &Event, chan: &str) -> Option<Line> {
    match ev {
        Event::Line { target: Target::Channel(c), line, .. } if c.eq_ignore_ascii_case(chan) => Some(line.clone()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sasl_join_send_and_receive() {
    let ergo = Ergo::start();
    register_account(ergo.plain_port, "alice", "hunter22").await;
    let mut bob = Raw::connect(ergo.plain_port, "bob").await;
    bob.send("JOIN #test").await;
    bob.expect("JOIN").await;

    let (tx, mut rx) = hirc_core::conn::spawn(alice(&ergo, "printf hunter22"));
    next_matching(&mut rx, |e| matches!(e, Event::Joined { channel, .. } if channel == "#test").then_some(())).await;

    tx.send(Command::Say { network: "test".into(), target: "#test".into(), text: "hello from hirc".into() }).unwrap();
    bob.expect("PRIVMSG #test :hello from hirc").await;
    // echo-message: our line shows at once as pending, then the server's
    // echo, with a msgid, is the one copy that stays.
    let pending = next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.own && l.text == "hello from hirc")).await;
    assert_eq!((pending.delivery, pending.msgid.as_deref()), (Delivery::Pending, None));
    let own = next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.own && l.text == "hello from hirc")).await;
    assert_eq!(own.delivery, Delivery::Sent);
    assert!(own.msgid.is_some());

    bob.send("PRIVMSG #test :hi alice").await;
    // Everything up to bob's line must not contain a second copy of ours.
    let mut seen = Vec::new();
    let got = next_matching(&mut rx, |e| {
        seen.push(e.clone());
        line_in(e, "#test").filter(|l| l.text == "hi alice")
    })
    .await;
    assert_eq!(got.nick.as_deref(), Some("bob"));
    assert!(got.msgid.is_some());
    assert!(!got.own);
    assert!(seen.iter().all(|ev| line_in(ev, "#test").is_none_or(|l| l.text != "hello from hirc")), "duplicate own line");

    // No second echo of our own message showed up in the meantime.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(ev) = rx.try_recv() {
        assert!(line_in(&ev, "#test").is_none_or(|l| l.text != "hello from hirc"), "duplicate own line");
    }
}

/// A second hirc, without SASL, on the plaintext port.
fn carol(ergo: &Ergo) -> Config {
    let mut config = Config::default();
    config.servers.insert(
        "test".into(),
        Server {
            nickname: "carol".into(),
            server: "localhost".into(),
            port: Some(ergo.plain_port),
            use_tls: false,
            channels: vec!["#test".into()],
            ..Server::default()
        },
    );
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn multiline_round_trip() {
    let ergo = Ergo::start();
    register_account(ergo.plain_port, "alice", "hunter22").await;
    let (tx, mut rx) = hirc_core::conn::spawn(alice(&ergo, "printf hunter22"));
    let limits = next_matching(&mut rx, |e| match e {
        Event::Multiline { limits, .. } => Some(*limits),
        _ => None,
    })
    .await;
    // The harness's own limits, read from its CAP LS.
    assert_eq!(limits, Some(hirc_core::multiline::Limits { max_bytes: 4096, max_lines: Some(100) }));
    next_matching(&mut rx, |e| matches!(e, Event::Joined { channel, .. } if channel == "#test").then_some(())).await;
    let (_ctx, mut crx) = hirc_core::conn::spawn(carol(&ergo));
    next_matching(&mut crx, |e| matches!(e, Event::Joined { channel, .. } if channel == "#test").then_some(())).await;

    tx.send(Command::Say { network: "test".into(), target: "#test".into(), text: "one\ntwo\nthree".into() }).unwrap();
    // Carol gets it as one message of three lines.
    let got = next_matching(&mut crx, |e| line_in(e, "#test").filter(|l| l.nick.as_deref() == Some("alice") && l.kind == LineKind::Privmsg)).await;
    assert_eq!(got.text, "one\ntwo\nthree");
    assert!(got.msgid.is_some());
    // Alice's echo comes back as one message too, with the server's msgid.
    let own = next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.own && l.kind == LineKind::Privmsg && l.msgid.is_some())).await;
    assert_eq!(own.text, "one\ntwo\nthree");
    assert_eq!(own.msgid, got.msgid);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_sasl_password_stops_without_retrying() {
    let ergo = Ergo::start();
    register_account(ergo.plain_port, "alice", "hunter22").await;
    let (_tx, mut rx) = hirc_core::conn::spawn(alice(&ergo, "printf wrong"));
    let reason = next_matching(&mut rx, |e| match e {
        Event::Status { status: ConnStatus::Failed { reason }, .. } => Some(reason.clone()),
        _ => None,
    })
    .await;
    assert!(reason.contains("SASL"), "{reason}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    while let Ok(ev) = rx.try_recv() {
        assert!(!matches!(ev, Event::Status { status: ConnStatus::Connecting, .. }), "retried after a SASL failure");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_password_command_stops() {
    let ergo = Ergo::start();
    let (_tx, mut rx) = hirc_core::conn::spawn(alice(&ergo, "exit 1"));
    next_matching(&mut rx, |e| matches!(e, Event::Status { status: ConnStatus::Failed { .. }, .. }).then_some(())).await;
}

/// The next page of history for `target`.
async fn page(rx: &mut UnboundedReceiver<Event>, target: &str) -> (Vec<Line>, bool) {
    next_matching(rx, |e| match e {
        Event::History { target: t, lines, start, .. } if t.name().is_some_and(|n| n.eq_ignore_ascii_case(target)) => Some((lines.clone(), *start)),
        _ => None,
    })
    .await
}

fn said(lines: &[Line]) -> Vec<String> {
    lines.iter().filter(|l| l.kind == LineKind::Privmsg).map(|l| l.text.clone()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_fills_the_gap_without_duplicates_or_news() {
    let ergo = Ergo::start();
    register_account(ergo.plain_port, "alice", "hunter22").await;
    let mut bob = Raw::connect(ergo.plain_port, "bob").await;
    bob.send("JOIN #test").await;
    bob.expect("JOIN").await;
    let (tx, mut rx) = hirc_core::conn::spawn(alice(&ergo, "printf hunter22"));
    next_matching(&mut rx, |e| matches!(e, Event::Joined { channel, .. } if channel == "#test").then_some(())).await;
    bob.send("PRIVMSG #test :before the drop").await;
    next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.text == "before the drop")).await;

    tx.send(Command::Disconnect { network: "test".into(), reason: Some("brb".into()) }).unwrap();
    bob.expect("QUIT").await;
    for i in 1..=3 {
        bob.send(&format!("PRIVMSG #test :gap {i}")).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    tx.send(Command::Reconnect { network: "test".into() }).unwrap();

    // Everything until the gap is in: no gap line comes live, and the
    // line we had is never asked for again.
    let mut got = Vec::new();
    let mut live = Vec::new();
    while got.len() < 3 {
        let ev = next_matching(&mut rx, |e| Some(e.clone())).await;
        match ev {
            Event::History { lines, .. } => {
                assert!(lines.iter().all(|l| l.replayed));
                got.extend(said(&lines));
            }
            Event::Line { line, .. } if line.text.starts_with("gap") || line.text == "before the drop" => live.push(line.text),
            _ => {}
        }
    }
    assert_eq!(got, ["gap 1", "gap 2", "gap 3"]);
    assert!(live.is_empty(), "came live: {live:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn history_pages_both_ways_at_the_servers_limit() {
    let ergo = Ergo::start();
    register_account(ergo.plain_port, "alice", "hunter22").await;
    let mut bob = Raw::connect(ergo.plain_port, "bob").await;
    let limit: usize = bob.isupport_value("CHATHISTORY").await.parse().unwrap();
    bob.send("JOIN #test").await;
    bob.expect("JOIN").await;
    let total = 2 * limit + 5;
    for i in 0..total {
        bob.send(&format!("PRIVMSG #test :n{i}")).await;
    }
    bob.send("PING :sent").await;
    bob.expect("PONG").await;

    // A page bigger than the server's limit is asked for as the limit.
    let mut config = alice(&ergo, "printf hunter22");
    config.servers.get_mut("test").unwrap().history_page_size = Some(10_000);
    let (tx, mut rx) = hirc_core::conn::spawn(config);
    let mut seen: Vec<String> = Vec::new();
    let mut ids = std::collections::HashSet::new();
    let mut take = |lines: &[Line], seen: &mut Vec<String>| {
        for l in lines {
            assert!(ids.insert(l.msgid.clone().unwrap()), "duplicate {l:?}");
        }
        seen.extend(said(lines));
    };
    let (lines, start) = page(&mut rx, "#test").await;
    assert!(!start && lines.len() <= limit);
    take(&lines, &mut seen);
    // Scroll to the top until the server has nothing older.
    loop {
        tx.send(Command::Older { network: "test".into(), target: "#test".into() }).unwrap();
        let (lines, start) = page(&mut rx, "#test").await;
        assert!(lines.len() <= limit);
        take(&lines, &mut seen);
        if start {
            assert!(lines.is_empty());
            break;
        }
    }
    let mut want: Vec<String> = (0..total).map(|i| format!("n{i}")).collect();
    seen.sort();
    want.sort();
    assert_eq!(seen, want);

    // And forward: more than a page while away comes in pages, each line once.
    tx.send(Command::Disconnect { network: "test".into(), reason: None }).unwrap();
    bob.expect("QUIT").await;
    let away = limit + 3;
    for i in 0..away {
        bob.send(&format!("PRIVMSG #test :a{i}")).await;
    }
    bob.send("PING :sent").await;
    bob.expect("PONG").await;
    tx.send(Command::Reconnect { network: "test".into() }).unwrap();
    let mut gap = Vec::new();
    let mut pages = 0;
    while gap.len() < away {
        let (lines, _) = page(&mut rx, "#test").await;
        pages += 1;
        take(&lines, &mut gap);
    }
    assert!(pages >= 2, "{away} lines past a limit of {limit} take two pages");
    let mut want: Vec<String> = (0..away).map(|i| format!("a{i}")).collect();
    gap.sort();
    want.sort();
    assert_eq!(gap, want);
}
