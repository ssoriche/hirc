//! The window list: every buffer across all networks in one numbered,
//! user-ordered column (irssi-style). Each row is a grid of four columns,
//! sized from `Metrics` so they line up on every row:
//!
//! | number | label | network | unread |
//!
//! The number column fits the widest window number; the network column
//! is one width for every row (`tag_width`), so a network's tag reads the
//! same, cut or not, on every row: as wide as the widest tag, giving way
//! to long labels down to a few cells, and to a letter or two in a narrow
//! list; the label takes the rest. The unread column is reserved on every
//! row. All text is one size: the label is `text.primary` and the number
//! and tag, metadata, are `text.secondary`.
//!
//! Activity: unread rows take the unread colour; when any of the unread
//! lines is meant for us (a mention, a query message), the count is drawn
//! in reverse video, the unread colour as its fill (irssi's highlight
//! activity, louder than plain unread). The selected row is a fill (one
//! apart from the mention fill, see `theme::fill_apart`), `text.primary`
//! and a bold label; hover is only a faint wash.
//!
//! Each state has one look. Dimming means only "this network is down":
//! every row of it, its server row included, is dimmed as a whole, except
//! the selected row, which stays legible on its fill and shows the state
//! by its tag and the header. Red marks a fault: a red tag, never dimmed,
//! and on a server row a red ring while hirc retries, a red dot once it
//! has given up; a network the user quit is offline, not failed: a quiet
//! ring. A channel we are not in, on a live network, keeps its row and
//! says so the way the header does: a quiet `○` in the unread column and
//! its name in `text.secondary`.
//!
//! Rows are one text line apart, the message view's own pitch, so the
//! list and the conversation beside it share one line grid (a terminal's
//! window list); a row is still easy to hit, since rows touch and each
//! takes clicks across the whole width of the list.
//!
//! A list too long for its pane keeps a row at the top while rows are
//! hidden above, and one at the bottom while rows are hidden below; each
//! says how many, with the same columns as a window row (their unread
//! lines, on the mention fill when some are for us, and the server row's
//! fault mark when a network among them is down by a fault); a click
//! scrolls them in. Scrolled to the end, the bottom row goes and the list
//! takes its place.

use crate::app::HircApp;
use crate::metrics::{Metrics, count_label};
use gpui::{AnyElement, Context, FontWeight, IntoElement, ScrollHandle, SharedString, div, prelude::*, px};
use hirc_core::event::{ConnStatus, Target};

/// Where row `index`'s centre is, in window coordinates, for a list
/// `width` by `height` of `count` rows, scrolled by `offset` (the scroll
/// handle's offset, <= 0).
#[cfg(feature = "snapshot")]
pub fn row_centre(m: &Metrics, width: f32, height: f32, count: usize, offset: f32, index: usize) -> (f32, f32) {
    (width / 2., list_top(m, overflows(m, height, count)) + m.row * (index as f32 + 0.5) + offset)
}

/// Whether `count` rows overflow a list `height` tall.
pub fn overflows(m: &Metrics, height: f32, count: usize) -> bool {
    2. * m.bar_pad() + m.row * count as f32 > height
}

/// Top of the first row: the bars' clear space (so the first row's text
/// sits on the header's line), and in a list that overflows, the row that
/// counts what is hidden above.
#[cfg(feature = "snapshot")]
fn list_top(m: &Metrics, overflow: bool) -> f32 {
    m.bar_pad() + if overflow { m.row } else { 0. }
}

/// The opacity of a window that can't be used.
const DIM: f32 = 0.5;
/// The least a tag keeps before it is left out: a letter and an ellipsis.
const TAG_LEAST_CELLS: f32 = 2.;

/// The network column's width, the same on every row, or `None` when
/// even a letter of a tag does not fit. `room` is what the label and tag
/// share; the widest label and tag are in px. The tag is as wide as the
/// widest tag (up to `tag_max`); long labels take it down to `tag_min`.
/// In a list too narrow for that beside a label's minimum, labels come
/// first and the tag keeps a letter and an ellipsis (`d…`), so every row
/// still names its network.
fn tag_width(m: &Metrics, room: f32, widest_label: f32, widest_tag: f32) -> Option<f32> {
    let most = widest_tag.min(m.tag_max());
    let least = m.tag_min().min(most);
    let letter = m.cells(TAG_LEAST_CELLS).min(most);
    let spare = room - m.label_min();
    let width = if spare >= least { (room - widest_label).clamp(least, most).min(spare) } else { (room - widest_label).clamp(letter, most).min(spare) };
    (width >= letter && letter > 0.).then_some(width)
}

pub fn render(app: &HircApp, width: f32, height: f32, cx: &mut Context<HircApp>) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    let rows: Vec<_> = (0..app.layout.windows.len()).filter_map(|pos| app.layout.resolve(&app.store, pos).map(|(n, b)| (pos, n, b))).collect();
    let number_width = m.number_width(rows.iter().map(|(pos, ..)| pos + 1).max().unwrap_or(1));
    // Text widths in cells: the list is set on the monospace grid.
    let cells = |s: &str| m.cells(s.chars().count() as f32);
    let (mut widest_label, mut widest_tag) = (0f32, 0f32);
    // Server rows have no tag: their label reaches the dot.
    for &(_, n, b) in &rows {
        let net = &app.store.networks[n];
        if let Some(name) = net.buffers[b].target.name() {
            widest_label = widest_label.max(cells(name));
            widest_tag = widest_tag.max(cells(&net.name));
        }
    }
    let room = width - 2. * m.gutter - number_width - m.count_width() - count_inset(m) - 3. * m.gap;
    let tags = tag_width(m, room, widest_label, widest_tag);
    let cols = Columns { number: number_width, tag: tags.unwrap_or(m.cell), tags: tags.is_some() };
    let mut list = div().id("sidebar").flex().flex_col().flex_1().min_h(px(0.)).overflow_y_scroll().track_scroll(&app.sidebar_scroll);
    for &(pos, n, b) in &rows {
        list = list.child(row(app, &cols, pos, n, b, cx));
    }
    let overflow = overflows(m, height, rows.len());
    let (above, below) = if overflow { hidden(&app.sidebar_scroll, m.row, rows.len()) } else { (None, None) };
    let windows = |range: std::ops::Range<usize>| rows[range].iter().map(|&(_, n, b)| (n, b)).collect::<Vec<_>>();
    let top = above.map(|k| edge(app, &cols, "↑", &windows(0..k), k - 1, cx));
    let bottom = below.map(|k| edge(app, &cols, "↓", &windows(rows.len() - k..rows.len()), rows.len() - k, cx));
    // The top row is kept while the list overflows, so the rows never
    // shift under the pointer as it comes and goes; the bottom one only
    // while rows are below (going, it gives the list a row at its end).
    let slot = |e: Option<AnyElement>| div().flex_none().h(px(m.row)).children(e);
    let bottom_slot = bottom.is_some();
    // An overflowing list shows whole rows only (what is left of a row
    // goes below the last slot), so it scrolls by rows: no row is ever
    // cut at an end, and the end rows count only rows out of view.
    let slots = 1 + usize::from(bottom_slot);
    let shown = whole_rows(height - 2. * m.bar_pad() - slots as f32 * m.row, m.row);
    let list = list.when(overflow, |d| d.flex_none().h(px(shown)));
    div()
        .flex()
        .flex_col()
        .w(px(width))
        .flex_none()
        .h_full()
        .bg(p.background)
        .border_r_1()
        .border_color(p.border)
        .py(px(m.bar_pad()))
        .when(overflow, |d| d.child(slot(top)))
        .child(list)
        .when(bottom_slot, |d| d.child(slot(bottom)))
}

/// The height of the whole rows, `row` px each, that fit in `room`.
fn whole_rows(room: f32, row: f32) -> f32 {
    if row <= 0. { 0. } else { (room / row).floor().max(1.) * row }
}

/// The columns every row shares.
struct Columns {
    number: f32,
    tag: f32,
    /// Whether tags fit at all (else the column holds only server dots).
    tags: bool,
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

/// A row's grid: the four columns in their places, a row tall.
fn grid(m: &Metrics, id: (&'static str, usize)) -> gpui::Stateful<gpui::Div> {
    div().id(id).flex().flex_none().items_start().h(px(m.row)).px(px(m.gutter)).gap(px(m.gap)).cursor_pointer()
}

/// How far the unread column keeps in from the gutter: the mention
/// fill's half cell beyond the digits, so the fill ends on the gutter
/// like everything else in the list, and plain digits share its column.
fn count_inset(m: &Metrics) -> f32 {
    (m.cell / 2.).round()
}

/// What the unread column holds.
enum Count {
    None,
    Lines(usize),
    /// A channel we are not in: the header's quiet `○`.
    NotJoined,
}

/// The unread column: `count` lines, in reverse video when some are
/// meant for us.
fn count_cell(app: &HircApp, count: Count, mention: bool, dimmed: bool) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    // The digits stay on the column's right edge; the fill runs half a
    // cell clear of them, the glyph band tall.
    let half = count_inset(m);
    let count = match count {
        Count::None => None,
        Count::Lines(c) => Some(c),
        Count::NotJoined => {
            return div().flex_none().flex().justify_end().w(px(m.count_width())).mr(px(half)).text_color(p.text_dim).child("○");
        }
    };
    div().flex_none().flex().justify_end().w(px(m.count_width())).mr(px(half)).when(dimmed, |d| d.opacity(DIM)).children(count.map(|c| {
        div()
            .when(!mention, |d| d.text_color(p.unread))
            .when(mention, |d| {
                d.mt(px(m.glyph_top))
                    .h(px(m.glyph))
                    .line_height(px(m.glyph))
                    .px(px(half))
                    .mr(px(-half))
                    .bg(p.unread)
                    .text_color(p.background)
                    .font_weight(FontWeight::BOLD)
            })
            .child(count_label(c))
    }))
}

/// The network column: `text`, right-aligned and cut to the column.
fn tag_cell(cols: &Columns, text: SharedString) -> gpui::Div {
    div().flex_none().w(px(cols.tag)).truncate().text_right().child(text)
}

fn row(app: &HircApp, cols: &Columns, pos: usize, n: usize, b: usize, cx: &mut Context<HircApp>) -> impl IntoElement {
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
    let fault = net.status.is_fault();
    // Dimmed: the network is down. The selected row stays legible.
    let dimmed = net.status.is_down() && !selected;
    let parted = !net.status.is_down() && matches!(buf.target, Target::Channel(_)) && !buf.joined;
    let label_color = match () {
        _ if buf.unread > 0 && !selected => p.unread,
        _ if parted && !selected => p.text_dim,
        _ => p.text,
    };
    // The network column: a server row's connection state, else the
    // network tag.
    let (network, network_color): (SharedString, _) = if is_server {
        match net.status {
            ConnStatus::Connected { .. } => ("●".into(), p.success),
            ConnStatus::Connecting | ConnStatus::Idle => ("●".into(), p.text_dim),
            ConnStatus::Disconnected { .. } => ("○".into(), p.error),
            ConnStatus::Failed { .. } => ("●".into(), p.error),
            ConnStatus::Offline => ("○".into(), p.text_dim),
        }
    } else {
        (
            net.name.clone().into(),
            if fault {
                p.error
            } else if selected && !net.status.is_down() {
                p.text
            } else {
                p.text_dim
            },
        )
    };
    let count = match () {
        _ if buf.unread > 0 && !selected => Count::Lines(buf.unread),
        _ if parted => Count::NotJoined,
        _ => Count::None,
    };
    grid(m, ("window", pos))
        .when(selected, |d| d.bg(p.selected))
        .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
        .child(
            div()
                .flex_none()
                .w(px(cols.number))
                .text_right()
                .text_color(if selected { p.text } else { p.text_dim })
                .when(dimmed, |d| d.opacity(DIM))
                .child((pos + 1).to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(label_color)
                .when(selected, |d| d.font_weight(FontWeight::BOLD))
                .when(dimmed, |d| d.opacity(DIM))
                .child(label),
        )
        .child(
            // A server row's dot keeps the column's right edge, and its
            // label (the network) has the rest.
            tag_cell(cols, if is_server || cols.tags { network } else { SharedString::default() })
                .when(is_server, |d| d.w(px(m.cell.ceil())))
                .text_color(network_color)
                // A fault stays red; otherwise the tag dims with its row.
                .when(dimmed && !fault, |d| d.opacity(DIM)),
        )
        .child(count_cell(app, count, buf.mentions > 0, dimmed))
        .on_click(cx.listener(move |this, _, _, cx| this.select(n, b, cx)))
}

/// The row at one end of a list that overflows, for the `windows` hidden
/// past it: `arrow`, how many, the network of a fault among them, and
/// their unread lines. A click scrolls row `reveal` into view.
fn edge(app: &HircApp, cols: &Columns, arrow: &'static str, windows: &[(usize, usize)], reveal: usize, cx: &mut Context<HircApp>) -> AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let nets = &app.store.networks;
    // A fault among them, as the server row marks it: a ring while hirc
    // retries, a dot once it has given up.
    let fault = windows.iter().map(|&(n, _)| &nets[n].status).find(|s| s.is_fault()).map(|s| match s {
        ConnStatus::Disconnected { .. } => "○",
        _ => "●",
    });
    let unread: usize = windows.iter().map(|&(n, b)| nets[n].buffers[b].unread).sum();
    let mention = windows.iter().any(|&(n, b)| nets[n].buffers[b].mentions > 0);
    grid(m, ("edge", reveal))
        .hover(|s| s.bg(p.hover))
        .text_color(p.text_dim)
        .child(div().flex_none().w(px(cols.number)).text_right().child(arrow))
        .child(div().flex_1().min_w(px(0.)).truncate().child(format!("{} more", windows.len())))
        .child(tag_cell(cols, fault.unwrap_or_default().into()).w(px(m.cell.ceil())).text_color(p.error))
        .child(count_cell(app, if unread > 0 { Count::Lines(unread) } else { Count::None }, mention, false))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.sidebar_scroll.scroll_to_item(reveal);
            cx.notify();
        }))
        .into_any_element()
}

#[cfg(all(test, feature = "snapshot"))]
mod tests {
    use super::*;
    use crate::metrics::Shape;

    #[test]
    fn row_centres_follow_the_row_pitch_and_scroll() {
        let m = Metrics::new(14., Shape::FALLBACK);
        assert_eq!(row_centre(&m, 240., 800., 6, 0., 0), (120., m.bar_pad() + m.row / 2.));
        assert_eq!(row_centre(&m, 240., 800., 6, 0., 3).1 - row_centre(&m, 240., 800., 6, 0., 2).1, m.row);
        // Scrolled down by two rows, row 2 sits where row 0 was.
        assert_eq!(row_centre(&m, 240., 800., 6, -2. * m.row, 2), row_centre(&m, 240., 800., 6, 0., 0));
        // A list that overflows starts below the row counting what is above.
        assert_eq!(row_centre(&m, 240., 300., 28, 0., 0).1, m.bar_pad() + m.row + m.row / 2.);
    }
}

#[cfg(test)]
mod hidden_tests {
    use super::hidden_rows;

    #[test]
    fn the_tag_column_gives_way_to_labels_down_to_its_floor() {
        let m = crate::metrics::Metrics::new(14., crate::metrics::Shape::FALLBACK);
        let tag = |room: f32, label: f32, tag: f32| super::tag_width(&m, m.cells(room), m.cells(label), m.cells(tag));
        // Room for both: the widest tag, whole.
        assert_eq!(tag(20., 8., 5.), Some(m.cells(5.)));
        // A tag never takes more than its most.
        assert_eq!(tag(40., 8., 20.), Some(m.tag_max()));
        // A long label takes the tag down to its floor, no further.
        assert_eq!(tag(20., 30., 20.), Some(m.tag_min()));
        // A narrow list: a letter and an ellipsis, the label the rest.
        let narrow = m.label_min() + m.cells(3.);
        assert_eq!(super::tag_width(&m, narrow, m.cells(12.), m.cells(8.)), Some(m.cells(2.)));
        // Short labels leave the tag more.
        assert_eq!(super::tag_width(&m, narrow, m.cells(6.), m.cells(8.)), Some(m.cells(3.)));
        // Not even a letter and an ellipsis: no tags.
        assert_eq!(super::tag_width(&m, m.label_min() + m.cells(1.), m.cells(12.), m.cells(8.)), None);
    }

    #[test]
    fn a_list_overflows_when_its_rows_and_edges_do_not_fit() {
        let m = crate::metrics::Metrics::new(14., crate::metrics::Shape::FALLBACK);
        let fits = 2. * m.bar_pad() + 6. * m.row;
        assert!(!super::overflows(&m, fits, 6));
        assert!(super::overflows(&m, fits - 1., 6));
    }

    #[test]
    fn an_overflowing_list_shows_whole_rows() {
        assert_eq!(super::whole_rows(438., 21.), 420.);
        assert_eq!(super::whole_rows(420., 21.), 420.);
        assert_eq!(super::whole_rows(5., 21.), 21.);
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
