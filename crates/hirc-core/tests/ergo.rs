//! hirc-core against a real Ergo. Run with:
//!   scripts/ergo-test.sh setup
//!   cargo test -p hirc-core --features ergo-tests --test ergo

#![cfg(feature = "ergo-tests")]

mod common;

use common::{Ergo, Raw, register_account};
use hirc_core::config::{Config, Sasl, Server};
use hirc_core::event::{Command, ConnStatus, Event, Target};
use hirc_core::store::Line;
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
    // echo-message: exactly one copy of our own line, with a msgid.
    let own = next_matching(&mut rx, |e| line_in(e, "#test").filter(|l| l.own && l.text == "hello from hirc")).await;
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
