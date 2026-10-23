use gpui::{App, Application, Bounds, Context, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb, size};

/// Placeholder until the real app lands: three panes in roughly the
/// colours of the default theme, enough to check the window manager.
struct Shell;

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .bg(rgb(0x11111b))
            .text_color(rgb(0xcdd6f4))
            .child(div().w(px(220.)).h_full().border_r_1().border_color(rgb(0x6c7086)).p_2().child("servers"))
            .child(div().flex_1().h_full().bg(rgb(0x181825)).p_2().child("buffer"))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("hirc".into()), ..Default::default() }),
                app_id: Some("hirc".into()),
                window_min_size: Some(size(px(600.), px(400.))),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Shell),
        )
        .expect("open window");
        cx.activate(true);
    });
}
