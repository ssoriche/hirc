//! The UI never waits on the network: see app.rs.
mod app;
mod input;
mod metrics;
mod scroll;
#[cfg(feature = "snapshot")]
mod snapshot;
mod theme;
mod views;
mod vim;

use app::{HircApp, Init, Quit};
use gpui::{App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};
use hirc_core::config;

/// The system monospace face, for when the config names no font.
#[cfg(target_os = "macos")]
const DEFAULT_FAMILY: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
const DEFAULT_FAMILY: &str = "monospace";

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
    #[cfg(not(feature = "snapshot"))]
    let (commands, events) = hirc_core::conn::spawn(config);
    #[cfg(feature = "snapshot")]
    let (commands, events) = match &mut snap {
        Some(plan) => plan.channels(),
        None => hirc_core::conn::spawn(config),
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
    let init = Init { networks, servers, theme, font_family, font_size, banner, hint, commands, events, no_layout_file, clock };
    #[cfg(feature = "snapshot")]
    if let Some(plan) = snap.take_if(|p| p.offscreen) {
        snapshot::run_offscreen(plan, init);
    }

    gpui_platform::application().run(move |cx: &mut App| {
        input::bind_keys(cx);
        app::bind_keys(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _id| {
            if cx.windows().is_empty() {
                cx.quit();
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
