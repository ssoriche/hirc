//! The message view's frame: it gives the line list a height of whole text
//! lines (the remainder becomes space above it, so no row is ever cut by
//! the title bar), keeps `bottom` clear above the input bar, and turns the
//! wheel into whole-line steps on the same grid as the vim keys.

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
    top: f32,
    bottom: f32,
    /// Wheel movement not yet a whole line.
    wheel: Rc<Cell<f32>>,
}

/// `child` is the list drawn from `list`; it must fill its parent
/// (`size_full`). At least `top` and `bottom` px stay clear above and below.
pub fn viewport(list: ListState, child: AnyElement, line: f32, top: f32, bottom: f32, wheel: Rc<Cell<f32>>) -> Viewport {
    Viewport { list, child, line, top, bottom, wheel }
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
        let (lines, spare) = scroll::whole_lines(f32::from(bounds.size.height) - self.top - self.bottom, self.line);
        let height = px(lines as f32 * self.line);
        let available = size(AvailableSpace::Definite(bounds.size.width), AvailableSpace::Definite(height));
        self.child.layout_as_root(available, window, cx);
        self.child.prepaint_at(point(bounds.origin.x, bounds.origin.y + px(self.top + spare)), window, cx);
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
