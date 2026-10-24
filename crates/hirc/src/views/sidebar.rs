//! The window list: every buffer across all networks in one numbered,
//! user-ordered column (irssi-style). Each row is a grid of four columns,
//! sized from `Metrics` so they line up on every row:
//!
//! | number | label | network | unread |
//!
//! The number column fits the widest window number; the label takes the rest
//! and keeps a minimum; the network column holds the network tag (or, on a
//! server row, its connection dot) and gives way first; the unread column is
//! reserved on every row so tags share one right edge. The selected row is a
//! fill plus `text.primary`; hover is only a faint wash. A network that is
//! down shows a red dot, red tags and dimmed rows.

use crate::app::HircApp;
use crate::metrics::{Metrics, count_label};
use gpui::{Context, IntoElement, ScrollHandle, SharedString, div, prelude::*, px};
use hirc_core::event::{ConnStatus, Target};

/// Where row `index`'s centre is, in window coordinates, for a list
/// `width` wide scrolled by `offset` (the scroll handle's offset, <= 0).
#[cfg(feature = "snapshot")]
pub fn row_centre(m: &Metrics, width: f32, offset: f32, index: usize) -> (f32, f32) {
    (width / 2., list_top(m) + m.row * (index as f32 + 0.5) + offset)
}

/// Top of the first row: the `edge` clear space (the last px of it is the
/// overflow hairline).
fn list_top(m: &Metrics) -> f32 {
    m.edge
}

pub fn render(app: &HircApp, width: f32, cx: &mut Context<HircApp>) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    let rows: Vec<_> = (0..app.layout.windows.len()).filter_map(|pos| app.layout.resolve(&app.store, pos).map(|(n, b)| (pos, n, b))).collect();
    let number_width = m.number_width(rows.iter().map(|(pos, ..)| pos + 1).max().unwrap_or(1));
    let mut list = div().id("sidebar").flex().flex_col().flex_1().min_h(px(0.)).overflow_y_scroll().track_scroll(&app.sidebar_scroll);
    for &(pos, n, b) in &rows {
        list = list.child(row(app, number_width, pos, n, b, cx));
    }
    // A hairline in `general.border` at an edge with rows hidden past it.
    let (above, below) = hidden(&app.sidebar_scroll);
    let hairline = |on: bool| div().flex_none().h(px(1.)).bg(if on { p.border } else { p.background });
    div()
        .flex()
        .flex_col()
        .w(px(width))
        .flex_none()
        .h_full()
        .bg(p.background)
        .border_r_1()
        .border_color(p.border)
        .py(px(list_top(m) - 1.))
        .child(hairline(above))
        .child(list)
        .child(hairline(below))
}

/// Whether rows are scrolled out of view above and below.
fn hidden(scroll: &ScrollHandle) -> (bool, bool) {
    let (offset, max) = (-f32::from(scroll.offset().y), f32::from(scroll.max_offset().y));
    (offset > 0.5, offset < max - 0.5)
}

fn row(app: &HircApp, number_width: f32, pos: usize, n: usize, b: usize, cx: &mut Context<HircApp>) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    let net = &app.store.networks[n];
    let buf = &net.buffers[b];
    let selected = app.selected == Some((n, b));
    let is_server = matches!(buf.target, Target::Server);
    let label: SharedString = match &buf.target {
        Target::Server => net.name.clone().into(),
        t => t.name().unwrap_or_default().to_owned().into(),
    };
    let label_color = match () {
        _ if selected => p.text,
        _ if buf.unread > 0 => p.unread,
        _ => p.text_dim,
    };
    let offline = matches!(net.status, ConnStatus::Disconnected { .. } | ConnStatus::Failed { .. });
    let dimmed = !is_server && (offline || matches!(buf.target, Target::Channel(_)) && !buf.joined);
    // The network column: a server row's connection dot, else the network tag.
    let (network, network_color): (SharedString, _) = if is_server {
        let dot = match net.status {
            ConnStatus::Connected { .. } => p.success,
            ConnStatus::Connecting | ConnStatus::Idle => p.text_dim,
            ConnStatus::Disconnected { .. } | ConnStatus::Failed { .. } => p.error,
        };
        ("●".into(), dot)
    } else {
        (net.name.clone().into(), if offline { p.error } else { p.text_dim })
    };
    let count = (buf.unread > 0 && !selected).then(|| count_label(buf.unread));
    div()
        .id(("window", pos))
        .flex()
        .flex_none()
        .items_start()
        .h(px(m.row))
        .px(px(m.gutter))
        .py(px(m.pad_y))
        .gap(px(m.gap))
        .cursor_pointer()
        .when(selected, |d| d.bg(p.selected))
        .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
        .child(div().flex_none().w(px(number_width)).text_right().text_color(if selected { p.text } else { p.text_dim }).child((pos + 1).to_string()))
        .child(div().flex_1().min_w(px(m.label_min())).truncate().text_color(label_color).when(dimmed, |d| d.opacity(0.5)).child(label))
        .child(
            div()
                .min_w(px(0.))
                .max_w(px(m.tag_max()))
                .truncate()
                .text_size(px(m.small))
                .relative()
                .top(px(m.small_drop))
                .text_color(network_color)
                .child(network),
        )
        .child(div().flex_none().w(px(m.count_width())).text_right().text_color(p.unread).children(count))
        .on_click(cx.listener(move |this, _, _, cx| this.select(n, b, cx)))
}

#[cfg(all(test, feature = "snapshot"))]
mod tests {
    use super::*;
    use crate::metrics::Shape;

    #[test]
    fn row_centres_follow_the_row_pitch_and_scroll() {
        let m = Metrics::new(14., Shape::FALLBACK);
        assert_eq!(row_centre(&m, 240., 0., 0), (120., m.edge + m.row / 2.));
        assert_eq!(row_centre(&m, 240., 0., 3).1 - row_centre(&m, 240., 0., 2).1, m.row);
        // Scrolled down by two rows, row 2 sits where row 0 was.
        assert_eq!(row_centre(&m, 240., -2. * m.row, 2), row_centre(&m, 240., 0., 0));
    }
}
