//! The message input: a single-line text field with IME, selection and
//! clipboard. Adapted from gpui's `examples/input.rs` (Apache-2.0,
//! Zed Industries). Enter emits `Submit` with the text and clears it.

use std::ops::Range;

use crate::theme::Palette;
use crate::vim::{self, Effect, Key, Mode, Named, Vim};
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine,
    SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::*;

actions!(text_input, [Backspace, Delete, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End, ShowCharacterPalette, Paste, Cut, Copy, Enter,]);

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    vim: Vim,
    /// For the mode label; set by the app.
    pub palette: Option<Palette>,
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
        line.closest_index_for_x(position.x - bounds.left())
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
        Some(Bounds::from_corners(
            point(bounds.left() + last_layout.x_for_index(range.start), bounds.top()),
            point(bounds.left() + last_layout.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(&mut self, point: gpui::Point<Pixels>, _window: &mut Window, _cx: &mut Context<Self>) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;

        if last_layout.text != self.content {
            return None;
        }
        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
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
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let normal = input.vim.mode == Mode::Normal;
        let style = window.text_style();

        let (display_text, text_color) = if content.is_empty() { (input.placeholder.clone(), style.color.opacity(0.4)) } else { (content, style.color) };

        let run = TextRun { len: display_text.len(), font: style.font(), color: text_color, background_color: None, underline: None, strikethrough: None };
        let runs = if let Some(marked_range) = input.marked_range.as_ref() {
            vec![
                TextRun { len: marked_range.start, ..run.clone() },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle { color: Some(run.color), thickness: px(1.0), wavy: false }),
                    ..run.clone()
                },
                TextRun { len: display_text.len() - marked_range.end, ..run },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window.text_system().shape_line(display_text, font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            // Normal mode draws a translucent block over the char under the cursor.
            let (width, color) = if normal {
                let end = line.x_for_index(next_char_boundary(&line.text, cursor));
                (if end > cursor_pos { end - cursor_pos } else { font_size * 0.6 }, style.color.opacity(0.5))
            } else {
                (px(2.), style.color)
            };
            (None, Some(fill(Bounds::new(point(bounds.left() + cursor_pos, bounds.top()), size(width, bounds.bottom() - bounds.top())), color)))
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(selected_range.start), bounds.top()),
                        point(bounds.left() + line.x_for_index(selected_range.end), bounds.bottom()),
                    ),
                    style.color.opacity(0.25),
                )),
                None,
            )
        };
        PrepaintState { line: Some(line), cursor, selection }
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
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let Some(line) = prepaint.line.take() else {
            return;
        };
        if let Err(e) = line.paint(bounds.origin, window.line_height(), window, cx) {
            tracing::debug!("input paint failed: {e}");
        }

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = match self.vim.mode {
            Mode::Insert => "INSERT",
            Mode::Normal => "NORMAL",
            Mode::Ex => ":",
        };
        let dim = self.palette.as_ref().map(|p| p.text_dim);
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
            .child(div().flex_none().mr_2().text_xs().when_some(dim, |d, c| d.text_color(c)).child(label))
            .child(div().flex_1().child(TextElement { input: cx.entity() }))
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl TextInput {
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
            is_selecting: false,
            vim: Vim::default(),
            palette: None,
        }
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
                Effect::Ex(_) => {
                    self.reset();
                    cx.emit(VimEvent(effect));
                }
                Effect::Scroll(_) | Effect::Nav(_) => cx.emit(VimEvent(effect)),
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

#[cfg(test)]
mod tests {
    use super::*;

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
