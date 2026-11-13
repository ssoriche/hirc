//! The message input: a text field with IME, selection and clipboard.
//! Adapted from gpui's `examples/input.rs` (Apache-2.0, Zed Industries).
//! Enter emits `Submit` with the text; the app clears it once it goes
//! (`sent`), or holds it with a note under it (`hold`).
//!
//! It is one row until the text needs more: a line of its own for each
//! line break (Shift+Enter, or a paste), and a long line wrapped where
//! the message view would wrap it, up to `MAX_ROWS`, then it scrolls
//! inside. Every row is a line of the grid, so the view above gives up
//! whole lines. The ex line stays one row and scrolls sideways.

use std::ops::Range;

use crate::metrics::{Metrics, Shape};
use crate::theme::Palette;
use crate::vim::{self, Effect, Key, Mode, Named, Vim};
use crate::wrap;
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, deferred, div, fill, point, prelude::*, px, relative, size,
};
use hirc_core::command;
use hirc_core::reactions::{PickKey, Picked, Picker};
use unicode_segmentation::*;

actions!(text_input, [Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, ShowCharacterPalette, Paste, Cut, Copy, Enter, Newline,]);

/// The most rows the input shows; more text scrolls inside them.
pub const MAX_ROWS: usize = 5;

/// A one-line note under the text: why Enter did not send it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// It can't go as it is (in the error role).
    Error(String),
    /// It goes on a second Enter (in the hint role); Esc or any edit
    /// puts it away.
    Confirm(String),
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    /// Shown while the line is empty; the app names the target in it.
    pub placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// The rows drawn last, when the text took more than one: each one's
    /// range of the text, its line and where it was drawn.
    last_rows: Vec<(Range<usize>, ShapedLine, Point<Pixels>)>,
    /// Horizontal scroll of the text, recomputed on every prepaint.
    scroll_x: Pixels,
    /// The first row shown, when there are more than `MAX_ROWS`.
    scroll_row: usize,
    is_selecting: bool,
    vim: Vim,
    /// For the mode label; set by the app.
    pub palette: Option<Palette>,
    /// Spacing, from the configured font; set by the app.
    pub metrics: Option<Metrics>,
    /// Where the text starts, from the input's left edge: the message
    /// view's text column, so a line is typed where it will be shown. The
    /// mode label sits right-aligned before it, where nicks sit.
    pub column: f32,
    /// The text's width (from the column to the bar's padding), set by
    /// the app: rows wrap against it, as the message view's text does.
    pub width: f32,
    /// Shown under the text until the next edit (see `Note`).
    pub note: Option<Note>,
    /// Shown after the mode label, in the quiet role, while the app has
    /// something to say about Normal mode (a marked line); set by the app.
    pub status: Option<SharedString>,
    /// The Tab completion in progress, so another Tab offers the next match.
    cycle: Option<hirc_core::complete::Cycle>,
    /// The emoji popup over the input, while a `:name` is being typed.
    popup: Option<hirc_core::complete::Popup>,
    /// The `:` of a popup closed with Esc, so it stays closed until that
    /// `:name` is left.
    dismissed: Option<usize>,
    /// The react picker over the input, while it is open: every key goes
    /// to it, none to the text.
    pub picker: Option<Picker>,
}

impl TextInput {
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.pick(PickKey::Backspace, cx);
            return;
        }
        // Key bindings run before `capture_key_down`, so route through vim here.
        if self.vim_key(named_key(Named::Backspace), cx) {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.vim.mode == Mode::Normal || self.picker.is_some() {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;

        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn show_character_palette(&mut self, _: &ShowCharacterPalette, window: &mut Window, _: &mut Context<Self>) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = pasted(&text, self.vim.mode == Mode::Ex);
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    /// Shift+Enter: a line break, in Insert mode (the ex line is one line).
    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.vim.mode == Mode::Insert {
            self.replace_text_in_range(None, "\n", window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        if !self.selected_range.is_empty() {
            let sel = clamp_range(&self.content, &self.selected_range);
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[sel].to_string()));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            return;
        }
        if !self.selected_range.is_empty() {
            let sel = clamp_range(&self.content, &self.selected_range);
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[sel].to_string()));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        // In Normal mode the cursor sits on a char, never at `len`; in ex
        // it never goes before the `:`.
        let offset = match self.vim.mode {
            Mode::Normal => vim::clamp_normal(&self.content, offset),
            Mode::Ex => offset.max(self.hidden()),
            Mode::Insert => offset,
        };
        self.selected_range = offset..offset;
        self.refresh_popup();
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed { self.selected_range.start } else { self.selected_range.end }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        if let Some(ix) = self.row_index(position) {
            return ix;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left() + self.scroll_x) + self.hidden()
    }

    /// The character nearest `position` among the rows drawn last (above
    /// them, the first row; below, the last), when the text took several.
    fn row_index(&self, position: Point<Pixels>) -> Option<usize> {
        let (range, row, origin) = self.last_rows.iter().rev().find(|(_, _, o)| position.y >= o.y).or(self.last_rows.first())?;
        Some(range.start + row.closest_index_for_x(position.x - origin.x).min(range.len()))
    }

    /// Bytes at the start of the line that are not drawn as text: the ex
    /// line's `:`, which the mode label shows instead (one colon, vim's).
    fn hidden(&self) -> usize {
        usize::from(self.vim.mode == Mode::Ex && self.content.starts_with(':'))
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.refresh_popup();
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        utf16_to_utf8(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        utf8_to_utf16(&self.content, offset)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content.grapheme_indices(true).rev().find_map(|(idx, _)| (idx < offset).then_some(idx)).unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content.grapheme_indices(true).find_map(|(idx, _)| (idx > offset).then_some(idx)).unwrap_or(self.content.len())
    }

    fn reset(&mut self) {
        self.content = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        self.last_rows.clear();
        self.scroll_x = px(0.);
        self.scroll_row = 0;
        self.is_selecting = false;
        self.note = None;
        self.cycle = None;
        self.popup = None;
        self.dismissed = None;
    }

    /// The rows the text shows as, `width` wide, when it needs more than
    /// one: a row for each line break, and a long line wrapped where the
    /// message view would wrap it (see `wrap`). `None` for one row, and
    /// for the ex line, which stays one row and scrolls sideways.
    fn rows(&self, window: &Window, width: f32) -> Option<Vec<Range<usize>>> {
        if self.vim.mode == Mode::Ex || self.content.is_empty() {
            return None;
        }
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let text = &self.content;
        let breaks = if width > 0. {
            let shape = |r: Range<usize>| {
                let run = TextRun { len: r.len(), font: style.font(), color: style.color, background_color: None, underline: None, strikethrough: None };
                let line = (!r.is_empty()).then(|| window.text_system().shape_line(text[r].to_owned().into(), font_size, &[run], None));
                move |i: usize| line.as_ref().map_or(0., |l| f32::from(l.x_for_index(i)))
            };
            // Half a px short, as the message view breaks.
            wrap::breaks_lines(text, shape, width - 0.5)
        } else {
            Vec::new()
        };
        let rows = wrap::rows(text, &breaks);
        (rows.len() > 1).then_some(rows)
    }

    /// Whether a line is marked in the buffer, so vim gives the line keys
    /// to it (see `Vim::marked`).
    pub fn set_marked(&mut self, on: bool) {
        self.vim.marked = on;
    }

    /// The width rows wrap against: the app's, else what was drawn last.
    /// While a status sits before the text, the text has only what is
    /// left of the row.
    fn wrap_width(&self, bounds: Option<Bounds<Pixels>>) -> f32 {
        let drawn = bounds.map(|b| f32::from(b.size.width));
        match (self.width > 0., self.status.is_some(), drawn) {
            (true, true, Some(d)) => self.width.min(d),
            (true, _, _) => self.width,
            (false, _, d) => d.unwrap_or(0.),
        }
    }

    /// Tab: complete the word before the cursor from `nicks` (or the
    /// commands, at the start of a `/` line), or the next match on
    /// another Tab.
    pub fn tab(&mut self, nicks: &[String], cx: &mut Context<Self>) {
        let cursor = clamp_range(&self.content, &(self.cursor_offset()..self.cursor_offset())).start;
        if let Some((text, cursor, cycle)) = hirc_core::complete::tab(&self.content, cursor, nicks, self.cycle.as_ref()) {
            self.edited();
            self.content = text.into();
            self.selected_range = cursor..cursor;
            self.selection_reversed = false;
            self.marked_range = None;
            self.cycle = Some(cycle);
            self.refresh_popup();
            cx.notify();
        }
    }

    /// The emoji popup for the text and cursor now (Insert mode only).
    fn refresh_popup(&mut self) {
        let cursor = self.cursor_offset();
        self.popup = (self.vim.mode == Mode::Insert).then(|| hirc_core::complete::popup(&self.content, cursor, self.dismissed, self.popup.as_ref())).flatten();
        if hirc_core::complete::trigger(&self.content, cursor).is_none_or(|r| Some(r.start) != self.dismissed) {
            self.dismissed = None;
        }
    }

    /// The text changed: a note about what it was goes.
    fn edited(&mut self) {
        self.note = None;
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(&mut self, _ignore_disabled_input: bool, _window: &mut Window, _cx: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to_utf16(&self.selected_range), reversed: self.selection_reversed })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, new_text: &str, _: &mut Window, cx: &mut Context<Self>) {
        // Normal mode never inserts text, nor does anything while the
        // picker has the keys (the IME included).
        if self.vim.mode == Mode::Normal || self.picker.is_some() {
            return;
        }
        let range =
            range_utf16.as_ref().map(|range_utf16| self.range_from_utf16(range_utf16)).or(self.marked_range.clone()).unwrap_or(self.selected_range.clone());
        let range = clamp_range(&self.content, &range);

        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        self.edited();
        self.refresh_popup();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // As `replace_text_in_range`: no composing while the picker has the keys.
        if self.vim.mode == Mode::Normal || self.picker.is_some() {
            return;
        }
        let range =
            range_utf16.as_ref().map(|range_utf16| self.range_from_utf16(range_utf16)).or(self.marked_range.clone()).unwrap_or(self.selected_range.clone());
        let range = clamp_range(&self.content, &range);

        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.edited();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        // The IME's selection is relative to `new_text`, not the whole content.
        self.selected_range = match new_selected_range_utf16 {
            Some(r) => range.start + utf16_to_utf8(new_text, r.start)..range.start + utf16_to_utf8(new_text, r.end),
            None => range.start + new_text.len()..range.start + new_text.len(),
        };

        self.refresh_popup();
        cx.notify();
    }

    fn bounds_for_range(&mut self, range_utf16: Range<usize>, bounds: Bounds<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        // Several rows: the row the range starts on.
        if let Some((r, row, origin)) = self.last_rows.iter().rev().find(|(r, _, _)| r.start <= range.start) {
            let line = self.metrics.map_or(bounds.size.height, |m| px(m.line));
            let x = |i: usize| origin.x + row.x_for_index(i.clamp(r.start, r.end) - r.start);
            return Some(Bounds::from_corners(point(x(range.start), origin.y), point(x(range.end), origin.y + line)));
        }
        let last_layout = self.last_layout.as_ref()?;
        let x = |i: usize| bounds.left() - self.scroll_x + last_layout.x_for_index(i.saturating_sub(self.hidden()));
        Some(Bounds::from_corners(point(x(range.start), bounds.top()), point(x(range.end), bounds.bottom())))
    }

    fn character_index_for_point(&mut self, point: gpui::Point<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        if !self.last_rows.is_empty() {
            self.last_bounds?.localize(&point)?;
            return self.row_index(point).map(|ix| self.offset_to_utf16(ix));
        }
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;

        if last_layout.text != self.content[self.hidden()..] {
            return None;
        }
        let utf8_index = last_layout.index_for_x(point.x - line_point.x + self.scroll_x)?;
        Some(self.offset_to_utf16(utf8_index + self.hidden()))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    /// The placeholder, on the text column under the cursor.
    placeholder: Option<ShapedLine>,
    /// The Normal-mode block, drawn over the text, and the text again in
    /// the background colour (from where it is drawn), clipped to the
    /// block: reverse video inside the cell, as a terminal splits a glyph
    /// at a cell's edge, so ink past the cell (Menlo's `w`) keeps its own
    /// colour beside the block instead of vanishing.
    block: Option<PaintQuad>,
    reversed: Option<(ShapedLine, Point<Pixels>)>,
    /// The Insert/ex bar, drawn over the text.
    caret: Option<PaintQuad>,
    scroll_x: Pixels,
    selection: Option<PaintQuad>,
    /// `…` at an edge with text scrolled out of view past it, over a
    /// background-coloured cell.
    marks: Vec<(PaintQuad, Pixels, ShapedLine)>,
    /// While the ex line is open, the draft it parked: at the row's far
    /// end in the placeholder's role, and where it starts.
    parked: Option<(ShapedLine, Pixels)>,
    /// When the text takes several rows: the ones shown, each with its
    /// range of the text and where it is drawn (`line` is then `None`),
    /// and the selection's fill on each.
    rows: Vec<(Range<usize>, ShapedLine, Point<Pixels>)>,
    fills: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // One line, or as many rows as the text takes, up to `MAX_ROWS`.
        let input = self.input.read(cx);
        let rows = input.rows(window, input.wrap_width(input.last_bounds)).map_or(1, |r| r.len().min(MAX_ROWS));
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let m = input.metrics.unwrap_or_else(|| Metrics::new(f32::from(font_size), Shape::FALLBACK));
        let text = style.color;
        let (bg, dim, hint, selection_bg) = match &input.palette {
            Some(p) => (p.input_bg, p.text_dim, p.hint, p.selection),
            None => (gpui::transparent_black(), text.opacity(0.6), text.opacity(0.4), text.opacity(0.25)),
        };
        let hidden = input.hidden();
        let shown: SharedString = input.content[hidden..].to_owned().into();
        let shift = |i: usize| i.saturating_sub(hidden).min(shown.len());
        let selected_range = shift(input.selected_range.start)..shift(input.selected_range.end);
        let cursor = shift(input.cursor_offset());
        let normal = input.vim.mode == Mode::Normal;
        let prev_scroll = input.scroll_x;

        // Colours over the line: a `/command` hirc knows takes the command
        // colour, one it does not (a typo) the error colour, so either
        // shows before Enter; marked (IME) text is underlined. The Normal
        // block's glyph is drawn again over the block (`reversed`).
        let (code, wrong) = input.palette.as_ref().map_or((dim, dim), |p| (p.code, p.error));
        let mut over = Vec::new();
        if input.vim.mode != Mode::Ex
            && let Some((cmd, verb)) = command::verb(&shown)
        {
            // A verb still being typed that a command begins with stays
            // plain text, so `/jo` on the way to `/join` never flashes red.
            match verb {
                command::Verb::Known => over.push((0..cmd.len() + 1, (code, false))),
                command::Verb::Unknown => over.push((0..cmd.len() + 1, (wrong, false))),
                command::Verb::Typing => {}
            }
        }
        if let Some(marked) = &input.marked_range {
            over.push((shift(marked.start)..shift(marked.end), (text, true)));
        }
        let runs: Vec<TextRun> = spans(shown.len(), (text, false), &over)
            .into_iter()
            .map(|(range, (color, underline))| TextRun {
                len: range.len(),
                font: style.font(),
                color,
                background_color: None,
                underline: underline.then_some(UnderlineStyle { color: Some(color), thickness: px(1.0), wavy: false }),
                strikethrough: None,
            })
            .collect();
        let shape = |s: SharedString, color| {
            let run = TextRun { len: s.len(), font: style.font(), color, background_color: None, underline: None, strikethrough: None };
            window.text_system().shape_line(s, font_size, &[run], None)
        };
        if hidden == 0
            && let Some(rows) = input.rows(window, input.wrap_width(Some(bounds)))
        {
            let spans = spans(shown.len(), (text, false), &over);
            let rows = Rows { rows, spans, shown: &shown, mode: input.vim.mode, cursor, selection: selected_range, scroll: input.scroll_row };
            let (state, top) = Self::rows(rows, bounds, window, m, (text, bg, selection_bg));
            self.input.update(cx, |input, _cx| {
                input.scroll_x = px(0.);
                input.scroll_row = top;
            });
            return state;
        }
        // The placeholder starts on the text column, where typing will:
        // the bar sits in the gap before it, as before any glyph, and under
        // the Normal block its first glyph is reversed, as typed text's is.
        let empty = input.content.is_empty() && !input.placeholder.is_empty();
        let placeholder = empty.then(|| shape(input.placeholder.clone(), hint));
        let line = window.text_system().shape_line(shown.clone(), font_size, &runs, None);

        // Keep the cursor two cells clear of each edge, so the `…` marks
        // never cover it; at the end, leave room for the bar. Scrolled, the
        // first glyph after the leading `…` starts where its cell ends, so
        // no sliver of a cut glyph shows between them.
        let view = f32::from(bounds.size.width);
        let margin = 2. * m.cell + 2. * m.caret;
        let edges: Vec<f32> = shown.char_indices().map(|(i, _)| i).chain([shown.len()]).map(|i| f32::from(line.x_for_index(i))).collect();
        let scroll_x = scroll_for_cursor(f32::from(prev_scroll), f32::from(line.x_for_index(cursor)), f32::from(line.width), view, margin, 2. * m.caret);
        let scroll_x = px(snap_scroll(scroll_x, &edges, m.cell));
        let origin_x = bounds.left() - scroll_x;
        let cursor_pos = line.x_for_index(cursor);
        let glyph_top = bounds.top() + px(m.glyph_top);
        let (mut block, mut caret, mut selection, mut reversed) = (None, None, None, None);
        if !selected_range.is_empty() {
            selection = Some(fill(
                Bounds::from_corners(
                    point(origin_x + line.x_for_index(selected_range.start), bounds.top()),
                    point(origin_x + line.x_for_index(selected_range.end), bounds.bottom()),
                ),
                selection_bg,
            ));
        } else if normal {
            let end = line.x_for_index(next_char_boundary(&shown, cursor));
            let width = if end > cursor_pos { end - cursor_pos } else { px(m.cell) };
            block = Some(fill(Bounds::new(point(origin_x + cursor_pos, glyph_top), size(width, px(m.glyph))), text));
            reversed =
                Some(if empty { (shape(input.placeholder.clone(), bg), bounds.origin) } else { (shape(shown.clone(), bg), point(origin_x, bounds.top())) });
        } else {
            // A bar's width clear of the glyph beside it: after the glyph
            // before it, or, at the start of an Insert line (where there is
            // none, and the placeholder's first glyph may follow), before
            // the text column, in the cell's gap after the mode label. The
            // ex line's `:` is that glyph before it, flush to the column.
            // Before the column, it keeps two bars' width clear of the
            // stem of a glyph that starts flush on it (the placeholder's
            // `M`), the clearance a glyph's own side bearing gives it after
            // typed text.
            let gap = px(m.caret);
            let x = if cursor == 0 && input.vim.mode == Mode::Insert { origin_x - gap * 2. - px(m.caret) } else { origin_x + cursor_pos + gap };
            caret = Some(fill(Bounds::new(point(x, glyph_top), size(px(m.caret), px(m.glyph))), text));
        }

        // The `…` marks cover whole glyphs only: the leading one its cell
        // (the scroll is snapped to it), the trailing one from the end of
        // the last glyph that fits before a cell's room.
        let mut marks = Vec::new();
        if scroll_x > px(0.5) {
            marks.push((bounds.left(), px(m.cell)));
        }
        if line.width - scroll_x > bounds.size.width + px(0.5) {
            let from = trailing_mark(&edges, f32::from(scroll_x), view, m.cell);
            marks.push((bounds.left() + px(from), bounds.size.width - px(from)));
        }
        let marks =
            marks.into_iter().map(|(x, w)| (fill(Bounds::new(point(x, bounds.top()), size(w, bounds.size.height)), bg), x, shape("…".into(), dim))).collect();
        // The draft under an open ex line stays in sight (vim keeps the
        // buffer on screen under its `:` line): its end, right-aligned,
        // cut from the front with `…` and giving way to the command, two
        // cells clear of it, and gone once fewer than a few cells are left.
        let parked = input.vim.parked().filter(|d| !d.is_empty()).and_then(|draft| {
            let room = view - (f32::from(line.width) - f32::from(scroll_x)).max(0.) - 2. * m.cell - 2. * m.caret;
            let shown = tail_fit(draft, room, |s| f32::from(shape(s.to_owned().into(), hint).width))?;
            let shaped = shape(shown.into(), hint);
            let x = bounds.right() - shaped.width;
            Some((shaped, x))
        });
        self.input.update(cx, |input, _cx| {
            input.scroll_x = scroll_x;
            input.scroll_row = 0;
        });
        PrepaintState { line: Some(line), placeholder, block, reversed, caret, scroll_x, selection, marks, parked, rows: Vec::new(), fills: Vec::new() }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);
        if !prepaint.rows.is_empty() {
            return self.paint_rows(bounds, prepaint, focus_handle.is_focused(window), window, cx);
        }
        let Some(line) = prepaint.line.take() else {
            return;
        };
        let focused = focus_handle.is_focused(window);
        let block = prepaint.block.take().filter(|_| focused);
        let caret = prepaint.caret.take().filter(|_| focused);
        let origin = point(bounds.origin.x - prepaint.scroll_x, bounds.origin.y);
        let line_height = window.line_height();
        // The line is scrolled, so keep everything inside the input's bounds.
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if let Some(quad) = prepaint.selection.take() {
                window.paint_quad(quad);
            }
            if let Err(e) = line.paint(origin, line_height, gpui::TextAlign::Left, None, window, cx) {
                tracing::debug!("input paint failed: {e}");
            }
            if let Some(placeholder) = prepaint.placeholder.take()
                && let Err(e) = placeholder.paint(bounds.origin, line_height, gpui::TextAlign::Left, None, window, cx)
            {
                tracing::debug!("input paint failed: {e}");
            }
            if let Some(block) = block {
                let cell = block.bounds;
                window.paint_quad(block);
                if let Some((text, at)) = prepaint.reversed.take() {
                    window.with_content_mask(Some(ContentMask { bounds: cell }), |window| {
                        if let Err(e) = text.paint(at, line_height, gpui::TextAlign::Left, None, window, cx) {
                            tracing::debug!("input paint failed: {e}");
                        }
                    });
                }
            }
            if let Some((parked, x)) = prepaint.parked.take()
                && let Err(e) = parked.paint(point(x, bounds.origin.y), line_height, gpui::TextAlign::Left, None, window, cx)
            {
                tracing::debug!("input paint failed: {e}");
            }
            for (quad, x, mark) in prepaint.marks.drain(..) {
                window.paint_quad(quad);
                if let Err(e) = mark.paint(point(x, bounds.origin.y), line_height, gpui::TextAlign::Left, None, window, cx) {
                    tracing::debug!("input paint failed: {e}");
                }
            }
        });
        // Outside the mask: at the line's start the bar sits in the gap
        // just before the text column (the cursor is never scrolled out).
        if let Some(caret) = caret {
            window.paint_quad(caret);
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.last_rows.clear();
        });
    }
}

/// What `TextElement::rows` lays out: the rows (ranges of `shown`), the
/// colour spans over the whole text, the mode, the cursor and selection,
/// and the first row shown last time.
struct Rows<'a> {
    rows: Vec<Range<usize>>,
    spans: Vec<(Range<usize>, (gpui::Hsla, bool))>,
    shown: &'a str,
    mode: Mode,
    cursor: usize,
    selection: Range<usize>,
    scroll: usize,
}

impl TextElement {
    /// The text on several rows: up to `MAX_ROWS` of them, scrolled so
    /// the cursor's row shows, each a line of the grid on the text
    /// column. The cursor, the Normal block and the selection go where
    /// they would on one line, on their own row. `colors` are the text,
    /// the background and the selection fill. Returns the first row shown.
    fn rows(r: Rows, bounds: Bounds<Pixels>, window: &mut Window, m: Metrics, colors: (gpui::Hsla, gpui::Hsla, gpui::Hsla)) -> (PrepaintState, usize) {
        let (text, bg, selection_bg) = colors;
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_h = px(m.line);
        let total = r.rows.len();
        let at = r.rows.iter().rposition(|row| row.start <= r.cursor).unwrap_or(0);
        let top = scroll_rows(r.scroll, at, total, MAX_ROWS);
        let runs = |range: &Range<usize>, color: Option<gpui::Hsla>| -> Vec<TextRun> {
            r.spans
                .iter()
                .filter_map(|(span, (c, underline))| {
                    let len = span.end.min(range.end).saturating_sub(span.start.max(range.start));
                    let c = color.unwrap_or(*c);
                    (len > 0).then(|| TextRun {
                        len,
                        font: style.font(),
                        color: c,
                        background_color: None,
                        underline: underline.then_some(UnderlineStyle { color: Some(c), thickness: px(1.0), wavy: false }),
                        strikethrough: None,
                    })
                })
                .collect()
        };
        let shape = |range: &Range<usize>, color: Option<gpui::Hsla>| {
            window.text_system().shape_line(r.shown[range.clone()].to_owned().into(), font_size, &runs(range, color), None)
        };
        let sel = &r.selection;
        let (mut rows, mut fills, mut block, mut reversed, mut caret) = (Vec::new(), Vec::new(), None, None, None);
        for (i, range) in r.rows.iter().enumerate().skip(top).take(MAX_ROWS) {
            let origin = point(bounds.left(), bounds.top() + line_h * (i - top) as f32);
            let line = shape(range, None);
            let x = |ix: usize| origin.x + line.x_for_index(ix.clamp(range.start, range.end) - range.start);
            // The selection on this row. Where it goes on over the break
            // that ends the row (a line break, a wrapped space), a cell
            // more shows that it takes the break too.
            let over = r.rows.get(i + 1).is_some_and(|next| next.start > range.end) && sel.end > range.end;
            if !sel.is_empty() && sel.start <= range.end && sel.end >= range.start {
                let (a, b) = (x(sel.start), if over { x(range.end) + px(m.cell) } else { x(sel.end) });
                if b > a {
                    fills.push(fill(Bounds::from_corners(point(a, origin.y), point(b, origin.y + line_h)), selection_bg));
                }
            }
            if i == at && sel.is_empty() {
                let glyph_top = origin.y + px(m.glyph_top);
                let cursor_x = x(r.cursor);
                if r.mode == Mode::Normal {
                    let end = next_char_boundary(r.shown, r.cursor).min(range.end);
                    let width = if x(end) > cursor_x { x(end) - cursor_x } else { px(m.cell) };
                    block = Some(fill(Bounds::new(point(cursor_x, glyph_top), size(width, px(m.glyph))), text));
                    reversed = Some((shape(range, Some(bg)), origin));
                } else {
                    // At a row's start the bar sits in the gap before the
                    // text column, as at the start of one line.
                    let gap = px(m.caret);
                    let x = if r.cursor == range.start { origin.x - gap * 2. - px(m.caret) } else { cursor_x + gap };
                    caret = Some(fill(Bounds::new(point(x, glyph_top), size(px(m.caret), px(m.glyph))), text));
                }
            }
            rows.push((range.clone(), line, origin));
        }
        let state = PrepaintState {
            line: None,
            placeholder: None,
            block,
            reversed,
            caret,
            scroll_x: px(0.),
            selection: None,
            marks: Vec::new(),
            parked: None,
            rows,
            fills,
        };
        (state, top)
    }

    /// `paint` for text on several rows (see `rows`).
    fn paint_rows(&mut self, bounds: Bounds<Pixels>, prepaint: &mut PrepaintState, focused: bool, window: &mut Window, cx: &mut App) {
        let line_height = window.line_height();
        let block = prepaint.block.take().filter(|_| focused);
        let caret = prepaint.caret.take().filter(|_| focused);
        let rows = std::mem::take(&mut prepaint.rows);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in prepaint.fills.drain(..) {
                window.paint_quad(quad);
            }
            for (_, line, origin) in &rows {
                if let Err(e) = line.paint(*origin, line_height, gpui::TextAlign::Left, None, window, cx) {
                    tracing::debug!("input paint failed: {e}");
                }
            }
            if let Some(block) = block {
                let cell = block.bounds;
                window.paint_quad(block);
                if let Some((text, at)) = prepaint.reversed.take() {
                    window.with_content_mask(Some(ContentMask { bounds: cell }), |window| {
                        if let Err(e) = text.paint(at, line_height, gpui::TextAlign::Left, None, window, cx) {
                            tracing::debug!("input paint failed: {e}");
                        }
                    });
                }
            }
        });
        // Outside the mask, as on one line: at a row's start the bar sits
        // just before the text column.
        if let Some(caret) = caret {
            window.paint_quad(caret);
        }
        self.input.update(cx, |input, _cx| {
            input.last_layout = None;
            input.last_bounds = Some(bounds);
            input.last_rows = rows;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One label per mode, each in its own role at the text's own size
        // and baseline: INSERT is the quiet default, NORMAL the theme's
        // accent (keys there edit, not type), and the ex line's `:` reads as
        // part of the command, flush against it, as vim's prompt does.
        let p = self.palette.as_ref();
        let (label, color, gap) = match self.vim.mode {
            Mode::Insert => ("INSERT", p.map(|p| p.text_dim), true),
            Mode::Normal => ("NORMAL", p.map(|p| p.tertiary), true),
            Mode::Ex => (":", p.map(|p| p.text), false),
        };
        let cell = self.metrics.map_or(px(8.), |m| px(m.cell));
        // A note under the text, on its column: a hint to confirm a big
        // send, in the placeholder's role, or why it can't go, in the
        // error role. One row, cut to fit.
        let note = self.note.as_ref().map(|note| match note {
            Note::Error(text) => (text.clone(), p.map(|p| p.error)),
            Note::Confirm(text) => (text.clone(), p.map(|p| p.hint)),
        });
        let popup = self.popup.clone().zip(p.cloned());
        let picker = self.picker.as_ref().zip(p).map(|(picker, p)| self.picker_box(picker, p));
        // The status (a marked line) reads on from the mode label, on the
        // text's row; the text follows it. In a narrow window it is cut
        // with `…` to most of the row, so it never runs past the input.
        let status = self.status.clone().map(|text| {
            div()
                .flex_none()
                .whitespace_nowrap()
                .when(self.width > 0., |d| d.max_w(px(self.width * 0.75)).overflow_hidden().text_ellipsis())
                .mr(cell * 2.)
                .when_some(p.map(|p| p.text_dim), |d, c| d.text_color(c))
                .child(text)
        });
        let text = div().flex().min_w(px(0.)).children(status).child(div().flex_1().min_w(px(0.)).child(TextElement { input: cx.entity() }));
        div()
            .relative()
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .capture_key_down(cx.listener(Self::on_key_down))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .w(px(self.column))
                    .child(div().when_some(color, |d, c| d.text_color(c)).when(gap, |d| d.mr(cell)).child(label)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(text)
                    .when_some(note, |d, (text, color)| d.child(div().when_some(color, |d, c| d.text_color(c)).truncate().child(text))),
            )
            // The popup sits above the input, over the message view; `deferred`
            // paints it after the rest of the window.
            .when_some(popup, |d, (popup, p)| {
                let rows = popup.rows.iter().enumerate().map(|(i, (emoji, name))| {
                    let row = div().flex().gap(cell).px(cell).child(*emoji).child(div().text_color(p.text_dim).child(format!(":{name}:")));
                    if i == popup.selected { row.bg(p.selected).text_color(p.selected_text) } else { row.text_color(p.text) }
                });
                d.child(
                    deferred(
                        div()
                            .absolute()
                            .bottom_full()
                            .left(px(self.column))
                            .mb(px(4.))
                            .min_w(px(200.))
                            .bg(p.input_bg)
                            .border_1()
                            .border_color(p.border)
                            .rounded_sm()
                            .py(px(2.))
                            .flex()
                            .flex_col()
                            .children(rows),
                    )
                    .with_priority(1),
                )
            })
            .when_some(picker, |d, picker| d.child(deferred(picker).with_priority(1)))
    }
}

impl TextInput {
    /// The react picker, above the input on the text column as the emoji
    /// popup is: `react`, the quick row (each emoji over its digit), the
    /// search line, then what the search found. Every row is a line high.
    fn picker_box(&self, picker: &Picker, p: &Palette) -> gpui::Div {
        let cell = self.metrics.map_or(px(8.), |m| px(m.cell));
        let line = self.metrics.map_or(px(16.), |m| px(m.line));
        let searching = !picker.query.is_empty();
        let quick = picker.quick.iter().enumerate().map(|(i, emoji)| {
            let digit = div().h(line).text_color(p.quiet).child(format!("{}", i + 1));
            let it = div().flex().flex_col().items_center().min_w(cell * 3.).child(div().h(line).child(emoji.clone())).child(digit);
            if !searching && i == picker.selected { it.bg(p.selected).text_color(p.selected_text) } else { it.text_color(p.text) }
        });
        let query = format!(":{}█", picker.query.trim_start_matches(':'));
        let rows = picker.rows.iter().enumerate().map(|(i, (emoji, name))| {
            let row = div().flex().h(line).gap(cell).px(cell).child(*emoji).child(div().text_color(p.text_dim).child(format!(":{name}:")));
            if searching && i == picker.selected { row.bg(p.selected).text_color(p.selected_text) } else { row.text_color(p.text) }
        });
        // The quick row is 26 cells wide (8 of 3, and its padding); where
        // the text column has less room than that, the box sits against
        // the input's right edge instead, so none of it is cut off.
        let fits = self.width <= 0. || px(self.width) >= cell * 26. + px(2.);
        div()
            .absolute()
            .bottom_full()
            .when(fits, |d| d.left(px(self.column)))
            .when(!fits, |d| d.right_0())
            .mb(px(4.))
            .min_w(px(200.))
            .bg(p.input_bg)
            .border_1()
            .border_color(p.border)
            .rounded_sm()
            .py(px(2.))
            .flex()
            .flex_col()
            .child(div().h(line).px(cell).text_color(p.quiet).child("react"))
            .child(div().flex().px(cell).children(quick))
            .child(div().h(line).px(cell).text_color(p.text).child(query))
            .children(rows)
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl TextInput {
    /// Snapshot scripts: same path as the platform's text input (ignored in Normal mode).
    #[cfg(feature = "snapshot")]
    pub fn type_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, text, window, cx)
    }

    pub fn new(placeholder: &str, cx: &mut Context<Self>) -> TextInput {
        TextInput {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.to_owned().into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            last_rows: Vec::new(),
            scroll_x: px(0.),
            scroll_row: 0,
            is_selecting: false,
            vim: Vim::default(),
            palette: None,
            metrics: None,
            column: 0.,
            width: 0.,
            note: None,
            status: None,
            cycle: None,
            popup: None,
            dismissed: None,
            picker: None,
        }
    }

    /// Show `next` (another window's draft) and hand back this window's.
    pub fn swap_draft(&mut self, next: String, cx: &mut Context<Self>) -> String {
        let draft = self.vim.switch_line().unwrap_or_else(|| self.content.to_string());
        self.reset();
        let end = if self.vim.mode == Mode::Normal { vim::clamp_normal(&next, next.len()) } else { next.len() };
        self.content = next.into();
        self.selected_range = end..end;
        self.refresh_popup();
        cx.notify();
        draft
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() {
            self.pick(PickKey::Enter, cx);
            return;
        }
        // With the popup open, Enter takes the chosen emoji instead.
        if let Some(p) = self.popup.take() {
            let (text, cursor) = p.accept(&self.content, self.cursor_offset());
            self.edited();
            self.content = text.into();
            self.selected_range = cursor..cursor;
            self.selection_reversed = false;
            self.marked_range = None;
            self.refresh_popup();
            cx.notify();
            return;
        }
        // Key bindings run before `capture_key_down`, so route through vim here.
        if self.vim_key(named_key(Named::Enter), cx) {
            return;
        }
        self.submit(cx);
    }

    /// Hand the text to the app, which clears it once it goes (`sent`)
    /// or holds it with a note (`hold`). A second Enter on a confirm note
    /// confirms.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.content.to_string();
        if text.trim().is_empty() {
            return;
        }
        let confirmed = matches!(self.note, Some(Note::Confirm(_)));
        cx.emit(Submit { text, confirmed });
        cx.notify();
    }

    /// Start a line with `prefix` (a reply's `nick: `), unless it starts
    /// with it already, and type on after it in Insert mode.
    pub fn begin(&mut self, prefix: &str, cx: &mut Context<Self>) {
        if !self.content.starts_with(prefix) {
            self.content = format!("{prefix}{}", self.content).into();
        }
        self.vim.reset();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.edited();
        self.refresh_popup();
        cx.notify();
    }

    /// Open the react picker over the input with `quick` on its quick row.
    /// Keys go to it until an emoji is picked or it is closed, which it
    /// says with a `PickerEvent`.
    pub fn open_picker(&mut self, quick: Vec<String>, cx: &mut Context<Self>) {
        self.picker = Some(Picker::new(quick));
        cx.notify();
    }

    /// A key for the open picker; a pick or a close puts it away.
    fn pick(&mut self, key: PickKey, cx: &mut Context<Self>) {
        let Some(picker) = self.picker.as_mut() else { return };
        match picker.key(key) {
            Picked::Stay => {}
            Picked::Emoji(emoji) => {
                self.picker = None;
                cx.emit(PickerEvent(Some(emoji)));
            }
            Picked::Close => {
                self.picker = None;
                cx.emit(PickerEvent(None));
            }
        }
        cx.notify();
    }

    /// What was submitted went: start a fresh line.
    pub fn sent(&mut self, cx: &mut Context<Self>) {
        self.reset();
        self.vim.reset();
        self.refresh_popup();
        cx.notify();
    }

    /// What was submitted stays, with `note` under it.
    pub fn hold(&mut self, note: Note, cx: &mut Context<Self>) {
        self.note = Some(note);
        cx.notify();
    }

    /// Keys that are not bound to an action. Runs in the capture phase so
    /// Normal-mode keys never reach the IME.
    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // Enter and Backspace arrive as actions; alt/cmd chords are the app's.
        if ks.modifiers.alt || ks.modifiers.platform || ks.modifiers.function || matches!(ks.key.as_str(), "enter" | "backspace") {
            return;
        }
        // With the picker open, every key is its own.
        if self.picker.is_some() {
            if let Some(key) = pick_key(ks.key.as_str(), ks.key_char.as_deref(), ks.modifiers.shift, ks.modifiers.control) {
                self.pick(key, cx);
            }
            cx.stop_propagation();
            return;
        }
        // With the popup open, Tab and the arrows choose and Esc closes it.
        if self.popup.is_some() {
            let step = match (ks.key.as_str(), ks.modifiers.control) {
                ("tab", false) if ks.modifiers.shift => Some(-1),
                ("tab", false) | ("down", false) | ("n", true) => Some(1),
                ("up", false) | ("p", true) => Some(-1),
                _ => None,
            };
            if let Some(by) = step {
                if let Some(p) = self.popup.as_mut() {
                    p.step(by);
                }
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if ks.key == "escape" {
                self.dismissed = self.popup.take().map(|p| p.start);
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        // Tab completes in Insert mode.
        if ks.key == "tab" && !ks.modifiers.shift && !ks.modifiers.control && self.vim.mode == Mode::Insert {
            cx.emit(WantTab);
            cx.stop_propagation();
            return;
        }
        let Some(key) = to_key(ks.key.as_str(), ks.key_char.as_deref(), ks.modifiers.shift, ks.modifiers.control) else { return };
        if self.vim_key(key, cx) {
            cx.stop_propagation();
        }
    }

    /// Run a key through vim and apply the effects. True if vim consumed it.
    fn vim_key(&mut self, key: Key, cx: &mut Context<Self>) -> bool {
        // Esc puts a note away (a confirm is cancelled), whatever the mode.
        if key.named == Some(Named::Escape) && self.note.take().is_some() {
            cx.notify();
        }
        let cursor = clamp_range(&self.content, &(self.cursor_offset()..self.cursor_offset())).start;
        let effects = self.vim.key(&key, &self.content, cursor);
        if effects == [Effect::Pass] {
            return false;
        }
        for effect in effects {
            match effect {
                Effect::Pass => {}
                Effect::Edit { text, cursor } => {
                    if text != self.content.as_ref() {
                        self.edited();
                    }
                    self.content = text.into();
                    self.selected_range = cursor..cursor;
                    self.selection_reversed = false;
                    self.marked_range = None;
                }
                Effect::Submit => self.submit(cx),
                Effect::Scroll(_) | Effect::Nav(_) | Effect::Ex(_) | Effect::Dismiss | Effect::Pick(_) | Effect::Cancel => cx.emit(VimEvent(effect)),
            }
        }
        self.refresh_popup();
        cx.notify();
        true
    }
}

/// Convert a gpui keystroke to vim's key. `None` for keys vim ignores.
fn to_key(key: &str, key_char: Option<&str>, shift: bool, ctrl: bool) -> Option<Key> {
    let named = match key {
        "escape" => Some(Named::Escape),
        "enter" => Some(Named::Enter),
        "backspace" => Some(Named::Backspace),
        _ => None,
    };
    if named.is_some() {
        return Some(Key { ch: None, named, ctrl });
    }
    let single = |s: &str| {
        let mut it = s.chars();
        it.next().filter(|_| it.next().is_none())
    };
    let from_key = || single(key).map(|c| if shift { c.to_uppercase().next().unwrap_or(c) } else { c });
    // With ctrl, `key_char` can be a control character; use the key itself.
    let ch = if ctrl { from_key()? } else { key_char.and_then(single).or_else(from_key)? };
    Some(Key { ch: Some(ch), named: None, ctrl })
}

/// A gpui keystroke as the react picker takes it. `None` for keys it
/// ignores (other ctrl chords, keys that type nothing).
fn pick_key(key: &str, key_char: Option<&str>, shift: bool, ctrl: bool) -> Option<PickKey> {
    match (key, ctrl) {
        ("escape", _) => Some(PickKey::Escape),
        ("enter", _) => Some(PickKey::Enter),
        ("backspace", _) => Some(PickKey::Backspace),
        ("tab", false) if shift => Some(PickKey::Prev),
        ("tab", false) | ("down", false) | ("n", true) => Some(PickKey::Next),
        ("up", false) | ("p", true) => Some(PickKey::Prev),
        (_, true) => None,
        _ => {
            let mut it = key_char?.chars();
            it.next().filter(|_| it.next().is_none()).map(PickKey::Char)
        }
    }
}

fn named_key(n: Named) -> Key {
    Key { ch: None, named: Some(n), ctrl: false }
}

fn next_char_boundary(s: &str, at: usize) -> usize {
    s.get(at..).and_then(|t| t.chars().next()).map_or(s.len(), |c| at + c.len_utf8())
}

/// A vim effect the app must handle (scroll, window nav, ex line).
pub struct VimEvent(pub vim::Effect);

impl EventEmitter<VimEvent> for TextInput {}

/// Tab in Insert mode: the app answers with `tab`, giving the nicks to
/// offer here (it knows the window; the input doesn't).
pub struct WantTab;

impl EventEmitter<WantTab> for TextInput {}

/// The text Enter submitted, and whether it confirms a note asking to.
pub struct Submit {
    pub text: String,
    pub confirmed: bool,
}

impl EventEmitter<Submit> for TextInput {}

/// The react picker picked an emoji (`Some`) or was closed (`None`).
pub struct PickerEvent(pub Option<String>);

impl EventEmitter<PickerEvent> for TextInput {}

/// Keys for every `TextInput`. Call once at startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", Enter, Some("TextInput")),
        KeyBinding::new("shift-enter", Newline, Some("TextInput")),
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("cmd-a", SelectAll, Some("TextInput")),
        KeyBinding::new("cmd-v", Paste, Some("TextInput")),
        KeyBinding::new("cmd-c", Copy, Some("TextInput")),
        KeyBinding::new("cmd-x", Cut, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some("TextInput")),
    ]);
}

/// UTF-16 offset in `s` to a UTF-8 byte offset, clamped to `s`.
fn utf16_to_utf8(s: &str, offset: usize) -> usize {
    let (mut utf8, mut utf16) = (0, 0);
    for ch in s.chars() {
        if utf16 >= offset {
            break;
        }
        utf16 += ch.len_utf16();
        utf8 += ch.len_utf8();
    }
    utf8
}

/// UTF-8 byte offset in `s` to a UTF-16 offset.
fn utf8_to_utf16(s: &str, offset: usize) -> usize {
    let (mut utf16, mut utf8) = (0, 0);
    for ch in s.chars() {
        if utf8 >= offset {
            break;
        }
        utf8 += ch.len_utf8();
        utf16 += ch.len_utf16();
    }
    utf16
}

/// Pull a range back inside `s` and onto char boundaries, so slicing can't panic.
fn clamp_range(s: &str, r: &Range<usize>) -> Range<usize> {
    let fix = |mut i: usize| {
        i = i.min(s.len());
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let start = fix(r.start);
    start..fix(r.end).max(start)
}

/// Colour spans over `len` bytes: `base` everywhere, with each of `over`
/// laid on top in order (later wins). Adjacent equal spans merge, so the
/// line is shaped in as few runs as possible.
fn spans<C: std::marker::Copy + PartialEq>(len: usize, base: C, over: &[(Range<usize>, C)]) -> Vec<(Range<usize>, C)> {
    let mut cuts: Vec<usize> = over.iter().flat_map(|(r, _)| [r.start.min(len), r.end.min(len)]).chain([0, len]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<(Range<usize>, C)> = Vec::new();
    for w in cuts.windows(2) {
        let c = over.iter().rev().find(|(r, _)| r.start <= w[0] && w[1] <= r.end).map_or(base, |(_, c)| *c);
        match out.last_mut() {
            Some((r, last)) if *last == c => r.end = w[1],
            _ => out.push((w[0]..w[1], c)),
        }
    }
    out
}

/// The fewest cells a parked draft shows: less says nothing.
const PARKED_MIN_CHARS: usize = 4;

/// The end of `text` that fits `room`, given `width` of a string: all of
/// it, or its tail after a `…`; `None` when fewer than `PARKED_MIN_CHARS`
/// would show.
fn tail_fit(text: &str, room: f32, width: impl Fn(&str) -> f32) -> Option<String> {
    if width(text) <= room {
        return Some(text.to_owned());
    }
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let shown = starts.iter().map(|&i| format!("…{}", &text[i..])).find(|s| width(s) <= room)?;
    (shown.chars().count() > PARKED_MIN_CHARS).then_some(shown)
}

/// The first of `shown` rows to show of `total`, so the cursor's row (`at`)
/// shows: `prev` while it does, else just far enough to bring it in, and
/// never past the last row.
fn scroll_rows(prev: usize, at: usize, total: usize, shown: usize) -> usize {
    let most = total.saturating_sub(shown);
    let top = if at < prev {
        at
    } else if at >= prev + shown {
        at + 1 - shown
    } else {
        prev
    };
    top.min(most)
}

/// What a paste puts in: its line breaks as `\n` (from `\r\n` and `\r`
/// too), without those at its very end. The ex line is one line, so there
/// each break is a space.
fn pasted(text: &str, one_line: bool) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_end_matches('\n');
    if one_line { text.replace('\n', " ") } else { text.to_owned() }
}

/// `scroll` moved on to the next glyph edge (of `edges`, ascending) that
/// puts a glyph's start exactly `cell` from the view's left, where the
/// leading `…` mark ends; no scroll stays none.
fn snap_scroll(scroll: f32, edges: &[f32], cell: f32) -> f32 {
    if scroll <= 0.5 {
        return 0.;
    }
    edges.iter().map(|x| x - cell).find(|s| *s >= scroll - 0.01).unwrap_or(scroll).max(0.)
}

/// Where the trailing `…` mark starts (from the view's left), at the end
/// of the last glyph (of `edges`) that ends at least `cell` short of the
/// `view`'s right edge, scrolled by `scroll`.
fn trailing_mark(edges: &[f32], scroll: f32, view: f32, cell: f32) -> f32 {
    edges.iter().rev().map(|x| x - scroll).find(|x| *x <= view - cell + 0.01).unwrap_or(view - cell).max(0.)
}

/// Horizontal scroll offset that keeps the cursor inside a `view`-wide window
/// onto a `line_width`-wide line. Moves `prev` only when the cursor would leave
/// the visible span, leaving `margin` of context, and never scrolls past the
/// end of the line (plus `allowance` for the cursor itself).
fn scroll_for_cursor(prev: f32, cursor_x: f32, line_width: f32, view: f32, margin: f32, allowance: f32) -> f32 {
    let max = (line_width + allowance - view).max(0.);
    let wanted = if cursor_x < prev {
        (cursor_x - margin).max(0.)
    } else if cursor_x > prev + view - margin {
        cursor_x - (view - margin)
    } else {
        prev
    };
    wanted.max(0.).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_reach_the_picker_as_its_own() {
        assert_eq!(pick_key("escape", None, false, false), Some(PickKey::Escape));
        assert_eq!(pick_key("enter", None, false, false), Some(PickKey::Enter));
        assert_eq!(pick_key("backspace", None, false, false), Some(PickKey::Backspace));
        for (key, ctrl) in [("tab", false), ("down", false), ("n", true)] {
            assert_eq!(pick_key(key, None, false, ctrl), Some(PickKey::Next), "{key}");
        }
        for (key, shift, ctrl) in [("tab", true, false), ("up", false, false), ("p", false, true)] {
            assert_eq!(pick_key(key, None, shift, ctrl), Some(PickKey::Prev), "{key}");
        }
        assert_eq!(pick_key("2", Some("2"), false, false), Some(PickKey::Char('2')));
        assert_eq!(pick_key(";", Some(":"), true, false), Some(PickKey::Char(':')));
        // Other ctrl chords, and keys that type nothing, do nothing there.
        assert_eq!(pick_key("a", Some("\u{1}"), false, true), None);
        assert_eq!(pick_key("left", None, false, false), None);
    }

    #[test]
    fn a_parked_draft_gives_way_from_the_front() {
        let w = |s: &str| s.chars().count() as f32;
        assert_eq!(tail_fit("my draft", 8., w).as_deref(), Some("my draft"));
        assert_eq!(tail_fit("my draft", 6., w).as_deref(), Some("…draft"));
        // Too little room to say anything: nothing.
        assert_eq!(tail_fit("my draft", 4., w), None);
        assert_eq!(tail_fit("my draft", 0., w), None);
    }

    const VIEW: f32 = 100.;
    const MARGIN: f32 = 20.;
    const CURSOR_ALLOWANCE: f32 = 2.;

    #[test]
    fn a_scrolled_line_starts_on_a_whole_glyph_after_the_mark() {
        // A monospace line, 10px a glyph; the mark is one 10px cell.
        let edges: Vec<f32> = (0..=20).map(|i| i as f32 * 10.).collect();
        assert_eq!(snap_scroll(0., &edges, 10.), 0.);
        assert_eq!(snap_scroll(0.4, &edges, 10.), 0.);
        // Off the grid: on to the next glyph, which starts at the mark's end.
        assert_eq!(snap_scroll(33.7, &edges, 10.), 40.);
        assert_eq!(snap_scroll(40., &edges, 10.), 40.);
        // A proportional face: the next edge, whatever its width.
        let edges = [0., 7., 19., 24., 40.];
        assert_eq!(snap_scroll(5., &edges, 10.), 9.);
        assert_eq!(snap_scroll(10., &edges, 10.), 14.);
    }

    #[test]
    fn the_trailing_mark_starts_after_the_last_whole_glyph() {
        let edges: Vec<f32> = (0..=20).map(|i| i as f32 * 10.).collect();
        // A 95px view scrolled 40: glyph edges at 0, 10 … 80 fit before 85.
        assert_eq!(trailing_mark(&edges, 40., 95., 10.), 80.);
        assert_eq!(trailing_mark(&edges, 40., 100., 10.), 90.);
    }

    #[test]
    fn scroll_is_zero_when_line_fits() {
        assert_eq!(scroll_for_cursor(0., 50., 60., VIEW, MARGIN, CURSOR_ALLOWANCE), 0.);
        // a stale offset is pulled back once everything fits
        assert_eq!(scroll_for_cursor(30., 50., 60., VIEW, MARGIN, CURSOR_ALLOWANCE), 0.);
    }

    #[test]
    fn scroll_follows_typing_past_right_edge() {
        let s = scroll_for_cursor(0., 150., 300., VIEW, MARGIN, CURSOR_ALLOWANCE);
        assert_eq!(s, 150. - (VIEW - MARGIN));
        // cursor stays visible with margin to spare
        assert!(150. - s <= VIEW - MARGIN);
    }

    #[test]
    fn scroll_returns_to_zero_on_home() {
        assert_eq!(scroll_for_cursor(70., 0., 200., VIEW, MARGIN, CURSOR_ALLOWANCE), 0.);
    }

    #[test]
    fn scroll_moves_left_into_hidden_text() {
        // cursor at 60 while the view starts at 80: show it with margin context
        assert_eq!(scroll_for_cursor(80., 60., 200., VIEW, MARGIN, CURSOR_ALLOWANCE), 40.);
        // never below zero
        assert_eq!(scroll_for_cursor(80., 10., 200., VIEW, MARGIN, CURSOR_ALLOWANCE), 0.);
    }

    #[test]
    fn scroll_keeps_prev_while_cursor_visible() {
        assert_eq!(scroll_for_cursor(50., 100., 300., VIEW, MARGIN, CURSOR_ALLOWANCE), 50.);
    }

    #[test]
    fn scroll_clamps_after_deleting_text() {
        // line shrank from 300 to 130; no empty space may remain on the right
        let s = scroll_for_cursor(200., 130., 130., VIEW, MARGIN, CURSOR_ALLOWANCE);
        assert_eq!(s, 130. + CURSOR_ALLOWANCE - VIEW);
    }

    #[test]
    fn scroll_with_zero_width_view_does_not_panic() {
        let s = scroll_for_cursor(5., 40., 80., 0., MARGIN, CURSOR_ALLOWANCE);
        assert!(s.is_finite() && s >= 0.);
        let s = scroll_for_cursor(0., 0., 0., 0., 0., 0.);
        assert_eq!(s, 0.);
    }

    #[test]
    fn spans_layer_colours_over_the_line() {
        assert_eq!(spans(5, 'a', &[]), vec![(0..5, 'a')]);
        assert_eq!(spans(0, 'a', &[]), vec![]);
        // A command token, then the block cursor on byte 6.
        assert_eq!(spans(10, 'a', &[(0..3, 'd'), (6..7, 'b')]), vec![(0..3, 'd'), (3..6, 'a'), (6..7, 'b'), (7..10, 'a')]);
        // Later layers win; equal neighbours merge; ranges past the end clip.
        assert_eq!(spans(4, 'a', &[(0..4, 'd'), (1..2, 'b')]), vec![(0..1, 'd'), (1..2, 'b'), (2..4, 'd')]);
        assert_eq!(spans(4, 'a', &[(2..9, 'a')]), vec![(0..4, 'a')]);
        assert_eq!(spans(3, 'a', &[(3..4, 'b')]), vec![(0..3, 'a')]);
    }

    #[test]
    fn utf16_offsets_roundtrip() {
        let s = "aあ😀b";
        assert_eq!(utf16_to_utf8(s, 0), 0);
        assert_eq!(utf16_to_utf8(s, 2), 4);
        assert_eq!(utf16_to_utf8(s, 4), 8);
        assert_eq!(utf16_to_utf8(s, 99), s.len());
        assert_eq!(utf8_to_utf16(s, 4), 2);
        assert_eq!(utf8_to_utf16(s, s.len()), 5);
    }

    #[test]
    fn clamp_keeps_slicing_safe() {
        let s = "あk";
        assert_eq!(clamp_range(s, &(6..6)), 4..4);
        assert_eq!(clamp_range(s, &(1..2)), 0..0);
        assert_eq!(&s[clamp_range(s, &(0..99))], "あk");
    }

    #[test]
    fn ime_selection_is_relative_to_new_text() {
        // committed "あ" then composing "k": selection 1..1 in "k" is byte 3+1.
        assert_eq!(3 + utf16_to_utf8("k", 1), 4);
    }

    #[test]
    fn keystrokes_become_vim_keys() {
        let ch = |k: Option<Key>| k.and_then(|k| k.ch);
        assert_eq!(ch(to_key("g", Some("G"), true, false)), Some('G'));
        assert_eq!(ch(to_key("g", None, true, false)), Some('G'));
        assert_eq!(ch(to_key(";", Some(":"), true, false)), Some(':'));
        assert_eq!(ch(to_key("d", Some("\u{4}"), false, true)), Some('d'));
        assert!(to_key("d", Some("d"), false, true).is_some_and(|k| k.ctrl));
        assert_eq!(to_key("escape", None, false, false).and_then(|k| k.named), Some(Named::Escape));
        assert!(to_key("left", None, false, false).is_none());
    }
}
