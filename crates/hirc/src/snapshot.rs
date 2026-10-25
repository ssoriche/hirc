//! Developer snapshot mode: a scripted, network-free run of the real UI that
//! saves screenshots, so the UI can be reviewed (and iterated on) without a
//! person at the keyboard. Only built with `--features snapshot`.
//!
//! Two renderers, both driven by the same fixture and script:
//!
//! - Window mode (default): opens the real window, drives it with a script,
//!   captures it with macOS `screencapture -l <window id>` and quits. The
//!   screen must be unlocked.
//! - Offscreen mode (`HIRC_OFFSCREEN=1`): no window and no AppKit app. A
//!   `HeadlessAppContext` (gpui `test-support`) with the macOS text system and
//!   the Metal headless renderer draws into a texture, and
//!   `capture_screenshot` reads it back. Needs no window server or unlocked
//!   screen; the simulated clock replaces real sleeps. Run it on the main
//!   thread (the binary does).
//!
//! Variables (all ignored unless `HIRC_FIXTURE` is set):
//!
//! | Variable | Effect |
//! |---|---|
//! | `HIRC_FIXTURE=<file.toml>` | No `conn::spawn`. Canned server lines go through a real `Session` per server; UI commands go to `session.command`, and only its `Event`s return. `Output::Send` never leaves the process; a faked server answers some sends through the session: a JOIN comes back as our JOIN plus a NAMES of just us (no topic), a PART as our PART, and on an `echo_message` server a PRIVMSG/NOTICE as its echo. Everything else is dropped. Connection status follows conn.rs: an `Output::Quit` (`/quit`) is `Offline`, an `Output::Stop` (e.g. a SASL failure) is `Failed { reason }`, and a server `ERROR` line closes the connection as `Disconnected { reason: <the ERROR text>, retry }`, with conn.rs's first retry delay. Either way that network reads no more fixture lines and later commands to it go nowhere (no reconnect is faked). |
//! | `HIRC_SCREENSHOT=<out.png>` | Final image, taken after the script. Window mode waits 300 ms for layout, captures, quits. |
//! | `HIRC_OFFSCREEN=1` | Offscreen mode (above). Exits 0 when done; on failure prints `snapshot: <reason>` and exits 1. |
//! | `HIRC_SIZE=WxH` | Logical size (default `1200x800`). |
//! | `HIRC_SCALE=<f>` | Device scale for offscreen output (default `2`). See `SCALE` below. |
//! | `HIRC_THEME=<name>` | Overrides the config's `theme` for this run; a missing theme exits 1. |
//! | `HIRC_SCRIPT="step;step"` | Steps, run once the fixture is fed (below). |
//!
//! Script steps, separated by `;` and trimmed. Each is followed by a few
//! frames (`run_until_parked` plus a simulated advance, offscreen):
//!
//! | Step | Effect |
//! |---|---|
//! | `window:N` | Go to window N. |
//! | `key:<ks>` | A keystroke through the normal key path, so vim modes apply: `key:escape`, `key:ctrl-d`, `key:alt-2`, `key:enter`. `+` is accepted for `-` (`cmd+k`). |
//! | `type:<text>` | Insert text (may contain `:`). Only inserts in Insert mode, so use `key:i` first when needed. |
//! | `wait:<ms>` | Advance the simulated clock (offscreen) or sleep (window). |
//! | `shot:<png>` | Save an image now. |
//! | `click:<Label>` | Click the centre of the first sidebar row whose label contains `<Label>`, ignoring case. No match prints `snapshot: no click target <Label>` and the script goes on. |
//! | `at:x,y` | Primary mouse down and up at logical (x, y). |
//! | `hover:x,y` | Mouse move to (x, y). |
//! | `scroll:<px>` | Scroll wheel over the buffer view; negative scrolls down. |
//!
//! An unknown step prints `snapshot: unknown step <s>` and the script goes on.
//!
//! SCALE: gpui's headless test window is fixed at 2x and exposes no public way
//! to change its platform scale (`Window::set_scale_factor` only changes the
//! layout side, which would mismatch the texture). Offscreen output is 2x;
//! `HIRC_SCALE=1` downsamples the 2x capture by averaging 2x2 blocks, and any
//! other value is an error.
//!
//! Fixture runs never read or write the window layout file, and `config.servers`
//! is replaced by the fixture's networks. Theme and font still come from the
//! normal config dir (`XDG_CONFIG_HOME`).
//!
//! Window id (window mode): the `AppKit` raw-window-handle gives the `NSView`;
//! `[[view window] windowNumber]` (via the `objc` crate gpui already uses) is
//! the `CGWindowID` that `screencapture -l` wants.
//!
//! Any failure prints `snapshot: <reason>` to stderr and exits non-zero.

use crate::app::{self, HircApp, Init};
use chrono::{DateTime, Duration as Delta, SecondsFormat, Utc};
use gpui::{
    AppContext, AsyncApp, HeadlessAppContext, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta,
    ScrollWheelEvent, TouchPhase, WindowHandle, point, px, size,
};
use hirc_core::config::{Config, Server};
use hirc_core::conn;
use hirc_core::event::{Command, ConnStatus, Event};
use hirc_core::proto::Message;
use hirc_core::session::{Output, Session, SessionConfig};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// What `Plan::channels` leaves for the offscreen runner to serve on the
/// foreground executor (no thread, so frames stay deterministic).
type Serve = (HashMap<String, Session>, UnboundedReceiver<Command>, UnboundedSender<Event>);

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

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Window(usize),
    Key(String),
    Type(String),
    Wait(u64),
    Shot(String),
    Click(String),
    At(f32, f32),
    Hover(f32, f32),
    Scroll(f32),
    Unknown(String),
}

/// `x,y` in logical px.
fn parse_point(s: &str) -> Option<(f32, f32)> {
    let (x, y) = s.split_once(',')?;
    let (x, y): (f32, f32) = (x.trim().parse().ok()?, y.trim().parse().ok()?);
    (x.is_finite() && y.is_finite()).then_some((x, y))
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
                // github-rs writes `cmd+k`; gpui writes `cmd-k`.
                Some(("key", k)) => Step::Key(k.trim().replace('+', "-")),
                Some(("type", t)) => Step::Type(t.to_owned()),
                Some(("wait", ms)) => ms.trim().parse().map_or_else(|_| unknown(), Step::Wait),
                Some(("shot", p)) => Step::Shot(p.trim().to_owned()),
                Some(("click", l)) if !l.trim().is_empty() => Step::Click(l.trim().to_owned()),
                Some(("at", p)) => parse_point(p).map_or_else(unknown, |(x, y)| Step::At(x, y)),
                Some(("hover", p)) => parse_point(p).map_or_else(unknown, |(x, y)| Step::Hover(x, y)),
                Some(("scroll", d)) => d.trim().parse().ok().filter(|d: &f32| d.is_finite()).map_or_else(unknown, Step::Scroll),
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

/// Device scale for offscreen output: a positive factor up to 8.
pub fn parse_scale(s: &str) -> Option<f32> {
    s.trim().parse().ok().filter(|f: &f32| *f > 0. && *f <= 8.)
}

/// `1` or `true`.
pub fn parse_flag(s: &str) -> bool {
    matches!(s.trim(), "1" | "true")
}

fn session_config(s: &FixtureServer) -> SessionConfig {
    SessionConfig { nickname: s.nick.clone(), username: s.nick.clone(), realname: s.nick.clone(), ..Default::default() }
}

/// Hand one server line to the session, as conn.rs's read loop does, and
/// collect the `Event`s it passes on. Returns whether the connection is over:
/// a `Stop` ends it there (later outputs are dropped, as live) with the
/// status conn.rs reports, and after an `ERROR` the server closes the
/// socket. `Send`s go nowhere.
fn take(session: &mut Session, network: &str, msg: Message, now: DateTime<Utc>, events: &mut Vec<Event>) -> bool {
    let error = msg.command == "ERROR";
    for out in session.handle(msg, now) {
        match out {
            Output::Event(e) => events.push(e),
            Output::Stop(_) | Output::Quit => {
                events.push(ended(network, out));
                return true;
            }
            Output::Send(_) => {}
        }
    }
    if error {
        events.push(closed(network, session));
    }
    error
}

/// conn.rs: a session's `Stop` won't retry until /reconnect, and a `Quit`
/// is the user going offline.
fn ended(network: &str, out: Output) -> Event {
    let status = match out {
        Output::Stop(reason) => ConnStatus::Failed { reason },
        _ => ConnStatus::Offline,
    };
    Event::Status { network: network.to_owned(), status }
}

/// conn.rs: the server closed the connection, giving its `ERROR` text as
/// the reason; a registered session retries after the shortest backoff.
fn closed(network: &str, session: &Session) -> Event {
    let reason = session.closing().unwrap_or("connection closed").to_owned();
    let retry = conn::retry_delay(Duration::ZERO, true).0;
    Event::Status { network: network.to_owned(), status: ConnStatus::Disconnected { reason, retry } }
}

fn feed_line(session: &mut Session, network: &str, line: &str, now: DateTime<Utc>, events: &mut Vec<Event>) -> Result<bool, String> {
    let msg = Message::parse(line).map_err(|e| format!("bad fixture line {line:?}: {e}"))?;
    Ok(take(session, network, msg, now, events))
}

/// When a fixture's lines end: a second after the latest one.
fn clock(fx: &Fixture) -> Result<DateTime<Utc>, String> {
    let base = DateTime::parse_from_rfc3339(fx.base_time.as_deref().unwrap_or(DEFAULT_BASE)).map_err(|e| format!("bad base_time: {e}"))?.with_timezone(&Utc);
    let times = fx.server.iter().flat_map(|srv| {
        srv.lines.iter().enumerate().map(move |(i, line)| {
            let at = Message::parse(line).ok().and_then(|m| m.tag("time").and_then(|t| DateTime::parse_from_rfc3339(t).ok())).map(|t| t.with_timezone(&Utc));
            at.unwrap_or_else(|| base + Delta::seconds(i as i64))
        })
    });
    Ok(times.max().unwrap_or(base) + Delta::seconds(1))
}

/// Run every server's lines through a real `Session`, as the network
/// thread would. Returns the events for the UI and the sessions, which keep
/// answering commands afterwards. A server whose connection ended (`Stop`,
/// `ERROR`) reads no further lines and has no session left.
pub fn feed(fx: &Fixture) -> Result<(Vec<Event>, HashMap<String, Session>), String> {
    let base = DateTime::parse_from_rfc3339(fx.base_time.as_deref().unwrap_or(DEFAULT_BASE)).map_err(|e| format!("bad base_time: {e}"))?.with_timezone(&Utc);
    let mut events = Vec::new();
    let mut sessions = HashMap::new();
    'servers: for srv in &fx.server {
        let mut session = Session::new(&srv.name, session_config(srv));
        session.start(); // its sends go nowhere
        if srv.echo_message {
            // Through `handle`, so no private API is needed.
            for line in ["CAP * LS :echo-message", "CAP * ACK :echo-message"] {
                feed_line(&mut session, &srv.name, &format!(":irc.example {line}"), base, &mut events)?;
            }
        }
        for (i, line) in srv.lines.iter().enumerate() {
            let at = Message::parse(line).ok().and_then(|m| m.tag("time").and_then(|t| DateTime::parse_from_rfc3339(t).ok())).map(|t| t.with_timezone(&Utc));
            let now = at.unwrap_or_else(|| base + Delta::seconds(i as i64));
            if feed_line(&mut session, &srv.name, line, now, &mut events)? {
                continue 'servers;
            }
        }
        sessions.insert(srv.name.clone(), session);
    }
    Ok((events, sessions))
}

/// What a server sends back for `sent`. It always confirms our JOIN (with a
/// minimal NAMES, just us, and no topic) and PART; on an `echo-message`
/// server our PRIVMSG/NOTICE also comes back, with a server time and msgid.
/// Anything else gets no reply.
fn fake_replies(sent: &Message, nick: &str, echo_message: bool, now: DateTime<Utc>, n: u64) -> Vec<Message> {
    let from = |source: String, command: &str, params: Vec<String>| {
        let mut m = Message::new(command, params);
        m.source = Some(source);
        m
    };
    let us = || format!("{nick}!u@fixture");
    let server = || "irc.fixture".to_owned();
    // `JOIN #a,#b keys` and `PART #a,#b :why` answer once per channel.
    let channels = || sent.params.first().into_iter().flat_map(|c| c.split(',')).filter(|c| !c.is_empty()).map(str::to_owned);
    match sent.command.as_str() {
        "JOIN" => channels()
            .flat_map(|chan| {
                [
                    from(us(), "JOIN", vec![chan.clone()]),
                    from(server(), "353", vec![nick.into(), "=".into(), chan.clone(), nick.into()]),
                    from(server(), "366", vec![nick.into(), chan, "End of /NAMES list.".into()]),
                ]
            })
            .collect(),
        "PART" => channels().map(|chan| from(us(), "PART", std::iter::once(chan).chain(sent.params.get(1).cloned()).collect())).collect(),
        "PRIVMSG" | "NOTICE" if echo_message => {
            let mut echo = sent.clone();
            echo.tags = vec![("time".into(), Some(now.to_rfc3339_opts(SecondsFormat::Millis, true))), ("msgid".into(), Some(format!("echo-{n}")))];
            echo.source = Some(us());
            vec![echo]
        }
        _ => Vec::new(),
    }
}

/// Answer one UI command through the session and return the events for the
/// UI, and whether the connection is over. Sends never leave the process;
/// the server's replies to them (`fake_replies`) go back through the
/// session, so on an `echo-message` session the PRIVMSG/NOTICE echo is the
/// copy the UI shows.
fn answer(session: &mut Session, cmd: Command, now: DateTime<Utc>, echoes: &mut u64) -> (Vec<Event>, bool) {
    let network = cmd.network().to_owned();
    let mut events = Vec::new();
    for out in session.command(cmd, now) {
        match out {
            Output::Event(e) => events.push(e),
            Output::Send(sent) => {
                *echoes += 1;
                for reply in fake_replies(&sent, session.nick(), session.caps().is_enabled("echo-message"), now, *echoes) {
                    if take(session, &network, reply, now, &mut events) {
                        return (events, true);
                    }
                }
            }
            Output::Stop(_) | Output::Quit => {
                events.push(ended(&network, out));
                return (events, true);
            }
        }
    }
    (events, false)
}

/// One UI command, for both command loops (window thread, offscreen task).
/// A network whose connection ended loses its session, so later commands to
/// it go nowhere, as conn.rs drops them until /reconnect.
fn serve(sessions: &mut HashMap<String, Session>, cmd: Command, now: DateTime<Utc>, echoes: &mut u64) -> Vec<Event> {
    let network = cmd.network().to_owned();
    let Some(session) = sessions.get_mut(&network) else { return Vec::new() };
    let (events, over) = answer(session, cmd, now, echoes);
    if over {
        sessions.remove(&network);
    }
    events
}

pub struct Plan {
    pub size: (f32, f32),
    fixture: Fixture,
    script: Vec<Step>,
    shot: Option<String>,
    canned: Option<(Vec<Event>, HashMap<String, Session>)>,
    /// Set once the feeder has sent every canned event.
    fed: Arc<AtomicBool>,
    pub offscreen: bool,
    scale: f32,
    pub theme: Option<String>,
    serve: Option<Serve>,
    /// The fixture's "now": a second after its last line. Replies to
    /// commands and hirc's own lines carry it, so a shot shows one clock.
    pub clock: DateTime<Utc>,
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
        let clock = clock(&fixture)?;
        let size = match std::env::var("HIRC_SIZE") {
            Ok(s) => parse_size(&s).ok_or_else(|| format!("bad HIRC_SIZE {s:?}, want WxH"))?,
            Err(_) => (1200., 800.),
        };
        let scale = match std::env::var("HIRC_SCALE") {
            Ok(s) => parse_scale(&s).ok_or_else(|| format!("bad HIRC_SCALE {s:?}, want a number above 0"))?,
            Err(_) => 2.,
        };
        let script = std::env::var("HIRC_SCRIPT").map(|s| parse_script(&s)).unwrap_or_default();
        let offscreen = std::env::var("HIRC_OFFSCREEN").is_ok_and(|v| parse_flag(&v));
        Ok(Some(Plan {
            size,
            fixture,
            script,
            shot: std::env::var("HIRC_SCREENSHOT").ok(),
            canned,
            fed: Arc::default(),
            offscreen,
            scale,
            theme: std::env::var("HIRC_THEME").ok().filter(|t| !t.is_empty()),
            serve: None,
            clock,
        }))
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
        if self.offscreen {
            for ev in events {
                let _ = ev_tx.send(ev);
            }
            self.fed.store(true, Ordering::SeqCst);
            self.serve = Some((sessions, cmd_rx, ev_tx));
            return (cmd_tx, ev_rx);
        }
        let fed = self.fed.clone();
        let clock = self.clock;
        std::thread::spawn(move || {
            for ev in events {
                let _ = ev_tx.send(ev);
            }
            fed.store(true, Ordering::SeqCst);
            let mut echoes = 0;
            while let Some(cmd) = cmd_rx.blocking_recv() {
                for e in serve(&mut sessions, cmd, clock, &mut echoes) {
                    let _ = ev_tx.send(e);
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

/// Inject one input step into the window; shared by both modes. `Wait` and
/// `Shot` are the runner's business. Events go through `update_window`, not
/// the root entity's update: handlers (clicks, actions) update `HircApp`
/// themselves and gpui forbids a nested lease.
fn inject<C: AppContext>(step: &Step, cx: &mut C, handle: WindowHandle<HircApp>) -> Result<(), String> {
    let gone = |_| "window closed".to_owned();
    let send = |cx: &mut C, e: PlatformInput| {
        cx.update_window(handle.into(), |_, window, cx| {
            window.dispatch_event(e, cx);
        })
        .map_err(gone)
    };
    let at = |x: f32, y: f32| point(px(x), px(y));
    let modifiers = Modifiers::default();
    let click = |cx: &mut C, position| -> Result<(), String> {
        send(cx, PlatformInput::MouseMove(MouseMoveEvent { position, pressed_button: None, modifiers }))?;
        send(cx, PlatformInput::MouseDown(MouseDownEvent { position, modifiers, button: MouseButton::Left, click_count: 1, first_mouse: false }))?;
        send(cx, PlatformInput::MouseUp(MouseUpEvent { position, modifiers, button: MouseButton::Left, click_count: 1 }))
    };
    match step {
        Step::Window(n) => handle.update(cx, |app, _, cx| app.goto_window(*n, cx)).map_err(gone),
        Step::Key(k) => {
            let ks = Keystroke::parse(k).map_err(|e| format!("bad key {k:?}: {e}"))?;
            cx.update_window(handle.into(), |_, window, cx| window.dispatch_keystroke(ks, cx)).map_err(gone).map(|_| ())
        }
        Step::Type(t) => handle.update(cx, |app, window, cx| app.type_text(t, window, cx)).map_err(gone),
        Step::Click(label) => match handle.update(cx, |app, _, _| app.sidebar_point(label)).map_err(gone)? {
            Some(Ok((x, y))) => click(cx, at(x, y)),
            Some(Err(why)) => {
                eprintln!("snapshot: click target {label}: {why}");
                Ok(())
            }
            None => {
                eprintln!("snapshot: no click target {label}");
                Ok(())
            }
        },
        Step::At(x, y) => click(cx, at(*x, *y)),
        Step::Hover(x, y) => send(cx, PlatformInput::MouseMove(MouseMoveEvent { position: at(*x, *y), pressed_button: None, modifiers })),
        Step::Scroll(d) => {
            // Over the buffer view: right of the sidebar, mid-height.
            let s = cx.update_window(handle.into(), |_, window, _| window.bounds().size).map_err(gone)?;
            let side = handle.update(cx, |app, _, _| app.sidebar_width()).map_err(gone)?;
            let position = at(side + (f32::from(s.width) - side) / 2., f32::from(s.height) / 2.);
            let delta = ScrollDelta::Pixels(point(px(0.), px(*d)));
            send(cx, PlatformInput::ScrollWheel(ScrollWheelEvent { position, delta, modifiers, touch_phase: TouchPhase::Moved }))
        }
        Step::Unknown(s) => {
            eprintln!("snapshot: unknown step {s}");
            Ok(())
        }
        Step::Wait(_) | Step::Shot(_) => Ok(()),
    }
}

async fn apply(step: &Step, cx: &mut AsyncApp, handle: WindowHandle<HircApp>) -> Result<(), String> {
    match step {
        Step::Wait(ms) => {
            cx.background_executor().timer(Duration::from_millis(*ms)).await;
            Ok(())
        }
        Step::Shot(path) => capture(cx, handle, path).await,
        _ => inject(step, cx, handle),
    }
}

// ---- offscreen ----

/// Frames after each step: let tasks run, advance the simulated clock, and
/// force a redraw (an update ends by drawing dirty windows).
fn frames(cx: &mut HeadlessAppContext, handle: WindowHandle<HircApp>) {
    for _ in 0..3 {
        cx.run_until_parked();
        cx.advance_clock(FRAME);
        cx.run_until_parked();
        let _ = cx.update_window(handle.into(), |_, window, _| window.refresh());
    }
}

fn shot(cx: &mut HeadlessAppContext, handle: WindowHandle<HircApp>, scale: f32, path: &str) -> Result<(), String> {
    frames(cx, handle);
    let img = cx.capture_screenshot(handle.into()).map_err(|e| format!("cannot render offscreen: {e}"))?;
    let img = if scale == 2. { img } else { downsample_2x(&img) };
    if let Some(dir) = std::path::Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    img.save(path).map_err(|e| format!("cannot write {path}: {e}"))
}

/// Halve each dimension by averaging 2x2 blocks (HIRC_SCALE=1).
fn downsample_2x(img: &image::RgbaImage) -> image::RgbaImage {
    let (w, h) = (img.width() / 2, img.height() / 2);
    image::RgbaImage::from_fn(w, h, |x, y| {
        let mut sum = [0u32; 4];
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            for (i, v) in img.get_pixel(x * 2 + dx, y * 2 + dy).0.iter().enumerate() {
                sum[i] += u32::from(*v);
            }
        }
        image::Rgba(sum.map(|v| (v / 4) as u8))
    })
}

/// Never returns: exits 0 after the run, or 1 with `snapshot: <reason>`.
pub fn run_offscreen(plan: Plan, init: Init) -> ! {
    match offscreen(plan, init) {
        Ok(()) => std::process::exit(0),
        Err(e) => fail(&e),
    }
}

fn offscreen(mut plan: Plan, init: Init) -> Result<(), String> {
    if plan.scale != 2. && plan.scale != 1. {
        return Err(format!("HIRC_SCALE={} unsupported: offscreen output is 2 (default) or 1; see snapshot.rs", plan.scale));
    }
    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(text_system, Arc::new(()), gpui_platform::current_headless_renderer);
    cx.update(|cx| {
        crate::input::bind_keys(cx);
        app::bind_keys(cx);
    });
    let (w, h) = plan.size;
    let handle = cx
        .open_window(size(px(w), px(h)), |window, cx| cx.new(|cx| HircApp::new(init, window, cx)))
        .map_err(|e| format!("cannot open headless window: {e}"))?;
    if let Some((mut sessions, mut cmd_rx, ev_tx)) = plan.serve.take() {
        let clock = plan.clock;
        cx.update(|cx| {
            cx.spawn(async move |_| {
                let mut echoes = 0;
                while let Some(cmd) = cmd_rx.recv().await {
                    for e in serve(&mut sessions, cmd, clock, &mut echoes) {
                        let _ = ev_tx.send(e);
                    }
                }
            })
            .detach();
        });
    }
    frames(&mut cx, handle);
    for step in &plan.script {
        match step {
            Step::Wait(ms) => cx.advance_clock(Duration::from_millis(*ms)),
            Step::Shot(path) => shot(&mut cx, handle, plan.scale, path)?,
            _ => inject(step, &mut cx, handle)?,
        }
        frames(&mut cx, handle);
    }
    if let Some(path) = &plan.shot {
        shot(&mut cx, handle, plan.scale, path)?;
    }
    Ok(())
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
    fn github_rs_steps_parse() {
        assert_eq!(
            parse_script("click:general;at:10,20;hover:5,6;scroll:-400;key:cmd+k;wait:50;shot:a.png"),
            [
                Step::Click("general".into()),
                Step::At(10., 20.),
                Step::Hover(5., 6.),
                Step::Scroll(-400.),
                Step::Key("cmd-k".into()),
                Step::Wait(50),
                Step::Shot("a.png".into())
            ]
        );
        assert_eq!(parse_script("key:ctrl+shift+a"), [Step::Key("ctrl-shift-a".into())]);
        assert_eq!(parse_script("click:  my chan "), [Step::Click("my chan".into())]);
    }

    #[test]
    fn bad_numbers_are_unknown() {
        assert_eq!(
            parse_script("at:x,1;at:1;hover:1,y;scroll:far;click:"),
            [
                Step::Unknown("at:x,1".into()),
                Step::Unknown("at:1".into()),
                Step::Unknown("hover:1,y".into()),
                Step::Unknown("scroll:far".into()),
                Step::Unknown("click:".into())
            ]
        );
    }

    #[test]
    fn scale_parses() {
        assert_eq!(parse_scale("2"), Some(2.));
        assert_eq!(parse_scale(" 1.5 "), Some(1.5));
        assert_eq!(parse_scale("0"), None);
        assert_eq!(parse_scale("-1"), None);
        assert_eq!(parse_scale("x"), None);
        assert_eq!(parse_scale("99"), None);
    }

    #[test]
    fn offscreen_flag_parses() {
        assert!(parse_flag("1"));
        assert!(parse_flag("true"));
        assert!(!parse_flag("0"));
        assert!(!parse_flag(""));
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
    fn the_fixture_clock_follows_its_last_line() {
        let fx = parse_fixture(DEMO).unwrap();
        let (events, _) = feed(&fx).unwrap();
        let last = events.iter().filter_map(|e| if let Event::Line { line, .. } = e { Some(line.time) } else { None }).max().unwrap();
        let now = clock(&fx).unwrap();
        assert!(now > last && now - last < Delta::minutes(1), "{now} vs {last}");
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
    fn fake_server_echoes_privmsg_and_notice() {
        let now: DateTime<Utc> = DEFAULT_BASE.parse().unwrap();
        let sent = Message::parse("PRIVMSG #general :hi there").unwrap();
        let [echo] = fake_replies(&sent, "me", true, now, 3).try_into().unwrap();
        assert_eq!(echo.nick(), Some("me"));
        assert_eq!(echo.command, "PRIVMSG");
        assert_eq!(echo.params, ["#general", "hi there"]);
        assert_eq!(echo.tag("time"), Some("2026-01-01T12:00:00.000Z"));
        assert_eq!(echo.tag("msgid"), Some("echo-3"));
        assert_eq!(fake_replies(&Message::parse("NOTICE bob :yo").unwrap(), "me", true, now, 4).len(), 1);
        // Without echo-message the server sends nothing back.
        assert!(fake_replies(&sent, "me", false, now, 5).is_empty());
    }

    #[test]
    fn fake_server_ignores_everything_else() {
        let now: DateTime<Utc> = DEFAULT_BASE.parse().unwrap();
        for line in ["NICK x", "PING :x", "TAGMSG #a", "QUIT :bye", "TOPIC #a :x"] {
            for echo in [true, false] {
                assert!(fake_replies(&Message::parse(line).unwrap(), "me", echo, now, 0).is_empty(), "{line}");
            }
        }
    }

    #[test]
    fn say_on_an_echo_session_yields_one_line() {
        let fx = parse_fixture(DEMO).unwrap();
        let (_, mut sessions) = feed(&fx).unwrap();
        let s = sessions.get_mut("demo").unwrap();
        let mut n = 0;
        let (events, _) = answer(s, Command::Say { network: "demo".into(), target: "#general".into(), text: "hello world".into() }, Utc::now(), &mut n);
        let lines: Vec<_> = events.iter().filter(|e| matches!(e, Event::Line { .. })).collect();
        assert_eq!(lines.len(), 1, "{events:?}");
        assert!(matches!(lines[0], Event::Line { line, .. } if line.text == "hello world"));
        // A session without echo-message shows the line locally, once.
        let s = sessions.get_mut("other").unwrap();
        let (events, _) = answer(s, Command::Say { network: "other".into(), target: "#rust".into(), text: "yo".into() }, Utc::now(), &mut n);
        assert_eq!(events.iter().filter(|e| matches!(e, Event::Line { .. })).count(), 1);
    }

    fn status_of<'a>(events: &'a [Event], net: &str) -> Option<&'a ConnStatus> {
        events.iter().rev().find_map(|e| match e {
            Event::Status { network, status } if network == net => Some(status),
            _ => None,
        })
    }

    #[test]
    fn stop_is_the_status_conn_reports() {
        let fx = parse_fixture(DEMO).unwrap();
        let (_, mut sessions) = feed(&fx).unwrap();
        let mut n = 0;
        let s = sessions.get_mut("demo").unwrap();
        let (events, over) = answer(s, Command::Disconnect { network: "demo".into(), reason: Some("bye".into()) }, Utc::now(), &mut n);
        assert!(over);
        // A quit is the user going offline, not a failure.
        assert_eq!(status_of(&events, "demo"), Some(&ConnStatus::Offline), "{events:?}");
    }

    #[test]
    fn a_stopped_network_answers_nothing() {
        let fx = parse_fixture(DEMO).unwrap();
        let (_, mut sessions) = feed(&fx).unwrap();
        let mut n = 0;
        let events = serve(&mut sessions, Command::Disconnect { network: "other".into(), reason: None }, Utc::now(), &mut n);
        assert_eq!(status_of(&events, "other"), Some(&ConnStatus::Offline));
        let join = Command::Join { network: "other".into(), channel: "#late".into(), key: None };
        assert!(serve(&mut sessions, join, Utc::now(), &mut n).is_empty());
        // The other network still answers.
        let join = Command::Join { network: "demo".into(), channel: "#late".into(), key: None };
        assert!(!serve(&mut sessions, join, Utc::now(), &mut n).is_empty());
    }

    #[test]
    fn error_line_disconnects_the_network() {
        let fx = parse_fixture(
            "[[server]]\nname='a'\nnick='me'\nlines=[':irc.example 001 me :hi', 'ERROR :Closing Link: me (Quit)', ':irc.example NOTICE me :never read']\n\n[[server]]\nname='b'\nnick='me'\nlines=[':irc.example 001 me :hi']",
        )
        .unwrap();
        let (events, sessions) = feed(&fx).unwrap();
        // The ERROR text is the reason, and the retry is conn.rs's first.
        let retry = conn::retry_delay(std::time::Duration::ZERO, true).0;
        assert_eq!(status_of(&events, "a"), Some(&ConnStatus::Disconnected { reason: "Closing Link: me (Quit)".into(), retry }), "{events:?}");
        // The socket is gone: nothing after ERROR is read.
        assert!(!events.iter().any(|e| matches!(e, Event::Line { line, .. } if line.text.contains("never read"))), "{events:?}");
        assert!(!sessions.contains_key("a"));
        assert!(matches!(status_of(&events, "b"), Some(ConnStatus::Connected { .. })));
    }

    #[test]
    fn join_and_part_come_back_from_the_server() {
        let fx = parse_fixture(DEMO).unwrap();
        let (_, mut sessions) = feed(&fx).unwrap();
        let mut n = 0;
        // With and without echo-message: a server always confirms these.
        for net in ["demo", "other"] {
            let s = sessions.get_mut(net).unwrap();
            let me = s.nick().to_owned();
            let (events, over) = answer(s, Command::Join { network: net.into(), channel: "#fresh".into(), key: None }, Utc::now(), &mut n);
            assert!(!over);
            assert!(events.contains(&Event::Joined { network: net.into(), channel: "#fresh".into() }), "{net}: {events:?}");
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, Event::Line { target: Target::Channel(c), line, .. } if c == "#fresh" && line.kind == LineKind::Join && line.own))
            );
            assert!(!events.iter().any(|e| matches!(e, Event::Topic { .. })), "no topic: {events:?}");
            let part = Command::Part { network: net.into(), channel: "#fresh".into(), reason: Some("bye now".into()) };
            let (events, _) = answer(s, part, Utc::now(), &mut n);
            assert!(events.contains(&Event::Parted { network: net.into(), channel: "#fresh".into() }), "{net}: {events:?}");
            assert!(events.iter().any(|e| matches!(e, Event::Line { line, .. } if line.kind == LineKind::Part && line.text == format!("{me} left (bye now)"))));
        }
    }

    #[test]
    fn fake_join_has_minimal_names_and_no_topic() {
        let now: DateTime<Utc> = DEFAULT_BASE.parse().unwrap();
        let wire: Vec<_> = fake_replies(&Message::parse("JOIN #fresh").unwrap(), "me", false, now, 0).iter().map(Message::to_wire).collect();
        assert_eq!(wire, [":me!u@fixture JOIN #fresh", ":irc.fixture 353 me = #fresh me", ":irc.fixture 366 me #fresh :End of /NAMES list."]);
        let wire: Vec<_> = fake_replies(&Message::parse("PART #fresh :see ya").unwrap(), "me", false, now, 0).iter().map(Message::to_wire).collect();
        assert_eq!(wire, [":me!u@fixture PART #fresh :see ya"]);
        let wire: Vec<_> = fake_replies(&Message::parse("PART #fresh").unwrap(), "me", false, now, 0).iter().map(Message::to_wire).collect();
        assert_eq!(wire, [":me!u@fixture PART #fresh"]);
    }

    #[test]
    fn nick_gets_no_reply() {
        let fx = parse_fixture(DEMO).unwrap();
        let (_, mut sessions) = feed(&fx).unwrap();
        let mut n = 0;
        for net in ["demo", "other"] {
            let s = sessions.get_mut(net).unwrap();
            let (events, over) = answer(s, Command::Nick { network: net.into(), nick: "someone".into() }, Utc::now(), &mut n);
            assert!(events.is_empty() && !over, "{net}: {events:?}");
        }
    }

    #[test]
    fn bad_fixture_is_an_error() {
        assert!(parse_fixture("[[server]]\nname = 1").is_err());
        let fx = parse_fixture("[[server]]\nname='a'\nnick='n'\nlines=['']").unwrap();
        assert!(feed(&fx).is_err());
    }
}
