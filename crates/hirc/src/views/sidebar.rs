//! The window list: every buffer across all networks in one numbered,
//! user-ordered column (irssi-style). Each row is a grid of four columns,
//! sized from `Metrics` so they line up on every row:
//!
//! | number | label | network | unread |
//!
//! The number column fits the widest window number; the label takes the rest
//! and keeps a minimum; the network column holds the network tag (or, on a
//! server row, its connection dot) and gives way first, down to a few
//! cells, before the label is cut; the unread column is reserved on every
//! row so tags share one right edge. All text is one size: the label is
//! `text.primary` and the number and tag, metadata, are `text.secondary`.
//! Activity: unread rows take the unread colour; when any of the unread
//! lines is meant for us (a mention, a query message), the count sits on
//! the `buffer.highlight` fill that marks a mention in the message view
//! (irssi's highlight activity). In a list too narrow for a readable tag,
//! tags are left out (the title bar names the window's network). The selected row is a fill, `text.primary`
//! throughout and a bold label; hover is only a faint wash. A window that
//! can't be used (network down, channel left) is dimmed as a whole row
//! (number, label, tag and count), except when selected, where the label
//! stays readable and the tag or dot carries the state. Red marks a fault
//! (a dropped or failed connection): a red dot and red tags, never dimmed.
//! A network the user quit is offline, not failed: a hollow dot and quiet
//! tags. Rows scrolled out of view leave a hairline at that edge, in the
//! colour of the loudest state they hide (error, unread, else the border).

use crate::app::HircApp;
use crate::metrics::{Metrics, count_label};
use gpui::{Context, FontWeight, Hsla, IntoElement, ScrollHandle, SharedString, div, prelude::*, px};
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
        list = list.child(row(app, width, number_width, pos, n, b, cx));
    }
    // A hairline at an edge with rows hidden past it, in the colour of
    // the loudest state among them.
    let (above, below) = hidden(&app.sidebar_scroll, m.row, rows.len());
    let loudest = |range: std::ops::Range<usize>| {
        let states = rows[range].iter().map(|&(_, n, b)| {
            let net = &app.store.networks[n];
            (net.status.is_fault(), net.buffers[b].unread > 0)
        });
        let (fault, unread) = states.fold((false, false), |(f, u), (a, b)| (f || a, u || b));
        match () {
            _ if fault => p.error,
            _ if unread => p.unread,
            _ => p.border,
        }
    };
    let hairline = |color: Option<Hsla>| div().flex_none().h(px(1.)).bg(color.unwrap_or(p.background));
    let (above, below) = (above.map(|n| loudest(0..n)), below.map(|n| loudest(rows.len() - n..rows.len())));
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

/// How many of `count` rows, `row` px each, are wholly or partly out of
/// view above and below (`None` when none are).
fn hidden(scroll: &ScrollHandle, row: f32, count: usize) -> (Option<usize>, Option<usize>) {
    let (offset, max) = (-f32::from(scroll.offset().y), f32::from(scroll.max_offset().y));
    let view = f32::from(scroll.bounds().size.height);
    hidden_rows(offset, max, view, row, count)
}

/// `hidden`, on plain numbers: the list is scrolled `offset` of `max`, and
/// shows `view` px.
fn hidden_rows(offset: f32, max: f32, view: f32, row: f32, count: usize) -> (Option<usize>, Option<usize>) {
    if row <= 0. {
        return (None, None);
    }
    let above = (offset > 0.5).then(|| (((offset - 0.5) / row).ceil() as usize).min(count));
    let below = (offset < max - 0.5).then(|| count.saturating_sub(((offset + view + 0.5) / row).floor() as usize));
    (above, below.filter(|n| *n > 0).or((offset < max - 0.5).then_some(1)))
}

/// The opacity of a window that can't be used.
const DIM: f32 = 0.5;
/// The least a tag keeps in a list `width` wide: `tag_min`, or what is
/// left beside the other columns at their least, so a row never overflows.
fn tag_floor(m: &Metrics, width: f32, number_width: f32) -> f32 {
    let rest = width - 2. * m.gutter - number_width - m.label_min() - m.count_width() - 3. * m.gap;
    rest.clamp(0., m.tag_min())
}

/// How much faster the network tag shrinks than the label.
const TAG_SHRINK: f32 = 64.;

fn row(app: &HircApp, width: f32, number_width: f32, pos: usize, n: usize, b: usize, cx: &mut Context<HircApp>) -> impl IntoElement {
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
    let label_color = if buf.unread > 0 && !selected { p.unread } else { p.text };
    let fault = net.status.is_fault();
    let unusable = !is_server && (net.status.is_down() || matches!(buf.target, Target::Channel(_)) && !buf.joined);
    let dimmed = unusable && !selected;
    // The network column: a server row's connection dot, else the network tag.
    let floor = tag_floor(m, width, number_width);
    let (network, network_color): (SharedString, _) = if is_server {
        match net.status {
            ConnStatus::Connected { .. } => ("●".into(), p.success),
            ConnStatus::Connecting | ConnStatus::Idle => ("●".into(), p.text_dim),
            ConnStatus::Disconnected { .. } | ConnStatus::Failed { .. } => ("●".into(), p.error),
            ConnStatus::Offline => ("○".into(), p.text_dim),
        }
    } else {
        (
            net.name.clone().into(),
            if fault {
                p.error
            } else if selected {
                p.text
            } else {
                p.text_dim
            },
        )
    };
    let count = (buf.unread > 0 && !selected).then(|| count_label(buf.unread));
    // Lines meant for us among them: the count on the mention fill, its
    // digits on the column's right edge and the fill half a cell clear.
    let mention = buf.mentions > 0;
    let half = (m.cell / 2.).round();
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
        .child(
            div()
                .flex_none()
                .w(px(number_width))
                .text_right()
                .text_color(if selected { p.text } else { p.text_dim })
                .when(dimmed, |d| d.opacity(DIM))
                .child((pos + 1).to_string()),
        )
        .child(
            div()
                .flex_grow(1.)
                .flex_shrink(1.)
                .min_w(px(m.label_min()))
                .truncate()
                .text_color(label_color)
                .when(selected, |d| d.font_weight(FontWeight::BOLD))
                .when(dimmed, |d| d.opacity(DIM))
                .child(label),
        )
        .child(
            div()
                // The tag gives way first: it shrinks far faster than the
                // label, down to a few cells.
                .flex_shrink(if is_server { 0. } else { TAG_SHRINK })
                .min_w(px(if is_server { 0. } else { floor }))
                .when(!is_server && floor < m.tag_min(), |d| d.hidden())
                .max_w(px(m.tag_max()))
                .truncate()
                .text_right()
                .text_color(network_color)
                // A fault stays red; otherwise the tag dims with its row.
                .when(dimmed && !fault, |d| d.opacity(DIM))
                .child(network),
        )
        .child(div().flex_none().flex().justify_end().w(px(m.count_width())).when(dimmed, |d| d.opacity(DIM)).children(count.map(|c| {
            div()
                .when(!mention, |d| d.text_color(p.unread))
                .when(mention, |d| {
                    d.mt(px(m.glyph_top))
                        .h(px(m.glyph))
                        .line_height(px(m.glyph))
                        .px(px(half))
                        .mr(px(-half))
                        .bg(p.mention)
                        .text_color(p.text)
                        .font_weight(FontWeight::BOLD)
                })
                .child(c)
        })))
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

#[cfg(test)]
mod hidden_tests {
    use super::hidden_rows;

    #[test]
    fn a_tag_floor_never_overflows_the_row() {
        let m = crate::metrics::Metrics::new(14., crate::metrics::Shape::FALLBACK);
        assert_eq!(super::tag_floor(&m, m.sidebar_width(1200.), m.number_width(9)), m.tag_min());
        let narrow = m.sidebar_width(400.);
        let floor = super::tag_floor(&m, narrow, m.number_width(28));
        assert!(floor < m.tag_min());
        let used = 2. * m.gutter + m.number_width(28) + m.label_min() + floor + m.count_width() + 3. * m.gap;
        assert!(used <= narrow + 1e-3 || floor == 0.);
    }

    #[test]
    fn rows_out_of_view_are_counted_per_edge() {
        // 10 rows of 20 px in a 100 px view.
        assert_eq!(hidden_rows(0., 100., 100., 20., 10), (None, Some(5)));
        assert_eq!(hidden_rows(100., 100., 100., 20., 10), (Some(5), None));
        // Half a row scrolled: that row counts as hidden.
        assert_eq!(hidden_rows(10., 100., 100., 20., 10), (Some(1), Some(5)));
        // Everything fits.
        assert_eq!(hidden_rows(0., 0., 200., 20., 10), (None, None));
    }
}
