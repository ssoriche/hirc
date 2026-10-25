//! The message view's frame: it gives the line list a height of whole text
//! lines, so no row is ever cut, with `pad` clear above, under the title
//! bar, and `pad` clear below, over the input bar, at every window height
//! and font. What is left of a line does not fit the grid; it goes to the
//! input bar (`spare`), which centres its text in it.
//!
//! Why the input bar: the window list's first row shares the header's
//! line, and its rows share the message rows' lines, so the header and
//! everything above the newest message sit on one grid from the top of
//! the window. Space left over can then only go below the view without
//! moving some row off that grid; given to the header, it would part the
//! header's line from the list's first row, and shared out above and
//! below the view, it would part the list's rows from the messages'.
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
/// below, and `spare`, what is left of a line, which the input bar takes:
/// the view sets it from its height (with what the bar took last), and
/// asks for a redraw when it changes.
pub struct Frame {
    pub line: f32,
    pub pad: f32,
    pub spare: Rc<Cell<f32>>,
}

/// `child` is the list drawn from `list`; it must fill its parent
/// (`size_full`). `sticky`, one line tall, is drawn over the list's first
/// visible row; `footer`, one line tall, below its last.
pub fn viewport(list: ListState, child: AnyElement, frame: Frame, wheel: Rc<Cell<f32>>, sticky: Option<AnyElement>, footer: Option<AnyElement>) -> Viewport {
    Viewport { list, child, frame, wheel, sticky, footer }
}

/// Clear space above and below `lines` whole lines in `height`: `pad`
/// above, always, and `pad` plus what is left of a line below (none,
/// once the input bar has taken it).
pub fn margins(height: f32, line: f32, pad: f32) -> (usize, f32, f32) {
    let (lines, spare) = scroll::whole_lines(height - 2. * pad, line);
    (lines, pad, pad + spare)
}

/// `spare` split into what goes above and below a bar's text: halves, on
/// whole device px (`scale` px a logical px), any odd px below.
pub fn split(spare: f32, scale: f32) -> (f32, f32) {
    let scale = if scale > 0. { scale } else { 1. };
    let above = (spare * scale / 2.).floor() / scale;
    (above, spare - above)
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
        let (line, pad) = (self.frame.line, self.frame.pad);
        let height = f32::from(bounds.size.height);
        // With what the input bar takes back, the view and the bar's share
        // stay the same height, so this settles in one frame.
        let spare = scroll::whole_lines(height + self.frame.spare.get() - 2. * pad, line).1;
        if (spare - self.frame.spare.get()).abs() > 0.01 {
            self.frame.spare.set(spare);
            cx.notify(window.current_view());
        }
        let (lines, above, _) = margins(height, line, pad);
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
    fn the_top_margin_never_changes_and_the_rest_goes_below() {
        // 3 px over 10 whole lines of 21: all of it below.
        assert_eq!(margins(10. * 21. + 20. + 3., 21., 10.), (10, 10., 13.));
        // An exact fit leaves just the pad.
        assert_eq!(margins(10. * 21. + 20., 21., 10.), (10, 10., 10.));
        // Every height: the pad above, and less than a line more below.
        for h in 300..400 {
            let (_, above, below) = margins(h as f32, 21., 10.);
            assert!(above == 10. && (10. ..31.).contains(&below), "{h}");
        }
    }

    #[test]
    fn the_input_bar_takes_the_spare_and_the_view_is_whole_lines() {
        // Whatever the height, once the bar has taken what is left of a
        // line, the view is whole lines with `pad` above and below.
        for h in 300..400 {
            let total = h as f32;
            let spare = scroll::whole_lines(total - 20., 21.).1;
            let (_, above, below) = margins(total - spare, 21., 10.);
            assert_eq!((above, below), (10., 10.), "{h}");
        }
        // Halves on whole device px; an odd one goes below.
        assert_eq!(split(3., 2.), (1.5, 1.5));
        assert_eq!(split(3., 1.), (1., 2.));
        assert_eq!(split(19., 2.), (9.5, 9.5));
        assert_eq!(split(0., 2.), (0., 0.));
    }
}
