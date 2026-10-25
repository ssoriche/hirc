//! The Halloy theme as gpui colours.

use gpui::{Hsla, rgba};
use hirc_core::theme::{Color, Theme};
use std::collections::HashMap;

#[derive(Clone)]
pub struct Palette {
    pub background: Hsla,
    pub border: Hsla,
    pub unread: Hsla,
    pub text: Hsla,
    pub text_dim: Hsla,
    /// `text.tertiary`: the theme's accent text (the NORMAL mode label).
    pub tertiary: Hsla,
    pub success: Hsla,
    pub error: Hsla,
    pub buffer_bg: Hsla,
    pub input_bg: Hsla,
    pub title_bg: Hsla,
    /// A hovered sidebar row: a faint wash.
    pub hover: Hsla,
    /// The selected sidebar row's fill: distinct from `background` (see
    /// `fill`) and from the mention fill (`highlight`), so "where I am"
    /// and "someone said my name" never look alike (see `fill_apart`).
    pub selected: Hsla,
    /// Our own nick (`buffer.nickname`, in bold): no one else gets it.
    pub nickname: Hsla,
    /// Other people's nicks: the theme's nickname colour turned around the
    /// hue wheel (see `nick_hues`); a buffer hands them out with `slots`.
    pub nicks: Vec<Hsla>,
    /// Background of a line that mentions us (`buffer.highlight`).
    pub highlight: Hsla,
    /// Selected text in the input (`buffer.selection`, by contrast).
    pub selection: Hsla,
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

/// `fill`, but also standing apart (by `FILL_CONTRAST`) from `apart`, a
/// fill with another meaning: the first candidate that does both, else
/// what `fill` picks.
fn fill_apart(bg: Color, apart: Color, candidates: &[Color]) -> Color {
    candidates
        .iter()
        .find(|c| c.a == 255 && contrast(**c, bg) >= FILL_CONTRAST && contrast(**c, apart) >= FILL_CONTRAST)
        .copied()
        .unwrap_or_else(|| fill(bg, candidates))
}

/// How many hues nick colours are picked from (some are skipped).
const NICK_HUES: usize = 16;
/// Derived nicks sit in a contrast band from the theme nickname's own
/// contrast up by this much, so every nick weighs about the same.
const NICK_BAND: f32 = 1.;
/// Least contrast a derived nick colour gets against the buffer (WCAG's
/// minimum for large or bold text; themes often set nicks near it).
const NICK_CONTRAST_MIN: f32 = 3.;
/// The band starts no higher than this: a theme whose nickname is near
/// white would push every derived hue to a pastel that reads as body text.
const NICK_CONTRAST_MAX: f32 = 7.;
/// How far (in turns) a nick hue keeps from a hue with another meaning.
const HUE_CLEARANCE: f32 = 0.05;

/// RGB to hue (turns), saturation and lightness, all 0..1.
fn to_hsl(c: Color) -> (f32, f32, f32) {
    let [r, g, b] = [c.r, c.g, c.b].map(|v| f32::from(v) / 255.);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.;
    let d = max - min;
    if d == 0. {
        return (0., 0., l);
    }
    let s = d / (1. - (2. * l - 1.).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.)
    } else if max == g {
        (b - r) / d + 2.
    } else {
        (r - g) / d + 4.
    };
    (h / 6., s, l)
}

fn from_hsl(h: f32, s: f32, l: f32) -> Color {
    let c = (1. - (2. * l - 1.).abs()) * s;
    let h6 = h.rem_euclid(1.) * 6.;
    let x = c * (1. - (h6.rem_euclid(2.) - 1.).abs());
    let (r, g, b) = match h6 as u32 {
        0 => (c, x, 0.),
        1 => (x, c, 0.),
        2 => (0., c, x),
        3 => (0., x, c),
        4 => (x, 0., c),
        _ => (c, 0., x),
    };
    let m = l - c / 2.;
    let v = |v: f32| ((v + m).clamp(0., 1.) * 255.).round() as u8;
    Color { r: v(r), g: v(g), b: v(b), a: 255 }
}

/// Nick colours derived from the theme: `nickname` first, then the same
/// saturation at evenly spaced hues, skipping hues close to the colours
/// that mean something else in the buffer (`avoid`: actions, events,
/// errors), so a nick never reads as one of those. Each derived hue takes
/// the lightness that puts it in one contrast band against `bg`, from the
/// theme nickname's own contrast (kept between `NICK_CONTRAST_MIN` and
/// `NICK_CONTRAST_MAX`) up by `NICK_BAND`: every nick is legible, none
/// shouts because of its hue, and none fades into the body text. Neighbouring hues alternate between the low
/// and high end of the band, so they differ in lightness too.
fn nick_hues(nickname: Color, bg: Color, avoid: &[Color]) -> Vec<Color> {
    let (h, s, _) = to_hsl(nickname);
    let taken: Vec<f32> = avoid.iter().map(|c| to_hsl(*c)).filter(|(_, s, _)| *s > 0.2).map(|(h, ..)| h).collect();
    let near = |hue: f32| {
        taken.iter().any(|t| {
            let d = (hue - t).rem_euclid(1.);
            d.min(1. - d) < HUE_CLEARANCE
        })
    };
    let want = contrast(nickname, bg).clamp(NICK_CONTRAST_MIN, NICK_CONTRAST_MAX);
    let dark = luminance(bg) < 0.5;
    (0..NICK_HUES)
        .filter_map(|i| {
            if i == 0 {
                return Some(nickname);
            }
            let hue = (h + i as f32 / NICK_HUES as f32).rem_euclid(1.);
            if near(hue) {
                return None;
            }
            let aim = want + if i % 2 == 0 { 0.2 } else { 0.8 } * NICK_BAND;
            // Lightnesses on the far side of the background, closest to `aim`.
            (0..=1000)
                .map(|k| from_hsl(hue, s, k as f32 / 1000.))
                .filter(|c| (luminance(*c) > luminance(bg)) == dark)
                .min_by(|a, b| (contrast(*a, bg) - aim).abs().total_cmp(&(contrast(*b, bg) - aim).abs()))
        })
        .collect()
}

/// Colour slots for the nicks of one buffer, in the order they first
/// appear: each takes the free slot furthest (in hue) from those already
/// taken, the hashed slot (`nick_slot`) breaking ties, so the people in a
/// conversation never share a colour while there are slots to go round,
/// and the first few are as far apart as the wheel allows. Keys are
/// ASCII-lowercased, as IRC compares nicks.
pub fn slots<'a>(nicks: impl IntoIterator<Item = &'a str>, count: usize) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    let mut used = vec![false; count];
    for nick in nicks {
        let key = nick.to_ascii_lowercase();
        if count == 0 || out.contains_key(&key) {
            continue;
        }
        let start = nick_slot(nick, count);
        // Slots sit in hue order: take the free one furthest from every
        // slot in use, nearest the hashed slot among equals.
        let ring = |a: usize, b: usize| {
            let d = a.abs_diff(b);
            d.min(count - d)
        };
        let clear = |s: usize| (0..count).filter(|&u| used[u]).map(|u| ring(s, u)).min().unwrap_or(count);
        let slot = (0..count).filter(|&s| !used[s]).max_by_key(|&s| (clear(s), std::cmp::Reverse(ring(s, start)))).unwrap_or(start);
        used[slot] = true;
        out.insert(key, slot);
    }
    out
}

/// A stable slot among `slots` colours for `nick` (FNV-1a over the
/// ASCII-folded nick, so `Bob` and `bob` match as IRC does).
pub fn nick_slot(nick: &str, slots: usize) -> usize {
    let hash = nick.bytes().fold(0x811c_9dc5_u32, |h, b| (h ^ u32::from(b.to_ascii_lowercase())).wrapping_mul(0x0100_0193));
    hash as usize % slots.max(1)
}

impl Palette {
    /// The colour for someone else's nick: its slot in `slots` (see
    /// `slots`), else its hashed slot.
    pub fn nick(&self, nick: &str, slots: &HashMap<String, usize>) -> Hsla {
        let slot = slots.get(&nick.to_ascii_lowercase()).copied().unwrap_or_else(|| nick_slot(nick, self.nicks.len()));
        self.nicks.get(slot).copied().unwrap_or(self.nickname)
    }
}

pub fn palette(t: &Theme) -> Palette {
    let highlight = fill(t.buffer.background, &[t.buffer.highlight, t.buffer.selection, t.buffer.border_selected]);
    Palette {
        background: c(t.general.background),
        border: c(t.general.border),
        unread: c(t.general.unread_indicator),
        text: c(t.text.primary),
        text_dim: c(t.text.secondary),
        tertiary: c(t.text.tertiary),
        success: c(t.text.success),
        error: c(t.text.error),
        buffer_bg: c(t.buffer.background),
        input_bg: c(t.buffer.background_text_input),
        title_bg: c(t.buffer.background_title_bar),
        hover: c(t.buttons.primary.background_hover),
        selected: c(fill_apart(
            t.general.background,
            highlight,
            &[t.buttons.secondary.background_selected, t.buffer.border_selected, t.buffer.selection, t.buttons.secondary.background_selected_hover],
        )),
        nickname: c(t.buffer.nickname),
        nicks: nick_hues(t.buffer.nickname, t.buffer.background, &[t.buffer.action, t.buffer.server_messages.default, t.text.error])
            .into_iter()
            .skip(1)
            .map(c)
            .collect(),
        highlight: c(highlight),
        selection: c(fill(t.buffer.background_text_input, &[t.buffer.selection, t.buffer.border_selected, t.buffer.highlight])),
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
    fn hsl_round_trips() {
        for rgb in [0x89dceb, 0xf9e2af, 0x04a5e5, 0x4c4f69, 0x000000, 0xffffff, 0x808080] {
            let c = Color::rgb(rgb);
            let (h, s, l) = to_hsl(c);
            let back = from_hsl(h, s, l);
            assert!([back.r.abs_diff(c.r), back.g.abs_diff(c.g), back.b.abs_diff(c.b)].iter().all(|d| *d <= 1), "{rgb:06x}");
        }
    }

    #[test]
    fn nick_hues_are_distinct_and_legible() {
        for (nick, bg) in [(0x89dceb, 0x181825), (0x04a5e5, 0xeff1f5), (0x7dcfff, 0x16161e)] {
            let (nick, bg) = (Color::rgb(nick), Color::rgb(bg));
            let hues = nick_hues(nick, bg, &[]);
            assert_eq!(hues.len(), NICK_HUES);
            assert_eq!(hues[0], nick);
            // One contrast band: none harder to read than the theme's own,
            // none much louder.
            let want = contrast(nick, bg).clamp(NICK_CONTRAST_MIN, NICK_CONTRAST_MAX);
            assert!(hues[1..].iter().all(|c| (want - 0.15..=want + NICK_BAND + 0.15).contains(&contrast(*c, bg))), "{hues:?}");
            for (i, a) in hues.iter().enumerate() {
                assert!(hues[i + 1..].iter().all(|b| b != a));
            }
        }
    }

    #[test]
    fn nick_hues_keep_clear_of_other_meanings() {
        // Mocha: actions peach, events yellow, errors red.
        let avoid = [Color::rgb(0xfab387), Color::rgb(0xf9e2af), Color::rgb(0xf38ba8)];
        let hues = nick_hues(Color::rgb(0x89dceb), Color::rgb(0x181825), &avoid);
        assert!(hues.len() >= 4 && hues.len() < NICK_HUES, "{}", hues.len());
        for c in &hues {
            let h = to_hsl(*c).0;
            for a in avoid {
                let d = (h - to_hsl(a).0).rem_euclid(1.);
                assert!(d.min(1. - d) >= HUE_CLEARANCE - 0.01, "{c:?} is near {a:?}");
            }
        }
        // Grey roles take no hue.
        assert_eq!(nick_hues(Color::rgb(0x89dceb), Color::rgb(0x181825), &[Color::rgb(0x808080)]).len(), NICK_HUES);
    }

    #[test]
    fn nick_slots_are_stable_and_case_blind() {
        assert_eq!(nick_slot("bob", 8), nick_slot("BoB", 8));
        assert_eq!(nick_slot("alice", 5), nick_slot("alice", 5));
        assert!(nick_slot("anything", 5) < 5);
        assert_eq!(nick_slot("anything", 0), 0);
        // Different nicks spread over the slots.
        let used: std::collections::HashSet<_> =
            ["alice", "bob", "carol", "dave", "erin", "frank", "gina", "henry"].map(|n| nick_slot(n, 8)).into_iter().collect();
        assert!(used.len() >= 5, "{used:?}");
    }

    #[test]
    fn a_buffer_hands_out_distinct_slots_in_order() {
        let nicks = ["alice", "bob", "carol", "dave", "erin", "dave_away", "x", "a_really_long_nickname"];
        let got = slots(nicks, 8);
        let distinct: std::collections::HashSet<_> = got.values().collect();
        assert_eq!(distinct.len(), 8, "{got:?}");
        // The first to speak keeps its hashed slot; case does not matter.
        assert_eq!(got["alice"], nick_slot("alice", 8));
        assert_eq!(slots(["Bob", "bob"], 8).len(), 1);
        // More nicks than slots: the rest share, from their hashed slot.
        assert_eq!(slots(nicks, 3).len(), nicks.len());
        assert!(slots(nicks, 0).is_empty());
    }

    #[test]
    fn the_selected_fill_stands_apart_from_the_mention_fill() {
        // Mocha: the selected role is the mention's hue at nearly its
        // lightness; the next role that differs from both wins.
        let (bg, mention) = (Color::rgb(0x11111b), Color::rgb(0x594c76));
        let picked = fill_apart(bg, mention, &[Color::rgb(0x4c4163), Color::rgb(0x4c4163), Color::rgb(0x2c324a)]);
        assert_eq!(picked, Color::rgb(0x2c324a));
        // Nothing apart from both: as `fill` would pick.
        assert_eq!(fill_apart(bg, Color::rgb(0x4c4163), &[Color::rgb(0x4c4163)]), Color::rgb(0x4c4163));
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
