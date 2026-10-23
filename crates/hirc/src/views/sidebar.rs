//! The window list: every buffer across all networks in one numbered,
//! user-ordered column (irssi-style). Each row shows its window number, the
//! label, the network as a dim tag and the unread count.

use crate::app::HircApp;
use gpui::{Context, IntoElement, SharedString, div, prelude::*, px};
use hirc_core::event::{ConnStatus, Target};

pub fn render(app: &HircApp, cx: &mut Context<HircApp>) -> impl IntoElement {
    let p = &app.palette;
    let mut col =
        div().id("sidebar").flex().flex_col().w(px(220.)).flex_none().h_full().bg(p.background).border_r_1().border_color(p.border).py_2().overflow_y_scroll();
    for pos in 0..app.layout.windows.len() {
        if let Some((n, b)) = app.layout.resolve(&app.store, pos) {
            col = col.child(row(app, pos, n, b, cx));
        }
    }
    col
}

fn row(app: &HircApp, pos: usize, n: usize, b: usize, cx: &mut Context<HircApp>) -> impl IntoElement {
    let p = &app.palette;
    let net = &app.store.networks[n];
    let buf = &net.buffers[b];
    let selected = app.selected == Some((n, b));
    let (label, color): (SharedString, _) = match &buf.target {
        Target::Server => (net.name.clone().into(), p.text),
        t => (t.name().unwrap_or_default().to_owned().into(), if buf.unread > 0 { p.unread } else { p.text_dim }),
    };
    let dot = match net.status {
        ConnStatus::Connected { .. } => p.success,
        ConnStatus::Connecting | ConnStatus::Idle => p.text_dim,
        ConnStatus::Disconnected { .. } | ConnStatus::Failed { .. } => p.error,
    };
    let dimmed = matches!(buf.target, Target::Channel(_)) && !buf.joined;
    let is_server = b == 0;
    div()
        .id(("window", pos))
        .flex()
        .gap_2()
        .px_3()
        .py(px(2.))
        .cursor_pointer()
        .text_color(color)
        .when(selected, |d| d.bg(p.selected))
        .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
        .child(div().w(px(18.)).flex_none().text_right().text_color(p.text_dim).child((pos + 1).to_string()))
        .when(is_server, |d| d.child(div().text_color(dot).child("●")))
        .child(div().flex_1().truncate().when(dimmed, |d| d.opacity(0.5)).child(label))
        .when(!is_server, |d| d.child(div().flex_none().text_xs().text_color(p.text_dim).child(SharedString::from(net.name.clone()))))
        .when(buf.unread > 0 && !is_server, |d| d.child(div().text_color(p.unread).child(buf.unread.to_string())))
        .on_click(cx.listener(move |this, _, _, cx| this.select(n, b, cx)))
}
