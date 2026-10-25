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
    /// Other people's nicks: a few hues clear of ours and of the colours
    /// that mean something (see `nick_hues`), then the same hues a tier
    /// away; a buffer hands them out with `slots`.
    pub nicks: Vec<Hsla>,
    /// How many distinct hues `nicks` holds (slot `i` has hue `i % hues`).
    pub nick_hues: usize,
    /// Background of a line that mentions us (`buffer.highlight`).
    pub highlight: Hsla,
    /// Selected text in the input (`buffer.selection`, by contrast).
    pub selection: Hsla,
    pub timestamp: Hsla,
    /// The quietest legible text on the buffer: the client's own wording
    /// (event verbs, scroll notes). The first of `buffer.timestamp`,
    /// `buffer.topic` and `text.secondary` that reaches `QUIET_CONTRAST`.
    pub quiet: Hsla,
    /// The input's placeholder: as `quiet`, against the input's background.
    pub hint: Hsla,
    /// A recognised `/command` in the input (`buffer.code`).
    pub code: Hsla,
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

/// Least contrast quiet text keeps against its background: WCAG's floor
/// for text that must still be read (3:1).
const QUIET_CONTRAST: f32 = 3.;

/// The first of `candidates` (quietest first) legible on `bg` by
/// `QUIET_CONTRAST`, else the most legible.
fn legible(bg: Color, candidates: &[Color]) -> Color {
    candidates
        .iter()
        .find(|c| contrast(**c, bg) >= QUIET_CONTRAST)
        .or_else(|| candidates.iter().max_by(|a, b| contrast(**a, bg).total_cmp(&contrast(**b, bg))))
        .copied()
        .unwrap_or(bg)
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

/// The most hues nick colours use. Fewer, further apart, read better than
/// many close ones: a conversation needs neighbours told apart, and the
/// slots (`slots`) see to it that neighbours never share.
const NICK_HUES_MAX: usize = 6;
/// The fewest hues there are, however crowded the theme's wheel: the
/// clearances below shrink until there is room for this many.
const NICK_HUES_MIN: usize = 4;
/// Least distance (degrees of OKLCH hue, which follows how far apart
/// colours look) between two nick hues.
const NICK_SEPARATION: f32 = 24.;
/// How far a nick hue keeps from our own nick's (no one else may look
/// like us), from the colours that mean something on a message line
/// (actions, event markers, errors), and from those that mean something
/// elsewhere (success, links, unread), which matter less beside a nick.
const CLEAR_OWN: f32 = 30.;
const CLEAR_LINE: f32 = 25.;
const CLEAR_ELSEWHERE: f32 = 12.;
/// Colours with less OKLCH chroma than this are greys: they take no hue.
const GREY: f32 = 0.04;
/// Nick colours sit at the theme nickname's contrast against the buffer,
/// kept legible (WCAG's 4.5:1 for text) and short of near-white pastels
/// that read as body text.
const NICK_CONTRAST_MIN: f32 = 4.5;
const NICK_CONTRAST_MAX: f32 = 7.;
/// The second tier (the same hues again, for a crowd bigger than the hues)
/// is this much contrast away from the first.
const NICK_TIER_STEP: f32 = 2.5;
/// Nick colours are at least this saturated (OKLCH chroma), so a theme
/// with a greyish nickname still gets hues that read as hues.
const NICK_CHROMA_MIN: f32 = 0.1;

fn linear(v: u8) -> f32 {
    let v = f32::from(v) / 255.;
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// OKLab (lightness, a, b): a space where distance follows how different
/// colours look, so hues spaced evenly in it look evenly spaced.
fn oklab(c: Color) -> [f32; 3] {
    let [r, g, b] = [c.r, c.g, c.b].map(linear);
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// The sRGB colour at OKLab `lab`, or `None` outside the sRGB gamut.
fn from_oklab([lightness, a, b]: [f32; 3]) -> Option<Color> {
    let l = (lightness + 0.396_337_78 * a + 0.215_803_76 * b).powi(3);
    let m = (lightness - 0.105_561_346 * a - 0.063_854_17 * b).powi(3);
    let s = (lightness - 0.089_484_18 * a - 1.291_485_5 * b).powi(3);
    let rgb = [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ];
    let encode = |v: f32| {
        if !(-1e-4..=1. + 1e-4).contains(&v) {
            return None;
        }
        let v = v.clamp(0., 1.);
        let v = if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1. / 2.4) - 0.055 };
        Some((v * 255.).round() as u8)
    };
    Some(Color { r: encode(rgb[0])?, g: encode(rgb[1])?, b: encode(rgb[2])?, a: 255 })
}

/// OKLCH chroma and hue (degrees) of `c`.
fn chroma_hue(c: Color) -> (f32, f32) {
    let [_, a, b] = oklab(c);
    (a.hypot(b), b.atan2(a).to_degrees().rem_euclid(360.))
}

/// Degrees between two hues, the short way round.
fn hue_gap(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(360.);
    d.min(360. - d)
}

/// The colour at OKLCH `hue` and `chroma` (less where sRGB cannot show
/// it) whose contrast against `bg` is nearest `aim`, on the far side of
/// `bg` from its own lightness (light on a dark buffer).
fn at_contrast(hue: f32, chroma: f32, bg: Color, aim: f32) -> Color {
    let dark = luminance(bg) < 0.5;
    let (cos, sin) = (hue.to_radians().cos(), hue.to_radians().sin());
    (1..400)
        .filter_map(|k| {
            let l = k as f32 / 400.;
            // Step the chroma down until the colour is in gamut.
            (0..=40).map(|i| chroma * (1. - i as f32 / 40.)).find_map(|c| from_oklab([l, c * cos, c * sin]))
        })
        .filter(|c| (luminance(*c) > luminance(bg)) == dark)
        .min_by(|a, b| (contrast(*a, bg) - aim).abs().total_cmp(&(contrast(*b, bg) - aim).abs()))
        .unwrap_or(bg)
}

/// Hues for other people's nicks: as far as they can be from each other
/// and from the hues that already mean something (`own`, our nick;
/// `line`, colours with a meaning on a message line; `elsewhere`, colours
/// with a meaning elsewhere), each placed in turn where it is furthest
/// from all of those, until no hue is `NICK_SEPARATION` clear of the
/// rest or there are `NICK_HUES_MAX`. A crowded wheel narrows the
/// clearances until `NICK_HUES_MIN` fit. In order of placement, so the
/// first few are the furthest apart.
fn nick_hues(own: Color, line: &[Color], elsewhere: &[Color]) -> Vec<f32> {
    let taken = |scale: f32| -> Vec<(f32, f32)> {
        let with =
            |cs: &[Color], clear: f32| cs.iter().map(|c| chroma_hue(*c)).filter(|(c, _)| *c > GREY).map(move |(_, h)| (h, clear * scale)).collect::<Vec<_>>();
        [with(&[own], CLEAR_OWN), with(line, CLEAR_LINE), with(elsewhere, CLEAR_ELSEWHERE)].concat()
    };
    let place = |taken: &[(f32, f32)], separation: f32| {
        let free: Vec<f32> = (0..360).map(|d| d as f32).filter(|h| taken.iter().all(|(t, clear)| hue_gap(*h, *t) >= *clear)).collect();
        let mut out: Vec<f32> = Vec::new();
        while out.len() < NICK_HUES_MAX {
            let room = |h: f32| out.iter().map(|o| hue_gap(h, *o)).chain(taken.iter().map(|(t, _)| hue_gap(h, *t))).fold(360., f32::min);
            let Some(best) = free.iter().copied().max_by(|a, b| room(*a).total_cmp(&room(*b))) else { break };
            if out.iter().any(|o| hue_gap(best, *o) < separation) {
                break;
            }
            out.push(best);
        }
        out
    };
    (0..=10)
        .map(|i| 1. - i as f32 / 10.)
        .map(|scale| place(&taken(scale), NICK_SEPARATION * scale.max(0.5)))
        .find(|hues| hues.len() >= NICK_HUES_MIN)
        .unwrap_or_default()
}

/// Nick colours for the `hues`: each at the theme nickname's chroma (at
/// least `NICK_CHROMA_MIN`) and its contrast against `bg` (kept between
/// `NICK_CONTRAST_MIN` and `NICK_CONTRAST_MAX`), so every nick weighs the
/// same; then the same hues again a tier away in contrast, for when a
/// buffer has more people than hues.
fn nick_colours(nickname: Color, bg: Color, hues: &[f32]) -> Vec<Color> {
    let chroma = chroma_hue(nickname).0.max(NICK_CHROMA_MIN);
    let first = contrast(nickname, bg).clamp(NICK_CONTRAST_MIN, NICK_CONTRAST_MAX);
    let second = if first >= NICK_CONTRAST_MIN + NICK_TIER_STEP { first - NICK_TIER_STEP } else { first + NICK_TIER_STEP };
    [first, second].iter().flat_map(|aim| hues.iter().map(move |h| at_contrast(*h, chroma, bg, *aim))).collect()
}

/// Lines (by index) within this many of each other are neighbours: their
/// nicks never share a colour while there is another to give.
const NEAR: usize = 2;

/// Colour slots for the nicks of one buffer. `lines` holds, for each line
/// in order, the (other people's) nicks it names; `colours` slots exist,
/// the first `hues` of them distinct hues in placement order and the rest
/// the same hues again (slot `i` has hue `i % hues`).
///
/// People take slots in the order they first appear: an unused slot while
/// there is one (the first hues first, as they are the furthest apart),
/// and never the slot, nor if it can be helped the hue, of anyone on the
/// line next to theirs anywhere in the buffer. Once every slot is used,
/// the one last seen longest ago is shared. Keys are ASCII-lowercased, as
/// IRC compares nicks.
pub fn slots(lines: &[Vec<String>], colours: usize, hues: usize) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    if colours == 0 {
        return out;
    }
    let hues = hues.clamp(1, colours);
    let lines: Vec<Vec<String>> = lines.iter().map(|l| l.iter().map(|n| n.to_ascii_lowercase()).collect()).collect();
    // How close (in lines) each pair of nicks ever comes.
    let mut near: HashMap<(&str, &str), usize> = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        for (j, other) in lines.iter().enumerate().skip(i).take(NEAR + 1) {
            for a in line {
                for b in other.iter().filter(|b| *b != a) {
                    let d = (j - i).max(1);
                    for key in [(a.as_str(), b.as_str()), (b.as_str(), a.as_str())] {
                        near.entry(key).and_modify(|e| *e = (*e).min(d)).or_insert(d);
                    }
                }
            }
        }
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        for nick in line {
            if !out.contains_key(nick) {
                let holders = |slot: usize| out.iter().filter(move |(_, s)| **s == slot).map(|(n, _)| n.as_str());
                let close = |within: usize, same: &dyn Fn(usize) -> bool| {
                    out.iter().any(|(n, s)| same(*s) && near.get(&(nick.as_str(), n.as_str())).is_some_and(|d| *d <= within))
                };
                let pick = (0..colours).min_by_key(|&slot| {
                    let exact = |s: usize| s == slot;
                    let hue = |s: usize| s % hues == slot % hues;
                    (
                        close(1, &exact),
                        close(1, &hue),
                        close(NEAR, &exact),
                        holders(slot).count(),
                        close(NEAR, &hue),
                        holders(slot).filter_map(|n| seen.get(n)).max().copied(),
                        slot,
                    )
                });
                out.insert(nick.clone(), pick.unwrap_or(0));
            }
            seen.insert(nick.clone(), i);
        }
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
        let slot = slots.get(&nick.to_ascii_lowercase()).copied().unwrap_or_else(|| nick_slot(nick, self.nick_hues));
        self.nicks.get(slot).copied().unwrap_or(self.nickname)
    }
}

pub fn palette(t: &Theme) -> Palette {
    let hues = nick_hues(
        t.buffer.nickname,
        &[t.buffer.action, t.buffer.server_messages.default, t.text.error],
        &[t.text.success, t.buffer.url, t.general.unread_indicator],
    );
    let quiet = [t.buffer.timestamp, t.buffer.topic, t.text.secondary];
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
        nicks: nick_colours(t.buffer.nickname, t.buffer.background, &hues).into_iter().map(c).collect(),
        nick_hues: hues.len(),
        highlight: c(highlight),
        selection: c(fill(t.buffer.background_text_input, &[t.buffer.selection, t.buffer.border_selected, t.buffer.highlight])),
        timestamp: c(t.buffer.timestamp),
        quiet: c(legible(t.buffer.background, &quiet)),
        hint: c(legible(t.buffer.background_text_input, &quiet)),
        code: c(t.buffer.code),
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
    fn oklab_round_trips() {
        for rgb in [0x89dceb, 0xf9e2af, 0x04a5e5, 0x4c4f69, 0x000000, 0xffffff, 0x808080] {
            let c = Color::rgb(rgb);
            let back = from_oklab(oklab(c)).unwrap();
            assert!([back.r.abs_diff(c.r), back.g.abs_diff(c.g), back.b.abs_diff(c.b)].iter().all(|d| *d <= 1), "{rgb:06x}");
        }
        // Greys have no chroma.
        assert!(chroma_hue(Color::rgb(0x808080)).0 < 1e-3);
    }

    /// The themes the review renders: (nickname, buffer, line meanings,
    /// other meanings).
    fn themes() -> [(u32, u32, [u32; 3], [u32; 3]); 3] {
        [
            // Catppuccin Mocha
            (0x89dceb, 0x181825, [0xfab387, 0xf9e2af, 0xf38ba8], [0xa6e3a1, 0x89b4fa, 0xcba6f7]),
            // Catppuccin Latte
            (0x04a5e5, 0xeff1f5, [0xfe640b, 0xdf8e1d, 0xd20f39], [0x40a02b, 0x1e66f5, 0x8839ef]),
            // hirc's defaults
            (0x7dcfff, 0x16161e, [0xff9e64, 0xe0af68, 0xf7768e], [0x9ece6a, 0x7aa2f7, 0xc792ea]),
        ]
    }

    #[test]
    fn nick_hues_are_apart_from_each_other_and_from_meanings() {
        for (own, _, line, elsewhere) in themes() {
            let (line, elsewhere) = (line.map(Color::rgb), elsewhere.map(Color::rgb));
            let hues = nick_hues(Color::rgb(own), &line, &elsewhere);
            assert!((NICK_HUES_MIN..=NICK_HUES_MAX).contains(&hues.len()), "{own:06x}: {hues:?}");
            for (i, a) in hues.iter().enumerate() {
                assert!(hues[i + 1..].iter().all(|b| hue_gap(*a, *b) >= NICK_SEPARATION), "{own:06x}: {hues:?}");
                assert!(hue_gap(*a, chroma_hue(Color::rgb(own)).1) >= CLEAR_OWN, "{own:06x}: {a} is near our own");
                for m in line {
                    assert!(hue_gap(*a, chroma_hue(m).1) >= CLEAR_LINE, "{own:06x}: {a} is near {m:?}");
                }
                for m in elsewhere.iter().filter(|m| chroma_hue(**m).0 > GREY) {
                    assert!(hue_gap(*a, chroma_hue(*m).1) >= CLEAR_ELSEWHERE, "{own:06x}: {a} is near {m:?}");
                }
            }
        }
        // Grey roles take no hue.
        let grey = [Color::rgb(0x808080)];
        assert_eq!(nick_hues(Color::rgb(0x89dceb), &grey, &grey), nick_hues(Color::rgb(0x89dceb), &[], &[]));
    }

    #[test]
    fn a_crowded_wheel_still_gives_the_fewest_hues() {
        let wheel: Vec<Color> = (0..12).map(|i| at_contrast(i as f32 * 30., 0.12, Color::rgb(0x181825), 7.)).collect();
        assert!(nick_hues(Color::rgb(0x89dceb), &wheel, &[]).len() >= NICK_HUES_MIN);
    }

    #[test]
    fn nick_colours_are_legible_and_tiered() {
        for (own, bg, line, elsewhere) in themes() {
            let (own, bg) = (Color::rgb(own), Color::rgb(bg));
            let hues = nick_hues(own, &line.map(Color::rgb), &elsewhere.map(Color::rgb));
            let colours = nick_colours(own, bg, &hues);
            assert_eq!(colours.len(), 2 * hues.len());
            let (first, second) = colours.split_at(hues.len());
            let aim = contrast(own, bg).clamp(NICK_CONTRAST_MIN, NICK_CONTRAST_MAX);
            assert!(first.iter().all(|c| (contrast(*c, bg) - aim).abs() < 0.2), "{first:?}");
            assert!(second.iter().all(|c| (contrast(*c, bg) - aim).abs() > NICK_TIER_STEP - 0.2), "{second:?}");
            assert!(colours.iter().all(|c| contrast(*c, bg) >= NICK_CONTRAST_MIN - 0.1), "{colours:?}");
        }
    }

    #[test]
    fn nick_slots_are_stable_and_case_blind() {
        assert_eq!(nick_slot("bob", 8), nick_slot("BoB", 8));
        assert_eq!(nick_slot("alice", 5), nick_slot("alice", 5));
        assert!(nick_slot("anything", 5) < 5);
        assert_eq!(nick_slot("anything", 0), 0);
    }

    fn lines(nicks: &[&str]) -> Vec<Vec<String>> {
        nicks.iter().map(|n| if n.is_empty() { vec![] } else { n.split(' ').map(str::to_owned).collect() }).collect()
    }

    #[test]
    fn a_buffer_hands_out_unused_slots_first_hues_first() {
        let got = slots(&lines(&["alice", "bob", "carol", "alice"]), 8, 4);
        assert_eq!((got["alice"], got["bob"], got["carol"]), (0, 1, 2));
        assert_eq!(slots(&lines(&["Bob", "bob"]), 8, 4).len(), 1);
        assert!(slots(&lines(&["alice"]), 0, 0).is_empty());
    }

    #[test]
    fn neighbours_never_share_a_colour() {
        // The review's #general: nine people and six slots (three hues).
        let seq = lines(&[
            "alice",
            "bob",
            "carol",
            "dave",
            "erin",
            "alice",
            "bob",
            "dave_away",
            "x",
            "a_really_long_nickname",
            "carol",
            "bob dave_away",
            "alice",
            "x",
            "erin",
            "dave",
            "a_really_long_nickname",
            "dave_away",
            "bob",
            "",
        ]);
        let got = slots(&seq, 6, 3);
        for pair in seq.windows(2) {
            for a in &pair[0] {
                for b in pair[1].iter().filter(|b| *b != a) {
                    assert_ne!(got[a], got[b], "{a} and {b}: {got:?}");
                }
            }
        }
        // Nicks named on one line (a kick) differ too.
        assert_ne!(got["bob"], got["dave_away"]);
        // With five hues (Mocha's), neighbours differ in hue as well.
        let got = slots(&seq, 10, 5);
        for pair in seq.windows(2) {
            for a in &pair[0] {
                for b in pair[1].iter().filter(|b| *b != a) {
                    assert_ne!(got[a] % 5, got[b] % 5, "{a} and {b}: {got:?}");
                }
            }
        }
    }

    #[test]
    fn a_shared_slot_goes_to_whoever_spoke_longest_ago() {
        // Two slots; carol shares with alice (long gone), not bob.
        let got = slots(&lines(&["alice", "", "", "", "bob", "", "", "", "carol"]), 2, 2);
        assert_eq!(got["carol"], got["alice"]);
    }

    #[test]
    fn quiet_text_stays_legible() {
        // Latte: the timestamp role is 2.3:1 on the buffer, so the topic
        // role (3.9:1) is used; Mocha's timestamp fails too.
        assert_eq!(legible(Color::rgb(0xeff1f5), &[Color::rgb(0x9ca0b0), Color::rgb(0x7c7f93), Color::rgb(0x6c6f85)]), Color::rgb(0x7c7f93));
        assert_eq!(legible(Color::rgb(0x181825), &[Color::rgb(0x585a6e), Color::rgb(0x7f849c), Color::rgb(0xa6adc8)]), Color::rgb(0x7f849c));
        // A legible timestamp role is used as is.
        assert_eq!(legible(Color::rgb(0x000000), &[Color::rgb(0x808080), Color::rgb(0xffffff)]), Color::rgb(0x808080));
        // Nothing reaches it: the most legible.
        assert_eq!(legible(Color::rgb(0x808080), &[Color::rgb(0x707070), Color::rgb(0x202020)]), Color::rgb(0x202020));
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
