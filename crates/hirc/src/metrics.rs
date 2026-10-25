//! Every spacing value in the UI, derived from the configured font size and
//! the font's own shape (digit advance, ascent, descent), so the layout keeps
//! its proportions at any font and size. Values are logical px, rounded to
//! whole px where they stack (line, row, paddings), so rows never land on
//! half pixels. Hairline borders (1px) are the only fixed value.
//!
//! The vertical rhythm: a text `line` plus `pad_y` above and below makes a
//! `row` (a sidebar row). Panes keep `edge` clear at the top and bottom, and
//! the header and input bar are one `row` inside `edge` (`bar_pad`), so their
//! text sits on the same baseline as the first sidebar row.
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

/// Line height as a multiple of the font size.
const LINE: f32 = 1.5;
/// The window list's width in cells, and its limits.
const SIDEBAR_CELLS: f32 = 26.;
const SIDEBAR_MIN_CELLS: f32 = 16.;
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

/// A label always keeps at least this many cells before tags give way.
const LABEL_MIN_CELLS: f32 = 6.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// The configured font size: every piece of text uses it, as a
    /// terminal does; hierarchy comes from colour roles and weight.
    pub size: f32,
    /// Width of one digit at `size`.
    pub cell: f32,
    /// Line height for all text.
    pub line: f32,
    /// Space above and below a line inside a row.
    pub pad_y: f32,
    /// One row of a list: `line + 2 * pad_y`.
    pub row: f32,
    /// Inset from every pane edge.
    pub gutter: f32,
    /// Space between columns in a row.
    pub gap: f32,
    /// Clear space at the top and bottom of a pane.
    pub edge: f32,
    /// The font's glyph band (ascent + descent, centred in the line box
    /// as text is), widened to whole px: the cursor covers it, so every
    /// glyph under it, descenders included, is inside.
    pub glyph_top: f32,
    pub glyph: f32,
    /// Width of the insert cursor (a bar), and its clearance from the
    /// glyph before it.
    pub caret: f32,
}

impl Metrics {
    pub fn new(size: f32, shape: Shape) -> Metrics {
        let size = if size.is_finite() && size > 0. { size } else { 14. };
        let line = (size * LINE).round();
        let pad_y = (size * 0.2).round();
        // Text is centred in its line box (gpui): its glyph band runs from
        // `(line - band) / 2` for `band` px. Widen it outward to whole px.
        let band = size * (shape.ascent + shape.descent);
        let top = ((line - band) / 2.).max(0.);
        let (glyph_top, glyph_bottom) = (top.floor(), (top + band).ceil().min(line));
        Metrics {
            size,
            cell: size * shape.cell,
            line,
            pad_y,
            row: line + 2. * pad_y,
            gutter: (size * 6. / 7.).round(),
            gap: (size * 0.5).round(),
            edge: (size * 0.5).round(),
            glyph_top,
            glyph: glyph_bottom - glyph_top,
            caret: (size / 7.).round().max(1.),
        }
    }

    /// Where a message's text starts, from the pane's gutter: the time,
    /// a space, a nick column `nick_cells` wide, a space. The input line's
    /// text starts here too, so what is typed lines up with what is said.
    pub fn text_column(&self, nick_cells: usize) -> f32 {
        self.grid(TIME_CELLS + 1 + nick_cells + 1)
    }

    /// Width of `n` digit cells, rounded up to a whole px.
    pub fn cells(&self, n: f32) -> f32 {
        (self.cell * n).ceil()
    }

    /// Vertical padding of the header and input bar: one row centred in
    /// `edge` clear space, matching the first sidebar row's baseline.
    pub fn bar_pad(&self) -> f32 {
        self.edge + self.pad_y
    }

    /// The window list's width for a window `window_width` wide: a fixed
    /// number of cells, but never more than a share of the window, and in
    /// a narrow window no more than leaves the messages `MESSAGE_CELLS`
    /// (nor fewer than a usable minimum): the conversation gives way last.
    pub fn sidebar_width(&self, window_width: f32) -> f32 {
        let width = |cells| self.cells(cells) + 2. * self.gutter;
        let room = (window_width * SIDEBAR_MAX_SHARE).min(window_width - width(MESSAGE_CELLS)).floor();
        width(SIDEBAR_CELLS).min(room.max(width(SIDEBAR_MIN_CELLS)))
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

    /// Cells in the message view's nick column for a view `view_width`
    /// wide. The same in every buffer (longer nicks end in an ellipsis), so
    /// the text column and the input line never move when you switch
    /// windows; narrower in a narrow window so the text keeps most of the
    /// line.
    pub fn nick_cells(&self, view_width: f32) -> usize {
        let share = ((view_width * NICK_MAX_SHARE) / self.cell).floor() as usize;
        NICK_MAX_CELLS.min(share.max(NICK_MIN_CELLS))
    }

    /// The least a row label keeps when space runs out.
    pub fn label_min(&self) -> f32 {
        self.cells(LABEL_MIN_CELLS)
    }
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
            for v in [m.line, m.pad_y, m.row, m.gutter, m.gap, m.edge, m.bar_pad(), m.glyph_top, m.glyph] {
                assert_eq!(v, v.round(), "size {size}: {v} is fractional");
            }
            assert_eq!(m.row, m.line + 2. * m.pad_y);
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
        assert_eq!((m.line, m.pad_y, m.row, m.gutter, m.gap, m.edge), (21., 3., 27., 12., 7., 7.));
        assert_eq!(m.bar_pad(), 10.);
    }

    #[test]
    fn sidebar_width_is_cells_capped_by_the_window() {
        let m = menlo(14.);
        let full = m.cells(SIDEBAR_CELLS) + 2. * m.gutter;
        assert_eq!(m.sidebar_width(1200.), full);
        assert_eq!(m.sidebar_width(900.), full);
        // Narrow: the share, unless the messages would get too few cells.
        assert_eq!(m.sidebar_width(760.), 228.);
        assert_eq!(m.sidebar_width(640.), m.cells(SIDEBAR_MIN_CELLS) + 2. * m.gutter);
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
    fn nick_column_fits_within_limits() {
        let m = menlo(14.);
        assert_eq!(m.nick_cells(1000.), NICK_MAX_CELLS);
        // A narrow view gives the nick at most a fifth of its width.
        assert_eq!(m.nick_cells(400.), (400. * NICK_MAX_SHARE / m.cell) as usize);
        assert_eq!(m.nick_cells(10.), NICK_MIN_CELLS);
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
        assert_eq!((m.glyph_top, m.glyph, m.caret), (2., 17., 2.));
        // Centred: as much clear space below the band as above.
        assert_eq!(m.line - m.glyph_top - m.glyph, m.glyph_top);
        assert!(menlo(28.).caret > menlo(14.).caret);
    }

    #[test]
    fn input_text_starts_on_the_message_text_column() {
        let m = menlo(14.);
        let buffer = m.grid(TIME_CELLS) + m.cell + m.grid(7) + m.cell;
        assert!((m.text_column(7) - buffer).abs() < 1e-3);
    }

    #[test]
    fn bad_sizes_fall_back() {
        assert_eq!(Metrics::new(f32::NAN, Shape::FALLBACK), Metrics::new(14., Shape::FALLBACK));
        assert_eq!(Metrics::new(0., Shape::FALLBACK).size, 14.);
    }
}
