//! Every spacing value in the UI, derived from the configured font size and
//! the font's own shape (digit advance, ascent, descent), so the layout keeps
//! its proportions at any font and size. Values are logical px, rounded to
//! whole px where they stack (line, row, paddings), so rows never land on
//! half pixels. Hairline borders (1px) are the only fixed value.
//!
//! The vertical rhythm is one line grid, as in a terminal: every row of
//! text, a message or a window in the list, is one `line` (`row`). The
//! line is an odd number of px, so the header and the input bar, each a
//! line of text with `bar_pad` above and below and a hairline rule, are
//! exactly two lines tall. The window list starts `bar_pad` down, so its
//! first row sits on the header's line, and the message view `bar_pad`
//! under the header, so every list row shares a line with a message row.
//!
//! The horizontal grid: `gutter` is the inset from every pane edge, `gap`
//! separates columns in a row, and `cells(n)` is the width of `n` digits.

/// How the configured font is shaped, per px of font size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    /// Advance of `0`: one cell of the monospace grid.
    pub cell: f32,
    /// Distance from the baseline up to the top of the line's glyphs.
    pub ascent: f32,
    /// Distance from the baseline down to the bottom (positive).
    pub descent: f32,
}

impl Shape {
    /// Typical monospace proportions, for when the font cannot be measured.
    pub const FALLBACK: Shape = Shape { cell: 0.6, ascent: 0.93, descent: 0.24 };
}

/// Line height as a multiple of the font size (to the nearest odd px).
const LINE: f32 = 1.5;
/// The window list's width in cells, and its limits.
const SIDEBAR_CELLS: f32 = 26.;
const SIDEBAR_MIN_CELLS: f32 = 16.;
/// The list's width that keeps network tags at their floor beside a
/// label's minimum (`LABEL_MIN_CELLS` + `TAG_MIN_CELLS`, the number,
/// count and gaps): in a narrow window the messages give up their time
/// column and narrow their nicks before the list loses its tags.
const SIDEBAR_TAGS_CELLS: f32 = 21.;
/// The most of the window the list may take.
const SIDEBAR_MAX_SHARE: f32 = 0.3;
/// Cells the message view keeps before the list takes more than its
/// minimum: the time and nick columns and about 40 cells of text.
const MESSAGE_CELLS: f32 = 60.;
/// Unread counts get this many cells; larger counts are abbreviated.
pub const COUNT_CELLS: usize = 3;
/// A network tag never takes more than this many cells, and gives up
/// cells to the label down to `TAG_MIN_CELLS` (an ellipsis included),
/// enough to tell networks apart.
const TAG_CELLS: f32 = 8.;
const TAG_MIN_CELLS: f32 = 7.;
/// Message lines: the timestamp (`HH:MM`) and the nick column, in cells.
pub const TIME_CELLS: usize = 5;
const NICK_MIN_CELLS: usize = 3;
const NICK_MAX_CELLS: usize = 12;
/// The nick column never takes more than this share of the message view.
const NICK_MAX_SHARE: f32 = 0.2;
/// Cells the text keeps before the nick column gives up its own, down to
/// `NICK_NARROW_CELLS` (most nicks still fit; longer ones overflow).
const TEXT_MIN_CELLS: usize = 40;
const NICK_NARROW_CELLS: usize = 8;
/// With the time column gone, the nick column's own floor.
const NICK_COMPACT_CELLS: usize = 6;
/// The least the messages keep in a narrow window before the list gives
/// up its tags: no time, narrow nicks and `TEXT_MIN_CELLS` of text.
const MESSAGE_MIN_CELLS: f32 = (NICK_NARROW_CELLS + 1 + TEXT_MIN_CELLS) as f32;
/// The text a line keeps beside a nick too long for the column: such a
/// nick shows whole and pushes its own line's text on, up to this.
const OVERFLOW_TEXT_CELLS: usize = 16;

/// A label always keeps at least this many cells before tags give way.
const LABEL_MIN_CELLS: f32 = 6.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// The configured font size: every piece of text uses it, as a
    /// terminal does; hierarchy comes from colour roles and weight.
    pub size: f32,
    /// Width of one digit at `size`.
    pub cell: f32,
    /// Line height for all text: an odd number of px.
    pub line: f32,
    /// One row of the window list: a line, on the message view's grid.
    pub row: f32,
    /// Inset from every pane edge.
    pub gutter: f32,
    /// Space between columns in a row.
    pub gap: f32,
    /// The font's glyph band (ascent + descent, centred in the line box
    /// as text is), widened to whole px: the cursor covers it, so every
    /// glyph under it, descenders included, is inside.
    pub glyph_top: f32,
    pub glyph: f32,
    /// Width of the insert cursor (a bar), and its clearance from the
    /// glyph beside it: a hairline that grows with the font.
    pub caret: f32,
}

impl Metrics {
    pub fn new(size: f32, shape: Shape) -> Metrics {
        let size = if size.is_finite() && size > 0. { size } else { 14. };
        let line = odd(size * LINE);
        // Text is centred in its line box (gpui): its glyph band runs from
        // `(line - band) / 2` for `band` px. Widen it outward to whole px.
        let band = size * (shape.ascent + shape.descent);
        let top = ((line - band) / 2.).max(0.);
        let (glyph_top, glyph_bottom) = (top.floor(), (top + band).ceil().min(line));
        Metrics {
            size,
            cell: size * shape.cell,
            line,
            row: line,
            gutter: (size * 6. / 7.).round(),
            gap: (size * 0.5).round(),
            glyph_top,
            glyph: glyph_bottom - glyph_top,
            caret: (size / 14.).round().max(1.),
        }
    }

    /// Where a message's text starts, from the pane's gutter: the time
    /// and a space (when shown), the nick column, a space. The input
    /// line's text starts here too, so what is typed lines up with what is
    /// said.
    pub fn text_column(&self, cols: Columns) -> f32 {
        self.grid(cols.lead() + cols.nick + 1)
    }

    /// Width of `n` digit cells, rounded up to a whole px.
    pub fn cells(&self, n: f32) -> f32 {
        (self.cell * n).ceil()
    }

    /// Vertical padding of the header and input bar, and the clear space
    /// at the ends of the window list and the message view: with the
    /// bar's hairline, a bar's padding makes one line, so a bar is two.
    pub fn bar_pad(&self) -> f32 {
        (self.line - 1.) / 2.
    }

    /// The window list's width for a window `window_width` wide: a fixed
    /// number of cells, but never more than a share of the window, and in
    /// a narrow window no more than leaves the messages `MESSAGE_CELLS`
    /// (nor fewer than a usable minimum): the conversation gives way last.
    ///
    /// Its floor keeps the network tags (`SIDEBAR_TAGS_CELLS`) while the
    /// messages can still keep their own least (`MESSAGE_MIN_CELLS`, no
    /// time column): network identity is the last thing to go.
    pub fn sidebar_width(&self, window_width: f32) -> f32 {
        let width = |cells| self.cells(cells) + 2. * self.gutter;
        let room = (window_width * SIDEBAR_MAX_SHARE).min(window_width - width(MESSAGE_CELLS)).floor();
        let floor = width(SIDEBAR_MIN_CELLS).max(width(SIDEBAR_TAGS_CELLS).min((window_width - width(MESSAGE_MIN_CELLS)).floor()));
        width(SIDEBAR_CELLS).min(room.max(floor))
    }

    /// Width of the window-number column for `windows` windows: as many
    /// cells as the widest number has digits.
    pub fn number_width(&self, windows: usize) -> f32 {
        self.cells(digits(windows) as f32)
    }

    /// Width of the unread-count column, reserved on every row.
    pub fn count_width(&self) -> f32 {
        self.cells(COUNT_CELLS as f32)
    }

    /// The most a network tag may take.
    pub fn tag_max(&self) -> f32 {
        self.cells(TAG_CELLS)
    }

    /// The least a network tag keeps while its label is cut.
    pub fn tag_min(&self) -> f32 {
        self.cells(TAG_MIN_CELLS)
    }

    /// Exact width of `n` cells (unrounded), for columns that must stay on
    /// the monospace grid however many of them sit side by side.
    pub fn grid(&self, n: usize) -> f32 {
        self.cell * n as f32
    }

    /// The message view's columns for a view `view_width` wide. The same
    /// in every buffer (a longer nick shows whole on its own line and
    /// pushes that line's text on), so the text column and the input line
    /// never move when you switch windows. As the view narrows, the nick
    /// column gives way first (to `NICK_NARROW_CELLS`) so the text keeps
    /// `TEXT_MIN_CELLS`, then the time column goes, then the nicks narrow
    /// to `NICK_COMPACT_CELLS`; the nicks never take more than a share of
    /// the view.
    pub fn columns(&self, view_width: f32) -> Columns {
        let share = ((view_width * NICK_MAX_SHARE) / self.cell).floor() as usize;
        let cells = ((view_width - 2. * self.gutter) / self.cell).floor().max(0.) as usize;
        let time = if cells >= TIME_CELLS + 1 + NICK_NARROW_CELLS + 1 + TEXT_MIN_CELLS { TIME_CELLS } else { 0 };
        let lead = if time > 0 { time + 1 } else { 0 };
        let room = cells.saturating_sub(lead + 1 + TEXT_MIN_CELLS);
        let (floor, most) = if time > 0 { (NICK_NARROW_CELLS, NICK_MAX_CELLS) } else { (NICK_COMPACT_CELLS, NICK_NARROW_CELLS) };
        let nick = most.min(room.max(floor)).min(share.max(NICK_MIN_CELLS));
        Columns { time, nick, view: cells }
    }

    /// The least a row label keeps when space runs out.
    pub fn label_min(&self) -> f32 {
        self.cells(LABEL_MIN_CELLS)
    }
}

/// The message view's columns, in cells (see `Metrics::columns`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Columns {
    /// The time column: `TIME_CELLS`, or 0 in a narrow view.
    pub time: usize,
    /// The nick column.
    pub nick: usize,
    /// The whole view, inside its gutters.
    pub view: usize,
}

impl Columns {
    /// Cells before the nick column: the time and its space, if shown.
    pub fn lead(&self) -> usize {
        if self.time > 0 { self.time + 1 } else { 0 }
    }

    /// The most a nick too long for the column may take on its own line,
    /// shown whole, before it too ends in `…`: what leaves the line's text
    /// `OVERFLOW_TEXT_CELLS`.
    pub fn nick_overflow(&self) -> usize {
        self.view.saturating_sub(self.lead() + 1 + OVERFLOW_TEXT_CELLS).max(self.nick)
    }
}

/// The odd whole number nearest `v` (from below on a tie), at least 1.
fn odd(v: f32) -> f32 {
    (2. * ((v - 1.) / 2. - 0.5).ceil() + 1.).max(1.)
}

/// Decimal digits in `n` (at least one).
pub fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// An unread count that fits `COUNT_CELLS` cells.
pub fn count_label(n: usize) -> String {
    if n < 1000 { n.to_string() } else { "1k+".to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menlo(size: f32) -> Metrics {
        Metrics::new(size, Shape { cell: 0.6021, ascent: 0.928, descent: 0.236 })
    }

    #[test]
    fn stacked_values_are_whole_pixels() {
        for size in [9., 11., 12.5, 13., 14., 15., 16., 18., 21., 24.] {
            let m = menlo(size);
            for v in [m.line, m.row, m.gutter, m.gap, m.bar_pad(), m.glyph_top, m.glyph] {
                assert_eq!(v, v.round(), "size {size}: {v} is fractional");
            }
            // One line grid: list rows are lines, and a bar (padding,
            // line, padding, hairline) is two.
            assert_eq!(m.row, m.line);
            assert_eq!(2. * m.bar_pad() + m.line + 1., 2. * m.line);
        }
    }

    #[test]
    fn spacing_grows_with_the_font() {
        let (a, b) = (menlo(12.), menlo(20.));
        assert!(b.line > a.line && b.row > a.row && b.gutter > a.gutter && b.gap > a.gap && b.cell > a.cell);
        assert!(b.sidebar_width(2000.) > a.sidebar_width(2000.));
    }

    #[test]
    fn values_at_menlo_14() {
        let m = menlo(14.);
        assert_eq!((m.line, m.row, m.gutter, m.gap), (21., 21., 12., 7.));
        assert_eq!(m.bar_pad(), 10.);
        // Monaco 13's 19.5 takes the odd px below.
        assert_eq!(menlo(13.).line, 19.);
        assert_eq!(odd(20.), 19.);
        assert_eq!(odd(21.4), 21.);
        assert_eq!(odd(22.1), 23.);
    }

    #[test]
    fn sidebar_width_is_cells_capped_by_the_window() {
        let m = menlo(14.);
        let full = m.cells(SIDEBAR_CELLS) + 2. * m.gutter;
        assert_eq!(m.sidebar_width(1200.), full);
        assert_eq!(m.sidebar_width(900.), full);
        // Narrow: the share, unless the messages would get too few cells.
        assert_eq!(m.sidebar_width(760.), 228.);
        // Narrower: the floor that keeps the tags, while the messages can
        // keep their own least.
        let tags = m.cells(SIDEBAR_TAGS_CELLS) + 2. * m.gutter;
        assert_eq!(m.sidebar_width(640.), tags);
        assert_eq!(m.sidebar_width(700.), tags);
        let view = 640. - tags;
        assert!(((view - 2. * m.gutter) / m.cell).floor() >= MESSAGE_MIN_CELLS);
        // Never below the minimum, however narrow the window.
        assert_eq!(m.sidebar_width(200.), m.cells(SIDEBAR_MIN_CELLS) + 2. * m.gutter);
    }

    #[test]
    fn number_column_fits_the_widest_number() {
        let m = menlo(14.);
        assert_eq!(m.number_width(9), m.cells(1.));
        assert_eq!(m.number_width(28), m.cells(2.));
        assert_eq!(m.number_width(100), m.cells(3.));
        assert_eq!(m.number_width(0), m.cells(1.));
    }

    #[test]
    fn counts_fit_their_column() {
        assert_eq!(count_label(7), "7");
        assert_eq!(count_label(999), "999");
        assert_eq!(count_label(1000), "1k+");
        assert!(count_label(123_456).chars().count() <= COUNT_CELLS);
    }

    #[test]
    fn message_columns_give_way_in_order() {
        let m = menlo(14.);
        let at = |w: f32| m.columns(w);
        assert_eq!((at(1000.).time, at(1000.).nick), (TIME_CELLS, NICK_MAX_CELLS));
        assert_eq!(at(657.).nick, NICK_MAX_CELLS);
        // Narrower: the nicks give way so the text keeps about 40 cells,
        // but no further than 8, with the time still shown.
        let narrow = at(495.);
        assert_eq!((narrow.time, narrow.nick), (TIME_CELLS, NICK_NARROW_CELLS));
        // The review's 640 window (a 439px view): the time goes, the nicks
        // keep 8, and the text 40.
        let small = at(640. - m.sidebar_width(640.));
        assert_eq!((small.time, small.nick), (0, NICK_NARROW_CELLS));
        assert!(small.view - small.lead() - small.nick > TEXT_MIN_CELLS, "{small:?}");
        // Narrower still: the nicks go down to their compact floor.
        let tight = at(400.);
        assert_eq!((tight.time, tight.nick), (0, NICK_COMPACT_CELLS));
        // A very narrow view gives the nick at most a fifth of its width.
        assert_eq!(at(200.).nick, (200. * NICK_MAX_SHARE / m.cell) as usize);
        assert_eq!(at(10.).nick, NICK_MIN_CELLS);
    }

    #[test]
    fn a_long_nick_may_overflow_its_column_but_leaves_the_text_room() {
        let m = menlo(14.);
        let wide = m.columns(957.);
        assert!(wide.nick_overflow() >= 32 && wide.view - wide.lead() - wide.nick_overflow() > OVERFLOW_TEXT_CELLS);
        // At 640 an IRC nick of 32 still shows whole.
        assert!(m.columns(640. - m.sidebar_width(640.)).nick_overflow() >= 31);
        // Never less than the column itself.
        let tiny = m.columns(100.);
        assert_eq!(tiny.nick_overflow(), tiny.nick);
    }

    #[test]
    fn message_columns_sit_on_the_grid() {
        let m = menlo(14.);
        // time, space, five-cell nick, space: text starts on cell 12.
        let x = m.grid(TIME_CELLS + 1 + 5 + 1);
        assert!((x / m.cell - 12.).abs() < 1e-4);
    }

    #[test]
    fn cursor_fits_the_glyph_band() {
        for size in [9., 13., 14., 20.] {
            let m = menlo(size);
            assert!(m.glyph > m.size && m.glyph <= m.line, "{size}");
            assert!(m.glyph_top >= 0. && m.glyph_top + m.glyph <= m.line);
            assert!(m.caret >= 1. && m.caret == m.caret.round());
            // The band holds the font's whole ascent-to-descent box.
            let top = (m.line - size * (0.928 + 0.236)) / 2.;
            assert!(m.glyph_top <= top + 1e-4 && m.glyph_top + m.glyph >= top + size * (0.928 + 0.236) - 1e-4, "{size}");
        }
        let m = menlo(14.);
        assert_eq!((m.glyph_top, m.glyph, m.caret), (2., 17., 1.));
        // Centred: as much clear space below the band as above.
        assert_eq!(m.line - m.glyph_top - m.glyph, m.glyph_top);
        assert!(menlo(28.).caret > menlo(14.).caret);
    }

    #[test]
    fn input_text_starts_on_the_message_text_column() {
        let m = menlo(14.);
        let buffer = m.grid(TIME_CELLS) + m.cell + m.grid(7) + m.cell;
        assert!((m.text_column(Columns { time: TIME_CELLS, nick: 7, view: 80 }) - buffer).abs() < 1e-3);
        // Without the time column, the nicks start on the gutter.
        assert!((m.text_column(Columns { time: 0, nick: 8, view: 49 }) - m.grid(9)).abs() < 1e-3);
    }

    #[test]
    fn bad_sizes_fall_back() {
        assert_eq!(Metrics::new(f32::NAN, Shape::FALLBACK), Metrics::new(14., Shape::FALLBACK));
        assert_eq!(Metrics::new(0., Shape::FALLBACK).size, 14.);
    }
}
