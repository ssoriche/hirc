//! The window list: every buffer across all networks in one numbered,
//! user-ordered column (irssi-style). Each row is a grid of four columns,
//! sized from `Metrics` so they line up on every row:
//!
//! | number | label | network | state |
//!
//! The number column fits the widest window number; the network column
//! is one width for every row (`tag_width`), so a network's tag reads the
//! same, cut or not, on every row: as wide as the widest tag, giving way
//! to long labels down to a few cells, and left out of a list too narrow
//! for those (the header names the network); the label takes the rest.
//! The state column, the unread count's, is reserved on every row, and
//! holds every mark a window shows: its unread count, a server row's
//! connection state, a channel we are not in. All text is one size: the label is `text.primary` and the number
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
//! every row of it, its server row included, is dimmed, except the
//! selected row's name and number, which the user is reading (its tag
//! and state mark say it is down), and a mention count, which still
//! matters. Every window of a down network carries the server row's mark
//! in its state column while it has no count. Red marks a fault: a red
//! tag, never dimmed, and a red ring while hirc retries, a red dot once
//! it has given up; a network the user quit is offline, not failed: a
//! quiet ring. A
//! channel we are not in, on a live network, keeps its row and says so
//! the way the header does: a quiet `○` in the state column and its name
//! in `text.secondary`. A server row's state sits in that column too, a
//! dot or a ring, so every window's state reads down one column; its
//! unread lines show in its label's colour (and lines meant for us as the
//! count, which then takes the column). A row with no tag lets its label
//! run up to a mark (one cell), as there is nothing else between them.
//!
//! Rows are one text line apart, the message view's own pitch, so the
//! list and the conversation beside it share one line grid (a terminal's
//! window list); a row is still easy to hit, since rows touch and each
//! takes clicks across the whole width of the list.
//!
//! A list too long for its pane keeps a row at the top while rows are
//! hidden above, and one at the bottom while rows are hidden below; each
//! says how many, with the same columns as a window row (their unread
//! lines, on the mention fill when some are for us, and in the network
//! column, in red, the networks among them down by a fault); a click
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
pub fn row_centre(m: &Metrics, lead: f32, width: f32, height: f32, count: usize, offset: f32, index: usize) -> (f32, f32) {
    (width / 2., list_top(m, lead, overflows(m, lead, height, count)) + m.row * (index as f32 + 0.5) + offset)
}

/// Whether `count` rows overflow a list `height` tall, starting `lead`
/// further down than the bars' padding.
pub fn overflows(m: &Metrics, lead: f32, height: f32, count: usize) -> bool {
    2. * m.bar_pad() + lead + m.row * count as f32 > height
}

/// Top of the first row: the bars' clear space and `lead`, the part of
/// what is left of a line that the header takes above its text (so the
/// first row's text sits on the header's line), and in a list that
/// overflows, the row that counts what is hidden above.
#[cfg(feature = "snapshot")]
fn list_top(m: &Metrics, lead: f32, overflow: bool) -> f32 {
    m.bar_pad() + lead + if overflow { m.row } else { 0. }
}

/// The opacity of a window that can't be used.
const DIM: f32 = 0.5;
/// The network column's width, the same on every row, or `None` when the
/// tags are left out. `room` is what the label and tag share; the widest
/// label and tag are in px. The tag is as wide as the widest tag (up to
/// `tag_max`); long labels take it down to `tag_min`, enough to tell
/// networks apart. In a list too narrow for that beside a label's
/// minimum, the tags go and the labels take their cells: a letter of a
/// tag (`d…`) tells networks apart only by chance, and would cut both
/// fields on one row. The header and the server rows still name each
/// window's network.
fn tag_width(m: &Metrics, room: f32, widest_label: f32, widest_tag: f32) -> Option<f32> {
    let most = widest_tag.min(m.tag_max());
    let least = m.tag_min().min(most);
    let spare = room - m.label_min();
    (spare >= least && least > 0.).then(|| (room - widest_label).clamp(least, most).min(spare))
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
    let lead = app.bar_extra.0;
    let overflow = overflows(m, lead, height, rows.len());
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
    let shown = whole_rows(height - 2. * m.bar_pad() - lead - slots as f32 * m.row, m.row);
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
        .pt(px(m.bar_pad() + lead))
        .pb(px(m.bar_pad()))
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
    /// Whether tags fit at all.
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

/// What the state column holds.
enum Count {
    None,
    Lines(usize),
    /// A window's state: a mark in its colour (a server row's dot or
    /// ring, a channel we are not in's quiet `○`), and whether it is a
    /// fault (never dimmed).
    Mark(&'static str, gpui::Hsla, bool),
}

/// The unread column: `count` lines, in reverse video when some are
/// meant for us (never dimmed: a mention still matters on a network that
/// is down). `narrow`: a mark takes one cell, and the label the rest.
fn count_cell(app: &HircApp, count: Count, mention: bool, dimmed: bool, narrow: bool) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    // The digits stay on the column's right edge; the fill runs half a
    // cell clear of them, the glyph band tall.
    let half = count_inset(m);
    let count = match count {
        Count::None => None,
        Count::Lines(c) => Some(c),
        Count::Mark(mark, color, fault) => {
            return div()
                .flex_none()
                .flex()
                .justify_end()
                .w(px(if narrow { m.cells(1.) } else { m.count_width() }))
                .mr(px(half))
                .text_color(color)
                .when(dimmed && !fault, |d| d.opacity(DIM))
                .child(mark);
        }
    };
    div().flex_none().flex().justify_end().w(px(m.count_width())).mr(px(half)).when(dimmed && !mention, |d| d.opacity(DIM)).children(count.map(|c| {
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
    // Dimmed: the network is down, selected or not (the fill still says
    // which row is selected).
    let dimmed = net.status.is_down();
    let parted = !net.status.is_down() && matches!(buf.target, Target::Channel(_)) && !buf.joined;
    let label_color = match () {
        _ if buf.unread > 0 && !selected => p.unread,
        _ if parted && !selected => p.text_dim,
        _ if selected => p.selected_text,
        _ => p.text,
    };
    // The network tag, red for a fault; on the selected fill, its own
    // role still, a step below the name.
    let tag_color = if fault {
        p.error
    } else if selected {
        p.selected_tag
    } else {
        p.tag
    };
    // The network's state, as its server row marks it.
    let state = match net.status {
        ConnStatus::Connected { .. } => Count::Mark("●", p.success, false),
        ConnStatus::Connecting | ConnStatus::Idle => Count::Mark("●", p.text_dim, false),
        ConnStatus::Disconnected { .. } => Count::Mark("○", p.error, true),
        ConnStatus::Failed { .. } => Count::Mark("●", p.error, true),
        ConnStatus::Offline => Count::Mark("○", p.text_dim, false),
    };
    // The state column: unread lines, else the window's state (a down
    // network's on every window of it).
    let count = match () {
        _ if buf.unread > 0 && !selected && (!is_server || buf.mentions > 0) => Count::Lines(buf.unread),
        _ if is_server || dimmed => state,
        _ if parted => Count::Mark("○", p.text_dim, false),
        _ => Count::None,
    };
    let tagged = !is_server && cols.tags;
    let narrow = !tagged && matches!(count, Count::Mark(..));
    // The selected row's name and number are what the user is reading:
    // never dimmed.
    let faded = dimmed && !selected;
    grid(m, ("window", pos))
        .when(selected, |d| d.bg(p.selected))
        .when(!selected, |d| d.hover(|s| s.bg(p.hover)))
        .child(
            div()
                .flex_none()
                .w(px(cols.number))
                .text_right()
                .text_color(if selected { p.selected_text } else { p.text_dim })
                .when(faded, |d| d.opacity(DIM))
                .child((pos + 1).to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(label_color)
                .when(selected, |d| d.font_weight(FontWeight::BOLD))
                .when(faded, |d| d.opacity(DIM))
                .child(label),
        )
        // A server row's label is the network, so it takes the tag's
        // column; with the tags left out, a window's label does too.
        .when(tagged, |d| {
            d.child(
                tag_cell(cols, net.name.clone().into())
                    .text_color(tag_color)
                    // A fault stays red; otherwise the tag dims with its row.
                    .when(dimmed && !fault, |d| d.opacity(DIM)),
            )
        })
        .child(count_cell(app, count, buf.mentions > 0, dimmed, narrow))
        .on_click(cx.listener(move |this, _, _, cx| this.select(n, b, cx)))
}

/// The row at one end of a list that overflows, for the `windows` hidden
/// past it: `arrow`, how many, the networks among them down by a fault
/// (named in red in the network column, as their rows' tags are), and
/// their unread lines. A click scrolls row `reveal` into view.
fn edge(app: &HircApp, cols: &Columns, arrow: &'static str, windows: &[(usize, usize)], reveal: usize, cx: &mut Context<HircApp>) -> AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let nets = &app.store.networks;
    // The networks among them down by a fault, each once, in order.
    let mut faults: Vec<&str> = Vec::new();
    for &(n, _) in windows {
        if nets[n].status.is_fault() && !faults.contains(&nets[n].name.as_str()) {
            faults.push(&nets[n].name);
        }
    }
    let unread: usize = windows.iter().map(|&(n, b)| nets[n].buffers[b].unread).sum();
    let mention = windows.iter().any(|&(n, b)| nets[n].buffers[b].mentions > 0);
    let faulted = !faults.is_empty();
    let faults = faults.join(", ");
    // Unread lines; with no network column to name a fault in, the
    // state column's red ring, as a faulted window's own row shows it.
    let count = match () {
        _ if unread > 0 => Count::Lines(unread),
        _ if faulted && !cols.tags => Count::Mark("○", p.error, true),
        _ => Count::None,
    };
    grid(m, ("edge", reveal))
        .hover(|s| s.bg(p.hover))
        .text_color(p.text_dim)
        .child(div().flex_none().w(px(cols.number)).text_right().child(arrow))
        .child(div().flex_1().min_w(px(0.)).truncate().child(format!("{} more", windows.len())))
        // The network column: the faulted networks, red as their tags
        // are (with no tags, they stay out of the row, as tags do).
        .when(cols.tags, |d| d.child(tag_cell(cols, faults.into()).text_color(p.error)))
        .child(count_cell(app, count, mention, false, false))
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
        assert_eq!(row_centre(&m, 0., 240., 800., 6, 0., 0), (120., m.bar_pad() + m.row / 2.));
        assert_eq!(row_centre(&m, 0., 240., 800., 6, 0., 3).1 - row_centre(&m, 0., 240., 800., 6, 0., 2).1, m.row);
        // Scrolled down by two rows, row 2 sits where row 0 was.
        assert_eq!(row_centre(&m, 0., 240., 800., 6, -2. * m.row, 2), row_centre(&m, 0., 240., 800., 6, 0., 0));
        // A list that overflows starts below the row counting what is above.
        assert_eq!(row_centre(&m, 0., 240., 300., 28, 0., 0).1, m.bar_pad() + m.row + m.row / 2.);
        // The header's share of what is left of a line moves every row.
        assert_eq!(row_centre(&m, 1.5, 240., 800., 6, 0., 0).1, m.bar_pad() + 1.5 + m.row / 2.);
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
        // A list too narrow for the floor beside a label's minimum: no
        // tags (never a letter and an ellipsis), whatever the labels.
        let narrow = m.label_min() + m.tag_min() - 1.;
        assert_eq!(super::tag_width(&m, narrow, m.cells(12.), m.cells(8.)), None);
        assert_eq!(super::tag_width(&m, narrow, m.cells(3.), m.cells(8.)), None);
        // Just room for the floor: tags, at the floor.
        assert_eq!(super::tag_width(&m, m.label_min() + m.tag_min(), m.cells(12.), m.cells(8.)), Some(m.tag_min()));
        // The list's narrow floor (the review's 640 window) keeps tags.
        let floor = m.sidebar_width(640.) - 2. * m.gutter - m.number_width(9) - m.count_width() - super::count_inset(&m) - 3. * m.gap;
        assert!(super::tag_width(&m, floor, m.cells(8.), m.cells(5.)).is_some());
        // A short tag needs only its own width.
        assert_eq!(super::tag_width(&m, m.label_min() + m.cells(4.), m.cells(12.), m.cells(4.)), Some(m.cells(4.)));
    }

    #[test]
    fn a_list_overflows_when_its_rows_and_edges_do_not_fit() {
        let m = crate::metrics::Metrics::new(14., crate::metrics::Shape::FALLBACK);
        let fits = 2. * m.bar_pad() + 6. * m.row;
        assert!(!super::overflows(&m, 0., fits, 6));
        assert!(super::overflows(&m, 0., fits - 1., 6));
        assert!(super::overflows(&m, 1., fits, 6));
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
