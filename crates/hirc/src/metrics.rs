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
/// Secondary text (network tags) as a fraction of the font size.
const SMALL: f32 = 6. / 7.;
/// The window list's width in cells, and its limits.
const SIDEBAR_CELLS: f32 = 26.;
const SIDEBAR_MIN_CELLS: f32 = 16.;
/// The most of the window the list may take.
const SIDEBAR_MAX_SHARE: f32 = 0.3;
/// Unread counts get this many cells; larger counts are abbreviated.
pub const COUNT_CELLS: usize = 3;
/// A network tag never takes more than this many (small) cells.
const TAG_CELLS: f32 = 8.;
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
    /// The configured font size.
    pub size: f32,
    /// Font size of secondary text (network tags).
    pub small: f32,
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
    /// How far small text moves down so its baseline meets the row's.
    pub small_drop: f32,
    /// Height of the font's glyph band (ascent + descent), which sits
    /// centred in the line box: the cursor's height.
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
        let small = (size * SMALL).round();
        // Text is centred in its line box (gpui), so its baseline sits at
        // `line / 2 + size * (ascent - descent) / 2`; small text in the same
        // line box needs to move down by the difference.
        let small_drop = (size - small) * (shape.ascent - shape.descent) / 2.;
        Metrics {
            size,
            small,
            cell: size * shape.cell,
            line,
            pad_y,
            row: line + 2. * pad_y,
            gutter: (size * 6. / 7.).round(),
            gap: (size * 0.5).round(),
            edge: (size * 0.5).round(),
            small_drop,
            glyph: (size * (shape.ascent + shape.descent)).round().min(line),
            caret: (size / 7.).round().max(1.),
        }
    }

    /// Top of the glyph band inside a line box, in whole px.
    pub fn glyph_top(&self) -> f32 {
        ((self.line - self.glyph) / 2.).floor()
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
    /// number of cells, but never more than a share of the window (nor
    /// fewer than a usable minimum).
    pub fn sidebar_width(&self, window_width: f32) -> f32 {
        let width = |cells| self.cells(cells) + 2. * self.gutter;
        width(SIDEBAR_CELLS).min((window_width * SIDEBAR_MAX_SHARE).floor().max(width(SIDEBAR_MIN_CELLS)))
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
        (self.cell * self.small / self.size * TAG_CELLS).ceil()
    }

    /// Exact width of `n` cells (unrounded), for columns that must stay on
    /// the monospace grid however many of them sit side by side.
    pub fn grid(&self, n: usize) -> f32 {
        self.cell * n as f32
    }

    /// Cells in the message view's nick column, given the longest prefix
    /// (nick or marker) in the buffer and the view's width: wide enough for
    /// the longest, within limits, and narrower in a narrow window so the
    /// text keeps most of the line.
    pub fn nick_cells(&self, longest: usize, view_width: f32) -> usize {
        let share = ((view_width * NICK_MAX_SHARE) / self.cell).floor() as usize;
        longest.clamp(NICK_MIN_CELLS, NICK_MAX_CELLS.min(share.max(NICK_MIN_CELLS)))
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
            for v in [m.line, m.pad_y, m.row, m.gutter, m.gap, m.edge, m.bar_pad(), m.small] {
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
        assert_eq!((m.line, m.pad_y, m.row, m.gutter, m.gap, m.edge, m.small), (21., 3., 27., 12., 7., 7., 12.));
        assert_eq!(m.bar_pad(), 10.);
    }

    #[test]
    fn small_text_drops_to_the_row_baseline() {
        let m = menlo(14.);
        let baseline = |size: f32, drop: f32| m.line / 2. + size * (0.928 - 0.236) / 2. + drop;
        assert!((baseline(m.size, 0.) - baseline(m.small, m.small_drop)).abs() < 1e-4);
    }

    #[test]
    fn sidebar_width_is_cells_capped_by_the_window() {
        let m = menlo(14.);
        let full = m.cells(SIDEBAR_CELLS) + 2. * m.gutter;
        assert_eq!(m.sidebar_width(1200.), full);
        assert_eq!(m.sidebar_width(640.), 192.);
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
        assert_eq!(m.nick_cells(5, 1000.), 5);
        assert_eq!(m.nick_cells(1, 1000.), NICK_MIN_CELLS);
        assert_eq!(m.nick_cells(22, 1000.), NICK_MAX_CELLS);
        // A narrow view gives the nick at most a fifth of its width.
        assert_eq!(m.nick_cells(22, 400.), (400. * NICK_MAX_SHARE / m.cell) as usize);
        assert_eq!(m.nick_cells(22, 10.), NICK_MIN_CELLS);
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
            assert!(m.glyph_top() >= 0. && m.glyph_top() + m.glyph <= m.line);
            assert!(m.caret >= 1. && m.caret == m.caret.round());
        }
        assert_eq!((menlo(14.).glyph, menlo(14.).caret), (16., 2.));
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
