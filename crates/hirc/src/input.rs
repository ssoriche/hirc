//! The message input: a single-line text field with IME, selection and
//! clipboard. Adapted from gpui's `examples/input.rs` (Apache-2.0,
//! Zed Industries). Enter emits `Submit` with the text and clears it.

use std::ops::Range;

use crate::metrics::{Metrics, Shape};
use crate::theme::Palette;
use crate::vim::{self, Effect, Key, Mode, Named, Vim};
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px, relative, size,
};
use hirc_core::command;
use unicode_segmentation::*;

actions!(text_input, [Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, ShowCharacterPalette, Paste, Cut, Copy, Enter,]);

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
    /// Horizontal scroll of the text, recomputed on every prepaint.
    scroll_x: Pixels,
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
}

impl TextInput {
    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
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
        if self.vim.mode == Mode::Normal {
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
            self.replace_text_in_range(None, &text.replace("\n", " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            let sel = clamp_range(&self.content, &self.selected_range);
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[sel].to_string()));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
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
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed { self.selected_range.start } else { self.selected_range.end }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
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
        self.scroll_x = px(0.);
        self.is_selecting = false;
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
        // Normal mode never inserts text.
        if self.vim.mode == Mode::Normal {
            return;
        }
        let range =
            range_utf16.as_ref().map(|range_utf16| self.range_from_utf16(range_utf16)).or(self.marked_range.clone()).unwrap_or(self.selected_range.clone());
        let range = clamp_range(&self.content, &range);

        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
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
        if self.vim.mode == Mode::Normal {
            return;
        }
        let range =
            range_utf16.as_ref().map(|range_utf16| self.range_from_utf16(range_utf16)).or(self.marked_range.clone()).unwrap_or(self.selected_range.clone());
        let range = clamp_range(&self.content, &range);

        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
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

        cx.notify();
    }

    fn bounds_for_range(&mut self, range_utf16: Range<usize>, bounds: Bounds<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let x = |i: usize| bounds.left() - self.scroll_x + last_layout.x_for_index(i.saturating_sub(self.hidden()));
        Some(Bounds::from_corners(point(x(range.start), bounds.top()), point(x(range.end), bounds.bottom())))
    }

    fn character_index_for_point(&mut self, point: gpui::Point<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
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
    /// The Normal-mode block, drawn under the text (its glyph is inverted).
    block: Option<PaintQuad>,
    /// The Insert/ex bar, drawn over the text.
    caret: Option<PaintQuad>,
    scroll_x: Pixels,
    selection: Option<PaintQuad>,
    /// `…` at an edge with text scrolled out of view past it, over a
    /// background-coloured cell.
    marks: Vec<(PaintQuad, Pixels, ShapedLine)>,
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
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
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

        // Colours over the line: a recognised `/command` takes the code
        // colour (so a typo, left in text, shows before Enter), marked
        // (IME) text is underlined, and the Normal block's glyph is drawn
        // in the background colour, as a terminal's reverse video.
        let code = input.palette.as_ref().map_or(dim, |p| p.code);
        let mut over = Vec::new();
        if input.vim.mode != Mode::Ex
            && let Some(cmd) = command::leading(&shown)
        {
            over.push((0..cmd.len() + 1, (code, false)));
        }
        if let Some(marked) = &input.marked_range {
            over.push((shift(marked.start)..shift(marked.end), (text, true)));
        }
        if normal && selected_range.is_empty() {
            over.push((cursor..next_char_boundary(&shown, cursor), (bg, false)));
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
        // The placeholder starts on the text column, where typing will:
        // the bar sits in the gap before it, as before any glyph, and under
        // the Normal block its first glyph is reversed, as typed text's is.
        let under_block = normal && input.focus_handle.is_focused(window);
        let placeholder = (input.content.is_empty() && !input.placeholder.is_empty()).then(|| {
            let first = if under_block { next_char_boundary(&input.placeholder, 0) } else { 0 };
            let runs: Vec<TextRun> = [(first, bg), (input.placeholder.len() - first, hint)]
                .into_iter()
                .filter(|(len, _)| *len > 0)
                .map(|(len, color)| TextRun { len, font: style.font(), color, background_color: None, underline: None, strikethrough: None })
                .collect();
            window.text_system().shape_line(input.placeholder.clone(), font_size, &runs, None)
        });
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
        let (mut block, mut caret, mut selection) = (None, None, None);
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
        } else {
            // A bar's width clear of the glyph beside it: after the glyph
            // before it, or, at the start of an Insert line (where there is
            // none, and the placeholder's first glyph may follow), before
            // the text column, in the cell's gap after the mode label. The
            // ex line's `:` is that glyph before it, flush to the column.
            let gap = px(m.caret);
            let x = if cursor == 0 && input.vim.mode == Mode::Insert { origin_x - gap - px(m.caret) } else { origin_x + cursor_pos + gap };
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
        self.input.update(cx, |input, _cx| input.scroll_x = scroll_x);
        PrepaintState { line: Some(line), placeholder, block, caret, scroll_x, selection, marks }
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
            for quad in [prepaint.selection.take(), block].into_iter().flatten() {
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
        div()
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .capture_key_down(cx.listener(Self::on_key_down))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::enter))
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
            .child(div().flex_1().min_w(px(0.)).child(TextElement { input: cx.entity() }))
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
            scroll_x: px(0.),
            is_selecting: false,
            vim: Vim::default(),
            palette: None,
            metrics: None,
            column: 0.,
        }
    }

    /// Show `next` (another window's draft) and hand back this window's.
    pub fn swap_draft(&mut self, next: String, cx: &mut Context<Self>) -> String {
        let draft = self.vim.switch_line().unwrap_or_else(|| self.content.to_string());
        self.reset();
        let end = if self.vim.mode == Mode::Normal { vim::clamp_normal(&next, next.len()) } else { next.len() };
        self.content = next.into();
        self.selected_range = end..end;
        cx.notify();
        draft
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        // Key bindings run before `capture_key_down`, so route through vim here.
        if self.vim_key(named_key(Named::Enter), cx) {
            return;
        }
        self.submit(cx);
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.content.to_string();
        if text.trim().is_empty() {
            return;
        }
        self.reset();
        self.vim.reset();
        cx.emit(Submit(text));
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
        let Some(key) = to_key(ks.key.as_str(), ks.key_char.as_deref(), ks.modifiers.shift, ks.modifiers.control) else { return };
        if self.vim_key(key, cx) {
            cx.stop_propagation();
        }
    }

    /// Run a key through vim and apply the effects. True if vim consumed it.
    fn vim_key(&mut self, key: Key, cx: &mut Context<Self>) -> bool {
        let cursor = clamp_range(&self.content, &(self.cursor_offset()..self.cursor_offset())).start;
        let effects = self.vim.key(&key, &self.content, cursor);
        if effects == [Effect::Pass] {
            return false;
        }
        for effect in effects {
            match effect {
                Effect::Pass => {}
                Effect::Edit { text, cursor } => {
                    self.content = text.into();
                    self.selected_range = cursor..cursor;
                    self.selection_reversed = false;
                    self.marked_range = None;
                }
                Effect::Submit => self.submit(cx),
                Effect::Scroll(_) | Effect::Nav(_) | Effect::Ex(_) | Effect::Dismiss => cx.emit(VimEvent(effect)),
            }
        }
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

fn named_key(n: Named) -> Key {
    Key { ch: None, named: Some(n), ctrl: false }
}

fn next_char_boundary(s: &str, at: usize) -> usize {
    s.get(at..).and_then(|t| t.chars().next()).map_or(s.len(), |c| at + c.len_utf8())
}

/// A vim effect the app must handle (scroll, window nav, ex line).
pub struct VimEvent(pub vim::Effect);

impl EventEmitter<VimEvent> for TextInput {}

pub struct Submit(pub String);

impl EventEmitter<Submit> for TextInput {}

/// Keys for every `TextInput`. Call once at startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", Enter, Some("TextInput")),
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
