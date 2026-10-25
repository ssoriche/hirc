//! The message view's frame: it gives the line list a height of whole text
//! lines, so no row is ever cut, with `pad` clear above, under the title
//! bar, at every window height (as a terminal's rows start at its top),
//! and at least `pad` below, where the part of a line that does not fit
//! is left. It turns
//! the wheel into whole-line steps on the same grid as the vim keys. An
//! optional `sticky` row is drawn over the list's top row, and an optional
//! `footer` takes the bottom row for itself (the list gives it up).

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
    line: f32,
    pad: f32,
    /// Wheel movement not yet a whole line.
    wheel: Rc<Cell<f32>>,
    sticky: Option<AnyElement>,
    footer: Option<AnyElement>,
}

/// `child` is the list drawn from `list`; it must fill its parent
/// (`size_full`). At least `pad` px stay clear above and below. `sticky`,
/// one line tall, is drawn over the list's first visible row; `footer`,
/// one line tall, below its last.
pub fn viewport(
    list: ListState,
    child: AnyElement,
    line: f32,
    pad: f32,
    wheel: Rc<Cell<f32>>,
    sticky: Option<AnyElement>,
    footer: Option<AnyElement>,
) -> Viewport {
    Viewport { list, child, line, pad, wheel, sticky, footer }
}

/// Clear space above and below `lines` whole lines in `height`: `pad`
/// above, always, and `pad` plus what is left of a line below.
pub fn margins(height: f32, line: f32, pad: f32) -> (usize, f32, f32) {
    let (lines, spare) = scroll::whole_lines(height - 2. * pad, line);
    (lines, pad, pad + spare)
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
        let (lines, above, _) = margins(f32::from(bounds.size.height), self.line, self.pad);
        let rows = lines.saturating_sub(usize::from(self.footer.is_some()));
        let height = px(rows as f32 * self.line);
        let available = size(AvailableSpace::Definite(bounds.size.width), AvailableSpace::Definite(height));
        self.child.layout_as_root(available, window, cx);
        let origin = point(bounds.origin.x, bounds.origin.y + px(above));
        self.child.prepaint_at(origin, window, cx);
        let row = size(AvailableSpace::Definite(bounds.size.width), AvailableSpace::Definite(px(self.line)));
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
        let (list, line, wheel, hitbox, view) = (self.list.clone(), self.line, self.wheel.clone(), hitbox.clone(), window.current_view());
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
}
