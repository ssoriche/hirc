//! Servers and their buffers. The server row shows connection state; the
//! rest are channels then queries, alphabetically, with unread counts.

use crate::app::HircApp;
use gpui::{Context, IntoElement, SharedString, div, prelude::*, px};
use hirc_core::event::{ConnStatus, Target};

pub fn render(app: &HircApp, cx: &mut Context<HircApp>) -> impl IntoElement {
    let p = &app.palette;
    let mut col =
        div().id("sidebar").flex().flex_col().w(px(220.)).flex_none().h_full().bg(p.background).border_r_1().border_color(p.border).py_2().overflow_y_scroll();
    for (n, net) in app.store.networks.iter().enumerate() {
        let mut order: Vec<usize> = (1..net.buffers.len()).collect();
        order.sort_by_key(|&b| {
            let buf = &net.buffers[b];
            (matches!(buf.target, Target::Query(_)), buf.target.name().unwrap_or_default().to_ascii_lowercase())
        });
        for b in std::iter::once(0).chain(order) {
            col = col.child(row(app, n, b, cx));
        }
    }
    col
}

fn row(app: &HircApp, n: usize, b: usize, cx: &mut Context<HircApp>) -> impl IntoElement {
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
    div()
        .id(("buffer", (n << 16) | b))
        .flex()
        .gap_2()
        .px_3()
        .py(px(2.))
        .cursor_pointer()
        .text_color(color)
        .when(selected, |d| d.bg(p.selected))
        .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
        .when(b == 0, |d| d.mt_2().child(div().text_color(dot).child("●")))
        .when(b != 0, |d| d.pl_6())
        .child(div().flex_1().truncate().when(dimmed, |d| d.opacity(0.5)).child(label))
        .when(buf.unread > 0 && b != 0, |d| d.child(div().text_color(p.unread).child(buf.unread.to_string())))
        .on_click(cx.listener(move |this, _, _, cx| this.select(n, b, cx)))
}
