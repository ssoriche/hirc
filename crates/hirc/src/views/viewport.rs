//! The message view's frame: it gives the line list a height of whole text
//! lines, so no row is ever cut, at every window height and font.
//!
//! What is left of a line (`extra`) is shared by the two bars that frame
//! the view, so the header and the input bar are always the same height
//! and centre their text: each takes `extra` (half above its text, half
//! below), and the view gives back half of it at each end, so its clear
//! space above the oldest line and below the newest stays equal. That is
//! the one way to also keep the window list on the same grid: its first
//! row on the header's line, and every row on a message row (the list
//! starts half of `extra` further down, as the header's text does).
//! `extra` is what is left of a line, or that less a line when more than
//! half is left (one more line shows), so the bars stay within a quarter
//! line of two lines tall, and the view's ends within a quarter line of
//! `pad`.
//!
//! It turns the wheel into whole-line steps on the same grid as the vim
//! keys. An optional `sticky` row is drawn over the list's top row, and an
//! optional `footer` takes the bottom row for itself (the list gives it
//! up).

use crate::scroll;
use gpui::{
    AnyElement, App, AvailableSpace, Bounds, DispatchPhase, Element, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId,
    ListState, Pixels, ScrollDelta, ScrollWheelEvent, Style, Window, point, px, relative, size,
};
use std::cell::Cell;
use std::rc::Rc;

pub struct Viewport {
    list: ListState,
    child: AnyElement,
    frame: Frame,
    /// Wheel movement not yet a whole line.
    wheel: Rc<Cell<f32>>,
    sticky: Option<AnyElement>,
    footer: Option<AnyElement>,
}

/// The grid the view keeps: `line` px lines with `pad` px clear above and
/// below, less what the bars take of `extra` (see the module docs): the
/// view sets `extra` from its height (with what the bars took last), and
/// asks for a redraw when it changes. `scale` is device px a logical px.
pub struct Frame {
    pub line: f32,
    pub pad: f32,
    pub scale: f32,
    pub extra: Rc<Cell<f32>>,
}

/// `child` is the list drawn from `list`; it must fill its parent
/// (`size_full`). `sticky`, one line tall, is drawn over the list's first
/// visible row; `footer`, one line tall, below its last.
pub fn viewport(list: ListState, child: AnyElement, frame: Frame, wheel: Rc<Cell<f32>>, sticky: Option<AnyElement>, footer: Option<AnyElement>) -> Viewport {
    Viewport { list, child, frame, wheel, sticky, footer }
}

/// What the bars share for a view that would be `base` tall if they took
/// none: what is left of a line once `pad` clears each end, or that less
/// a line when more than half a line is left.
pub fn extra(base: f32, line: f32, pad: f32) -> f32 {
    let spare = scroll::whole_lines(base - 2. * pad, line).1;
    if 2. * spare <= line { spare } else { spare - line }
}

/// `extra` split into what goes above and below a bar's text: halves, on
/// whole device px (`scale` px a logical px), any odd px below.
pub fn halves(extra: f32, scale: f32) -> (f32, f32) {
    let scale = if scale > 0. { scale } else { 1. };
    let above = (extra * scale / 2.).floor() / scale;
    (above, extra - above)
}

/// Whole lines in a view `height` tall once the bars have `extra`, and the
/// clear space above them: `pad`, less what the header took below its
/// text (and below them, `pad` less what the input bar took above its
/// text), so the oldest line sits on the window list's grid.
pub fn margins(height: f32, line: f32, pad: f32, extra: f32, scale: f32) -> (usize, f32) {
    let (above, below) = halves(extra, scale);
    let top = pad - below;
    (scroll::whole_lines(height - top - (pad - above), line).0, top)
}

impl IntoElement for Viewport {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Viewport {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let mut style = Style { flex_grow: 1., flex_shrink: 1., flex_basis: relative(0.).into(), ..Default::default() };
        style.size.width = relative(1.).into();
        style.min_size.height = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        let (line, pad, scale) = (self.frame.line, self.frame.pad, self.frame.scale);
        let height = f32::from(bounds.size.height);
        // With what the bars take back, the view and the bars' share stay
        // the same height, so this settles in one frame.
        let extra = extra(height + 2. * self.frame.extra.get(), line, pad);
        if (extra - self.frame.extra.get()).abs() > 0.01 {
            self.frame.extra.set(extra);
            cx.notify(window.current_view());
        }
        let (lines, above) = margins(height, line, pad, self.frame.extra.get(), scale);
        let rows = lines.saturating_sub(usize::from(self.footer.is_some()));
        let height = px(rows as f32 * line);
        let available = size(AvailableSpace::Definite(bounds.size.width), AvailableSpace::Definite(height));
        self.child.layout_as_root(available, window, cx);
        let origin = point(bounds.origin.x, bounds.origin.y + px(above));
        self.child.prepaint_at(origin, window, cx);
        let row = size(AvailableSpace::Definite(bounds.size.width), AvailableSpace::Definite(px(line)));
        if let Some(sticky) = &mut self.sticky {
            sticky.layout_as_root(row, window, cx);
            sticky.prepaint_at(origin, window, cx);
        }
        if let Some(footer) = &mut self.footer {
            footer.layout_as_root(row, window, cx);
            footer.prepaint_at(origin + point(px(0.), height), window, cx);
        }
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        if let Some(sticky) = &mut self.sticky {
            sticky.paint(window, cx);
        }
        if let Some(footer) = &mut self.footer {
            footer.paint(window, cx);
        }
        let (list, line, wheel, hitbox, view) = (self.list.clone(), self.frame.line, self.wheel.clone(), hitbox.clone(), window.current_view());
        // Capture phase, so the list's own pixel scrolling never runs.
        window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture || !hitbox.should_handle_scroll(window) {
                return;
            }
            let px_y = match ev.delta {
                ScrollDelta::Pixels(p) => f32::from(p.y),
                ScrollDelta::Lines(l) => l.y * line,
            };
            let mut acc = wheel.get();
            let lines = scroll::wheel_lines(&mut acc, px_y, line);
            wheel.set(acc);
            scroll::by_lines(&list, lines, line);
            cx.stop_propagation();
            cx.notify(view);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bars_share_what_is_left_and_stay_one_height() {
        // 3 px over whole lines: each bar takes 3, the view gives back 1.5
        // at each end.
        assert_eq!(extra(10. * 21. + 20. + 3., 21., 10.), 3.);
        // An exact fit: nothing.
        assert_eq!(extra(10. * 21. + 20., 21., 10.), 0.);
        // More than half a line left: one more line, and the bars give.
        assert_eq!(extra(10. * 21. + 20. + 19., 21., 10.), -2.);
        // Halves on whole device px; an odd one goes below.
        assert_eq!(halves(3., 2.), (1.5, 1.5));
        assert_eq!(halves(3., 1.), (1., 2.));
        assert_eq!(halves(-3., 1.), (-2., -1.));
        assert_eq!(halves(0., 2.), (0., 0.));
    }

    #[test]
    fn every_height_keeps_the_grid_and_equal_ends() {
        let (line, pad, bar) = (21., 10., 42.);
        for h in 300..500 {
            // The window: header and input bar (two lines, plus `extra`
            // each) around the view.
            let total = h as f32;
            let e = extra(total - 2. * bar, line, pad);
            assert!(e.abs() <= line / 2., "{h}: {e}");
            let view = total - 2. * (bar + e);
            let (lines, top) = margins(view, line, pad, e, 2.);
            let bottom = view - top - lines as f32 * line;
            // Equal clear space at both ends, a quarter line from `pad`.
            assert!((top - bottom).abs() < 1e-3 && (top - pad).abs() <= line / 4. + 1e-3, "{h}: {top} {bottom}");
            // The header's text (`pad` plus half of `e` down) and the first
            // message line sit on one grid: the window list's.
            let (above, _) = halves(e, 2.);
            let first = bar + e + top;
            let grid = pad + above;
            let k = (first - grid) / line;
            assert!((k - k.round()).abs() < 1e-3, "{h}: {first} vs {grid}");
        }
    }
}
