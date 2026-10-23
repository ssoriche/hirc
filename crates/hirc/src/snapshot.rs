//! Developer snapshot mode: a scripted, network-free run of the real window
//! that saves a screenshot, so the UI can be reviewed (and iterated on)
//! without a person at the keyboard. Only built with `--features snapshot`.
//!
//! gpui 0.2.2 cannot render offscreen to an image, so this opens the real
//! window with canned data, drives it with a script, captures the window
//! with macOS `screencapture -l <window id>` and quits. The screen must be
//! unlocked for the capture to work.
//!
//! Switches (all ignored unless `HIRC_FIXTURE` is set):
//!
//! | Variable | Effect |
//! |---|---|
//! | `HIRC_FIXTURE=<file.toml>` | No `conn::spawn`. Canned server lines go through a real `Session` per server; UI commands go to `session.command`, and only its `Event`s return. `Output::Send` is dropped, so nothing leaves the process. |
//! | `HIRC_SCRIPT="step;step"` | Steps run once the fixture is fed: `window:N`, `key:<keystroke>`, `type:<text>`, `wait:<ms>`, `shot:<path.png>`. |
//! | `HIRC_SCREENSHOT=<out.png>` | After the script, wait 300 ms for layout, capture, quit. |
//! | `HIRC_SIZE=WxH` | Window size in logical px (default `1200x800`). |
//!
//! Fixture runs never read or write the window layout file, and `config.servers`
//! is replaced by the fixture's networks. Theme and font still come from the
//! normal config dir (`XDG_CONFIG_HOME`). `type:` only inserts in Insert mode,
//! like real typing, so use `key:i` first when needed.
//!
//! Window id: the `AppKit` raw-window-handle gives the `NSView`; `[[view window]
//! windowNumber]` (via the `objc` crate gpui already uses) is the
//! `CGWindowID` that `screencapture -l` wants.
//!
//! Any failure prints `snapshot: <reason>` to stderr and exits non-zero.

use crate::app::HircApp;
use chrono::{DateTime, Duration as Delta, Utc};
use gpui::{AsyncApp, Keystroke, WindowHandle};
use hirc_core::config::{Config, Server};
use hirc_core::event::{Command, Event};
use hirc_core::proto::Message;
use hirc_core::session::{Output, Session, SessionConfig};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const DEFAULT_BASE: &str = "2026-01-01T12:00:00Z";
/// Time between steps, so each one renders before the next.
const FRAME: Duration = Duration::from_millis(80);
const LAYOUT_WAIT: Duration = Duration::from_millis(300);

#[derive(Debug, Deserialize)]
pub struct Fixture {
    #[serde(default)]
    pub base_time: Option<String>,
    #[serde(default)]
    pub server: Vec<FixtureServer>,
}

#[derive(Debug, Deserialize)]
pub struct FixtureServer {
    pub name: String,
    pub nick: String,
    #[serde(default)]
    pub echo_message: bool,
    #[serde(default)]
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Window(usize),
    Key(String),
    Type(String),
    Wait(u64),
    Shot(String),
    Unknown(String),
}

pub fn parse_fixture(text: &str) -> Result<Fixture, String> {
    toml::from_str(text).map_err(|e| format!("bad fixture: {e}"))
}

pub fn parse_script(script: &str) -> Vec<Step> {
    script
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let unknown = || Step::Unknown(s.to_owned());
            match s.split_once(':') {
                Some(("window", n)) => n.trim().parse().map_or_else(|_| unknown(), Step::Window),
                Some(("key", k)) => Step::Key(k.trim().to_owned()),
                Some(("type", t)) => Step::Type(t.to_owned()),
                Some(("wait", ms)) => ms.trim().parse().map_or_else(|_| unknown(), Step::Wait),
                Some(("shot", p)) => Step::Shot(p.trim().to_owned()),
                _ => unknown(),
            }
        })
        .collect()
}

/// `WxH` in logical px.
pub fn parse_size(s: &str) -> Option<(f32, f32)> {
    let (w, h) = s.split_once(['x', 'X'])?;
    let (w, h): (f32, f32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w >= 100. && h >= 100.).then_some((w, h))
}

fn session_config(s: &FixtureServer) -> SessionConfig {
    SessionConfig { nickname: s.nick.clone(), username: s.nick.clone(), realname: s.nick.clone(), ..Default::default() }
}

fn feed_line(session: &mut Session, line: &str, now: DateTime<Utc>, events: &mut Vec<Event>) -> Result<(), String> {
    let msg = Message::parse(line).map_err(|e| format!("bad fixture line {line:?}: {e}"))?;
    events.extend(session.handle(msg, now).into_iter().filter_map(|o| if let Output::Event(e) = o { Some(e) } else { None }));
    Ok(())
}

/// Run every server's lines through a real `Session`, as the network
/// thread would. Returns the events for the UI and the sessions, which keep
/// answering commands afterwards.
pub fn feed(fx: &Fixture) -> Result<(Vec<Event>, HashMap<String, Session>), String> {
    let base = DateTime::parse_from_rfc3339(fx.base_time.as_deref().unwrap_or(DEFAULT_BASE)).map_err(|e| format!("bad base_time: {e}"))?.with_timezone(&Utc);
    let mut events = Vec::new();
    let mut sessions = HashMap::new();
    for srv in &fx.server {
        let mut session = Session::new(&srv.name, session_config(srv));
        session.start(); // its sends go nowhere
        if srv.echo_message {
            // Through `handle`, so no private API is needed.
            for line in ["CAP * LS :echo-message", "CAP * ACK :echo-message"] {
                feed_line(&mut session, &format!(":irc.example {line}"), base, &mut events)?;
            }
        }
        for (i, line) in srv.lines.iter().enumerate() {
            let at = Message::parse(line).ok().and_then(|m| m.tag("time").and_then(|t| DateTime::parse_from_rfc3339(t).ok())).map(|t| t.with_timezone(&Utc));
            let now = at.unwrap_or_else(|| base + Delta::seconds(i as i64));
            feed_line(&mut session, line, now, &mut events)?;
        }
        sessions.insert(srv.name.clone(), session);
    }
    Ok((events, sessions))
}

pub struct Plan {
    pub size: (f32, f32),
    fixture: Fixture,
    script: Vec<Step>,
    shot: Option<String>,
    canned: Option<(Vec<Event>, HashMap<String, Session>)>,
    /// Set once the feeder has sent every canned event.
    fed: Arc<AtomicBool>,
}

pub fn fail(reason: &str) -> ! {
    eprintln!("snapshot: {reason}");
    std::process::exit(1);
}

impl Plan {
    /// `None` when `HIRC_FIXTURE` is unset: a normal run.
    pub fn from_env() -> Result<Option<Plan>, String> {
        let Ok(path) = std::env::var("HIRC_FIXTURE") else { return Ok(None) };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let fixture = parse_fixture(&text)?;
        let canned = Some(feed(&fixture)?);
        let size = match std::env::var("HIRC_SIZE") {
            Ok(s) => parse_size(&s).ok_or_else(|| format!("bad HIRC_SIZE {s:?}, want WxH"))?,
            Err(_) => (1200., 800.),
        };
        let script = std::env::var("HIRC_SCRIPT").map(|s| parse_script(&s)).unwrap_or_default();
        Ok(Some(Plan { size, fixture, script, shot: std::env::var("HIRC_SCREENSHOT").ok(), canned, fed: Arc::default() }))
    }

    /// The fixture decides the networks.
    pub fn apply(&self, config: &mut Config) {
        config.servers = self
            .fixture
            .server
            .iter()
            .map(|s| (s.name.clone(), Server { nickname: s.nick.clone(), server: "fixture.invalid".into(), use_tls: false, ..Default::default() }))
            .collect();
    }

    /// Stands in for `conn::spawn`: feeds the canned events, then answers
    /// UI commands through the sessions. Runs on its own thread.
    pub fn channels(&mut self) -> (UnboundedSender<Command>, UnboundedReceiver<Event>) {
        let (cmd_tx, mut cmd_rx) = unbounded_channel::<Command>();
        let (ev_tx, ev_rx) = unbounded_channel::<Event>();
        let (events, mut sessions) = self.canned.take().unwrap_or_default();
        let fed = self.fed.clone();
        std::thread::spawn(move || {
            for ev in events {
                let _ = ev_tx.send(ev);
            }
            fed.store(true, Ordering::SeqCst);
            while let Some(cmd) = cmd_rx.blocking_recv() {
                let Some(session) = sessions.get_mut(cmd.network()) else { continue };
                for out in session.command(cmd, Utc::now()) {
                    if let Output::Event(e) = out {
                        let _ = ev_tx.send(e);
                    }
                }
            }
        });
        (cmd_tx, ev_rx)
    }

    /// Run the script, then the screenshot, then quit.
    pub async fn run(self, cx: &mut AsyncApp, handle: WindowHandle<HircApp>) {
        while !self.fed.load(Ordering::SeqCst) {
            cx.background_executor().timer(FRAME).await;
        }
        // Let the UI drain what the feeder queued.
        cx.background_executor().timer(Duration::from_millis(250)).await;
        let interactive = self.script.is_empty() && self.shot.is_none();
        for step in &self.script {
            if let Err(e) = apply(step, cx, handle).await {
                fail(&e);
            }
            cx.background_executor().timer(FRAME).await;
        }
        if let Some(path) = &self.shot {
            cx.background_executor().timer(LAYOUT_WAIT).await;
            if let Err(e) = capture(cx, handle, path).await {
                fail(&e);
            }
        }
        if !interactive {
            cx.update(|cx| cx.quit());
        }
    }
}

async fn apply(step: &Step, cx: &mut AsyncApp, handle: WindowHandle<HircApp>) -> Result<(), String> {
    let gone = |_| "window closed".to_owned();
    match step {
        Step::Window(n) => handle.update(cx, |app, _, cx| app.goto_window(*n, cx)).map_err(gone),
        Step::Key(k) => {
            let ks = Keystroke::parse(k).map_err(|e| format!("bad key {k:?}: {e}"))?;
            handle
                .update(cx, |_, window, cx| {
                    window.dispatch_keystroke(ks, cx);
                })
                .map_err(gone)
        }
        Step::Type(t) => handle.update(cx, |app, window, cx| app.type_text(t, window, cx)).map_err(gone),
        Step::Wait(ms) => {
            cx.background_executor().timer(Duration::from_millis(*ms)).await;
            Ok(())
        }
        Step::Shot(path) => capture(cx, handle, path).await,
        Step::Unknown(s) => {
            eprintln!("snapshot: unknown step {s}");
            Ok(())
        }
    }
}

async fn capture(cx: &mut AsyncApp, handle: WindowHandle<HircApp>, path: &str) -> Result<(), String> {
    let id = handle.update(cx, |_, window, _| window_number(window)).map_err(|_| "window closed".to_owned())??;
    let out = path.to_owned();
    let status = cx
        .background_executor()
        .spawn(async move { std::process::Command::new("screencapture").args(["-x", "-o", "-l", &id.to_string(), &out]).status() })
        .await
        .map_err(|e| format!("cannot run screencapture: {e}"))?;
    if !status.success() {
        return Err(format!("screencapture failed ({status}); is the screen unlocked?"));
    }
    if !std::path::Path::new(path).is_file() {
        return Err(format!("screencapture wrote no file at {path}"));
    }
    Ok(())
}

/// The `CGWindowID` of the gpui window.
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc 0.2 macros probe `feature = "cargo-clippy"`
fn window_number(window: &gpui::Window) -> Result<isize, String> {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // Not `window.window_handle()`: gpui's inherent method of that name wins.
    let handle = HasWindowHandle::window_handle(window).map_err(|e| format!("no window handle: {e}"))?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return Err("not an AppKit window".into()) };
    let view = h.ns_view.as_ptr() as *mut Object;
    // SAFETY: `ns_view` is the live NSView of our window and we are on the main thread.
    let number: isize = unsafe {
        let ns_window: *mut Object = msg_send![view, window];
        if ns_window.is_null() {
            return Err("view has no window".into());
        }
        msg_send![ns_window, windowNumber]
    };
    Ok(number)
}

#[cfg(not(target_os = "macos"))]
fn window_number(_: &gpui::Window) -> Result<isize, String> {
    Err("screenshots need macOS".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hirc_core::event::Target;
    use hirc_core::store::LineKind;

    const DEMO: &str = include_str!("../fixtures/demo.toml");

    #[test]
    fn script_steps_parse() {
        assert_eq!(
            parse_script("key:escape;type:hi there;wait:200;shot:a.png;window:2"),
            [Step::Key("escape".into()), Step::Type("hi there".into()), Step::Wait(200), Step::Shot("a.png".into()), Step::Window(2)]
        );
        assert_eq!(parse_script("type:a:b; ;bogus;wait:x"), [Step::Type("a:b".into()), Step::Unknown("bogus".into()), Step::Unknown("wait:x".into())]);
    }

    #[test]
    fn size_parses() {
        assert_eq!(parse_size("900x600"), Some((900., 600.)));
        assert_eq!(parse_size("900"), None);
        assert_eq!(parse_size("0x0"), None);
    }

    #[test]
    fn demo_fixture_feeds_rich_events() {
        let fx = parse_fixture(DEMO).unwrap();
        assert!(fx.server.len() >= 2);
        let (events, sessions) = feed(&fx).unwrap();
        assert!(events.len() > 30, "got {}", events.len());
        assert_eq!(sessions.len(), fx.server.len());
        let lines: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Line { target, line, .. } => Some((target, line)),
                _ => None,
            })
            .collect();
        assert!(lines.iter().any(|(_, l)| l.kind == LineKind::Action));
        assert!(lines.iter().any(|(t, _)| matches!(t, Target::Query(n) if n == "carol")));
        let channels: std::collections::HashSet<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Joined { channel, .. } => Some(channel.clone()),
                _ => None,
            })
            .collect();
        assert!(channels.len() >= 3, "{channels:?}");
    }

    #[test]
    fn times_follow_base_plus_line_index() {
        let fx = parse_fixture(DEMO).unwrap();
        let (events, _) = feed(&fx).unwrap();
        let base: DateTime<Utc> = DEFAULT_BASE.parse().unwrap();
        let t = events.iter().find_map(|e| match e {
            Event::Line { line, .. } if line.nick.as_deref() == Some("bob") => Some(line.time),
            _ => None,
        });
        assert!(t.unwrap() > base);
    }

    #[test]
    fn echo_message_say_sends_only_the_echo() {
        let fx = parse_fixture(DEMO).unwrap();
        let (_, mut sessions) = feed(&fx).unwrap();
        let s = sessions.get_mut("demo").unwrap();
        assert!(s.caps().is_enabled("echo-message"));
        let out = s.command(Command::Say { network: "demo".into(), target: "#general".into(), text: "hi".into() }, Utc::now());
        assert!(out.iter().all(|o| !matches!(o, Output::Event(Event::Line { .. }))), "the server's echo is the copy we show: {out:?}");
        // Without echo-message the other network shows the line locally.
        let s = sessions.get_mut("other").unwrap();
        let out = s.command(Command::Say { network: "other".into(), target: "#rust".into(), text: "hi".into() }, Utc::now());
        assert!(out.iter().any(|o| matches!(o, Output::Event(Event::Line { .. }))));
    }

    #[test]
    fn bad_fixture_is_an_error() {
        assert!(parse_fixture("[[server]]\nname = 1").is_err());
        let fx = parse_fixture("[[server]]\nname='a'\nnick='n'\nlines=['']").unwrap();
        assert!(feed(&fx).is_err());
    }
}
