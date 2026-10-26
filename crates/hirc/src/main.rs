//! The UI never waits on the network: see app.rs.
mod app;
mod input;
mod links;
mod metrics;
mod scroll;
#[cfg(feature = "snapshot")]
mod snapshot;
mod theme;
mod views;
mod vim;
mod wrap;

use app::{HircApp, Init, Quit};
use gpui::{App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};
use hirc_core::config;
use std::time::Duration;

/// The system monospace face, for when the config names no font.
#[cfg(target_os = "macos")]
const DEFAULT_FAMILY: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
const DEFAULT_FAMILY: &str = "monospace";

/// How long a quit waits for the history cache to be written.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Quit once the lines queued for the history cache are written, or after
/// `FLUSH_TIMEOUT`. The write waits on the disk, so it runs on its own
/// thread; the UI thread only awaits the answer.
fn quit(flush: Option<hirc_core::conn::Flush>, cx: &mut App) {
    let Some(flush) = flush else { return cx.quit() };
    let written = cx.background_executor().spawn(async move {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(flush.wait());
        });
        rx.recv_timeout(FLUSH_TIMEOUT).ok()
    });
    cx.spawn(async move |cx| {
        match written.await {
            Some(true) => {}
            Some(false) => tracing::warn!("history cache: some lines were not written"),
            None => tracing::warn!("history cache: not written within {FLUSH_TIMEOUT:?}; quitting anyway"),
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

fn main() {
    tracing_subscriber::fmt::init();
    let dir = config::dir();
    let mut banner = None;
    let mut hint = None;
    let path = dir.join("config.toml");
    #[cfg_attr(not(feature = "snapshot"), allow(unused_mut))]
    let mut config = match config::load(&path) {
        Ok(c) => c,
        // No config is a first run, not an error: a quiet note, not a banner.
        Err(e) if e.is_missing() => {
            hint = Some(config::tilde_home(&format!("No config at {}; using the built-in defaults", path.display())));
            config::Config::default()
        }
        Err(e) => {
            banner = Some(config::tilde_home(&format!("{e} — using the built-in defaults")));
            config::Config::default()
        }
    };
    #[cfg_attr(not(feature = "snapshot"), allow(unused_mut))]
    let mut theme = match config.theme.as_deref().map(|name| hirc_core::theme::load(&dir, name)) {
        Some(Ok(t)) => t,
        Some(Err(e)) => {
            banner.get_or_insert(config::tilde_home(&format!("{e} — using the default theme")));
            Default::default()
        }
        None => Default::default(),
    };
    #[cfg(feature = "snapshot")]
    let mut snap = snapshot::Plan::from_env().unwrap_or_else(|e| snapshot::fail(&e));
    #[cfg(feature = "snapshot")]
    if let Some(plan) = &snap {
        plan.apply(&mut config);
        if let Some(name) = &plan.theme {
            theme = hirc_core::theme::load(&dir, name).unwrap_or_else(|e| snapshot::fail(&format!("cannot load theme {name}: {e}")));
        }
    }
    let networks: Vec<String> = config.servers.keys().cloned().collect();
    let servers: Vec<(String, Vec<String>)> = config.servers.iter().map(|(name, s)| (name.clone(), s.channels.clone())).collect();
    // The design is a monospace grid; without a configured family, use the
    // system's monospace face rather than gpui's proportional default.
    let font_family = config.font.family.clone().or_else(|| Some(DEFAULT_FAMILY.to_owned()));
    let font_size = config.font.size.unwrap_or(14.);
    let start = |config| {
        let (commands, events, flush) = hirc_core::conn::spawn(config, Some(hirc_core::cache::path()));
        (commands, events, Some(flush))
    };
    #[cfg(not(feature = "snapshot"))]
    let (commands, events, flush) = start(config);
    // A snapshot run never touches the history cache.
    #[cfg(feature = "snapshot")]
    let (commands, events, flush) = match &mut snap {
        Some(plan) => {
            let (commands, events) = plan.channels();
            (commands, events, None)
        }
        None => start(config),
    };
    let no_layout_file = cfg!(feature = "snapshot") && {
        #[cfg(feature = "snapshot")]
        let on = snap.is_some();
        #[cfg(not(feature = "snapshot"))]
        let on = false;
        on
    };
    #[cfg(feature = "snapshot")]
    let (width, height) = snap.as_ref().map_or((1200., 800.), |p| p.size);
    #[cfg(not(feature = "snapshot"))]
    let (width, height) = (1200., 800.);

    #[cfg(feature = "snapshot")]
    let clock = snap.as_ref().map(|p| p.clock);
    #[cfg(not(feature = "snapshot"))]
    let clock = None;
    // A snapshot run (the same test) never opens a browser.
    let dry_open = no_layout_file;
    let init = Init { networks, servers, theme, font_family, font_size, banner, hint, commands, events, no_layout_file, clock, dry_open };
    #[cfg(feature = "snapshot")]
    if let Some(plan) = snap.take_if(|p| p.offscreen) {
        snapshot::run_offscreen(plan, init);
    }

    gpui_platform::application().run(move |cx: &mut App| {
        input::bind_keys(cx);
        app::bind_keys(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        let quitting = flush.clone();
        cx.on_action(move |_: &Quit, cx| quit(quitting.clone(), cx));
        // A quit that skips the Quit action (from the OS, say): gpui gives
        // quit handlers only a moment, but what is written in it is kept.
        if let Some(flush) = flush.clone() {
            cx.on_app_quit(move |cx| {
                let flush = flush.clone();
                let written = cx.background_executor().spawn(async move { flush.wait() });
                async move {
                    if !written.await {
                        tracing::warn!("history cache: some lines were not written");
                    }
                }
            })
            .detach();
        }
        cx.on_window_closed(move |cx, _id| {
            if cx.windows().is_empty() {
                quit(flush.clone(), cx);
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let _window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions { title: Some("hirc".into()), ..Default::default() }),
                    app_id: Some("hirc".into()),
                    window_min_size: Some(size(px(600.), px(400.))),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| HircApp::new(init, window, cx)),
            )
            .expect("open window");
        #[cfg(feature = "snapshot")]
        if let Some(plan) = snap {
            cx.spawn(async move |cx| plan.run(cx, _window).await).detach();
        }
        cx.activate(true);
    });
}
