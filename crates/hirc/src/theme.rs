//! The Halloy theme as gpui colours.

use gpui::{Hsla, rgba};
use hirc_core::theme::{Color, Theme};

#[derive(Clone)]
pub struct Palette {
    pub background: Hsla,
    pub border: Hsla,
    pub unread: Hsla,
    pub text: Hsla,
    pub text_dim: Hsla,
    pub success: Hsla,
    pub error: Hsla,
    pub buffer_bg: Hsla,
    pub input_bg: Hsla,
    pub title_bg: Hsla,
    /// A hovered sidebar row: a faint wash.
    pub hover: Hsla,
    /// The selected sidebar row's fill, distinct from `background` (see `fill`).
    pub selected: Hsla,
    pub nickname: Hsla,
    pub timestamp: Hsla,
    pub action: Hsla,
    pub server: Hsla,
    pub topic: Hsla,
}

fn c(c: Color) -> Hsla {
    rgba(c.to_rgba_u32()).into()
}

/// WCAG relative luminance.
fn luminance(c: Color) -> f32 {
    let lin = |v: u8| {
        let v = f32::from(v) / 255.;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast ratio between two colours (1 to 21).
fn contrast(a: Color, b: Color) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Least contrast a fill needs to read as a band against its background.
const FILL_CONTRAST: f32 = 1.3;

/// The first candidate that stands out from `bg` (by `FILL_CONTRAST`), or
/// the one that stands out most. Some themes set a role equal to the
/// background, which would make a state invisible.
fn fill(bg: Color, candidates: &[Color]) -> Color {
    let opaque = |c: &&Color| c.a == 255;
    candidates
        .iter()
        .filter(opaque)
        .find(|c| contrast(**c, bg) >= FILL_CONTRAST)
        .or_else(|| candidates.iter().filter(opaque).max_by(|a, b| contrast(**a, bg).total_cmp(&contrast(**b, bg))))
        .copied()
        .unwrap_or(candidates[0])
}

pub fn palette(t: &Theme) -> Palette {
    Palette {
        background: c(t.general.background),
        border: c(t.general.border),
        unread: c(t.general.unread_indicator),
        text: c(t.text.primary),
        text_dim: c(t.text.secondary),
        success: c(t.text.success),
        error: c(t.text.error),
        buffer_bg: c(t.buffer.background),
        input_bg: c(t.buffer.background_text_input),
        title_bg: c(t.buffer.background_title_bar),
        hover: c(t.buttons.primary.background_hover),
        selected: c(fill(t.general.background, &[t.buttons.secondary.background_selected, t.buffer.border_selected, t.buffer.selection])),
        nickname: c(t.buffer.nickname),
        timestamp: c(t.buffer.timestamp),
        action: c(t.buffer.action),
        server: c(t.buffer.server_messages.default),
        topic: c(t.buffer.topic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_is_symmetric_and_bounded() {
        let (black, white) = (Color::rgb(0), Color::rgb(0xffffff));
        assert!((contrast(black, white) - 21.).abs() < 0.01);
        assert_eq!(contrast(white, black), contrast(black, white));
        assert_eq!(contrast(white, white), 1.);
    }

    #[test]
    fn fill_skips_roles_that_vanish_into_the_background() {
        let bg = Color::rgb(0xdce0e8);
        // Equal to the background, then too close, then usable.
        let picked = fill(bg, &[Color::rgb(0xdce0e8), Color::rgb(0xd6dae2), Color::rgb(0xbcc0cc)]);
        assert_eq!(picked, Color::rgb(0xbcc0cc));
        // The first usable candidate wins.
        assert_eq!(fill(Color::rgb(0x11111b), &[Color::rgb(0x4c4163), Color::rgb(0x888888)]), Color::rgb(0x4c4163));
        // None usable: the most distinct one.
        assert_eq!(fill(bg, &[Color::rgb(0xdce0e8), Color::rgb(0xd0d4dc)]), Color::rgb(0xd0d4dc));
    }
}
