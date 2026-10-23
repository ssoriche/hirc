// The UI never waits on the network: see app.rs.
mod app;
mod input;
mod theme;
mod views;
mod vim;

use app::{HircApp, Init, Quit};
use gpui::{App, Application, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, prelude::*, px, size};
use hirc_core::config;

fn main() {
    tracing_subscriber::fmt::init();
    let dir = config::dir();
    let mut banner = None;
    let config = match config::load(&dir.join("config.toml")) {
        Ok(c) => c,
        Err(e) => {
            banner = Some(e.to_string());
            config::Config::default()
        }
    };
    let theme = match config.theme.as_deref().map(|name| hirc_core::theme::load(&dir, name)) {
        Some(Ok(t)) => t,
        Some(Err(e)) => {
            banner.get_or_insert(e.to_string());
            Default::default()
        }
        None => Default::default(),
    };
    let networks: Vec<String> = config.servers.keys().cloned().collect();
    let servers: Vec<(String, Vec<String>)> = config.servers.iter().map(|(name, s)| (name.clone(), s.channels.clone())).collect();
    let font_family = config.font.family.clone();
    let font_size = config.font.size.unwrap_or(14.);
    let (commands, events) = hirc_core::conn::spawn(config);

    Application::new().run(move |cx: &mut App| {
        input::bind_keys(cx);
        app::bind_keys(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let init = Init { networks, servers, theme, font_family, font_size, banner, commands, events };
        cx.open_window(
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
        cx.activate(true);
    });
}
