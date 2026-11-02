//! Waiting on hirc's events, for the tests against a real Ergo and the
//! tests through real bouncers.

use hirc_core::config::Config;
use hirc_core::event::{Command, ConnStatus, Event, Target};
use hirc_core::store::{Line, LineKind, Store};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// hirc's network side with no history cache.
pub fn net(config: Config) -> (UnboundedSender<Command>, UnboundedReceiver<Event>) {
    let (tx, rx, _) = hirc_core::conn::spawn(config, None);
    (tx, rx)
}

pub async fn next_matching<T>(rx: &mut UnboundedReceiver<Event>, mut f: impl FnMut(&Event) -> Option<T>) -> T {
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

pub fn line_in(ev: &Event, chan: &str) -> Option<Line> {
    match ev {
        Event::Line { target: Target::Channel(c), line, .. } if c.eq_ignore_ascii_case(chan) => Some(line.clone()),
        _ => None,
    }
}

/// The next page of history for `target`.
pub async fn page(rx: &mut UnboundedReceiver<Event>, target: &str) -> (Vec<Line>, bool) {
    next_matching(rx, |e| match e {
        Event::History { target: t, lines, start, .. } if t.name().is_some_and(|n| n.eq_ignore_ascii_case(target)) => Some((lines.clone(), *start)),
        _ => None,
    })
    .await
}

pub fn said(lines: &[Line]) -> Vec<String> {
    lines.iter().filter(|l| l.kind == LineKind::Privmsg).map(|l| l.text.clone()).collect()
}

/// Waits for our JOIN of `chan`.
pub async fn joined(rx: &mut UnboundedReceiver<Event>, chan: &str) {
    next_matching(rx, |e| matches!(e, Event::Joined { channel, .. } if channel.eq_ignore_ascii_case(chan)).then_some(())).await
}

/// Waits until the connection is down because we asked (a Disconnect).
pub async fn offline(rx: &mut UnboundedReceiver<Event>) {
    next_matching(rx, |e| matches!(e, Event::Status { status: ConnStatus::Offline, .. }).then_some(())).await
}

/// Applies every event to `store` until one matches `f` (that one too).
pub async fn applied_until(rx: &mut UnboundedReceiver<Event>, store: &mut Store, mut f: impl FnMut(&Event) -> bool) {
    next_matching(rx, |e| {
        store.apply(e.clone());
        f(e).then_some(())
    })
    .await
}

/// What was said in `target` on the store's first network, in the
/// store's order (server time, then arrival).
pub fn said_in(store: &mut Store, target: &Target) -> Vec<String> {
    let b = store.buffer_ix(0, target);
    said(&store.networks[0].buffers[b].lines)
}
