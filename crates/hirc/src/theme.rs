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
    /// A hovered sidebar row: a quiet wash, visibly off the list's
    /// background and well short of the selected fill (see `wash`).
    pub hover: Hsla,
    /// The selected sidebar row's fill: distinct from `background` (see
    /// `fill`) and from the mention fill (`highlight`), so "where I am"
    /// and "someone said my name" never look alike (see `fill_apart`).
    pub selected: Hsla,
    /// The selected row's label: `text.primary`, kept legible on the
    /// selected fill (see `lift`).
    pub selected_text: Hsla,
    /// Our own nick (`buffer.nickname`, in bold): no one else gets it.
    pub nickname: Hsla,
    /// Other people's nicks: a few hues clear of ours and of the colours
    /// that mean something (see `nick_hues`), then a second tier at
    /// another contrast in the hues between; a buffer hands them out with
    /// `slots`.
    pub nicks: Vec<Hsla>,
    /// The same slots on the mention fill, aimed at its contrast, so a
    /// nick keeps its hue and stays legible on a line that names us.
    pub nicks_band: Vec<Hsla>,
    /// Each slot's OKLCH hue, for `slots` to keep near hues apart.
    pub nick_hue: Vec<f32>,
    /// How many slots are the first tier (a hashed nick takes one).
    pub nick_hues: usize,
    /// Background of a line that mentions us (`buffer.highlight`).
    pub highlight: Hsla,
    /// Selected text in the input (`buffer.selection`, by contrast).
    pub selection: Hsla,
    pub timestamp: Hsla,
    /// The timestamp on the mention fill: the least lift that keeps it
    /// legible there, so the time column stays one colour as near as it
    /// can.
    pub timestamp_band: Hsla,
    /// A network tag (the window list's and the header's):
    /// `text.secondary` at the quiet floor, a plain step below the name it
    /// follows, so the two read apart by colour whatever weights the font
    /// has. `selected_tag` is the same on the selected fill.
    pub tag: Hsla,
    pub selected_tag: Hsla,
    /// A peer who is away: a state, not identity, so not the tag's role
    /// (the event colour, `buffer.server_messages.default`).
    pub away: Hsla,
    /// The quietest legible text on the buffer: the client's own wording
    /// (event verbs, scroll notes). The first of `buffer.timestamp`,
    /// `buffer.topic` and `text.secondary` that reaches `QUIET_CONTRAST`.
    pub quiet: Hsla,
    /// The input's placeholder: as `quiet`, against the input's background.
    pub hint: Hsla,
    /// A recognised `/command` in the input: `buffer.code`, or `buffer.url`
    /// where the code colour is too near body text to tell apart (see
    /// `apart_from`).
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

/// Least contrast text that is read keeps (messages, states, labels):
/// WCAG's 4.5:1.
const TEXT_CONTRAST: f32 = 4.5;

/// `c`, moved in OKLab lightness away from the backgrounds until it has
/// `min` contrast on every one of `bgs`: the theme's own colour (its hue,
/// and its chroma where sRGB can show it) made legible, unchanged when it
/// already is. Some themes set a role too faint for the surface it is
/// drawn on (Latte's timestamp and event colours are 2.3:1).
fn lift(c: Color, bgs: &[Color], min: f32) -> Color {
    let worst = |c: Color| bgs.iter().map(|b| contrast(c, *b)).fold(f32::INFINITY, f32::min);
    if bgs.is_empty() || worst(c) >= min {
        return c;
    }
    // Further the way it already stands from the surface: lighter on a
    // darker one, darker on a lighter one.
    let step = if luminance(c) >= luminance(bgs[0]) { 0.004 } else { -0.004 };
    let [l, a, b] = oklab(c);
    let mut best = c;
    for k in 1..=250 {
        let l = l + step * k as f32;
        if !(0. ..=1.).contains(&l) {
            break;
        }
        // Ease the chroma until sRGB can show it.
        let Some(next) = (0..=20).map(|i| 1. - i as f32 / 20.).find_map(|s| from_oklab([l, a * s, b * s])) else { continue };
        best = next;
        if worst(next) >= min {
            break;
        }
    }
    best
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

/// `c` laid over `bg` at the least opacity (in steps of 1%) that stands
/// `min` off it: a wash of the colour, opaque. The colour itself when no
/// wash gets there.
fn tint(c: Color, bg: Color, min: f32) -> Color {
    let mix = |a: f32| {
        let m = |x: u8, y: u8| (f32::from(x) * a + f32::from(y) * (1. - a)).round() as u8;
        Color { r: m(c.r, bg.r), g: m(c.g, bg.g), b: m(c.b, bg.b), a: 255 }
    };
    (1..=100).map(|k| mix(k as f32 / 100.)).find(|w| contrast(*w, bg) >= min).unwrap_or(Color { a: 255, ..c })
}

/// Least contrast a hover wash keeps from the list's background (seen,
/// though quiet), and from the selected fill (never read as selection).
const HOVER_CONTRAST: f32 = 1.15;
const HOVER_APART: f32 = 1.2;

/// The hover wash: the first candidate `HOVER_CONTRAST` off `bg` and
/// `HOVER_APART` from `selected`, else the first candidate, lifted off
/// `bg` to `HOVER_CONTRAST` (its own hue, the way it already leans).
fn wash(bg: Color, selected: Color, candidates: &[Color]) -> Color {
    candidates
        .iter()
        .find(|c| c.a == 255 && contrast(**c, bg) >= HOVER_CONTRAST && contrast(**c, selected) >= HOVER_APART)
        .copied()
        .unwrap_or_else(|| lift(candidates[0], &[bg], HOVER_CONTRAST))
}

/// How far (OKLab distance) a colour that marks something in running
/// text keeps from the text itself, so it reads as a different colour at
/// a glance and not as a tint of it.
const TEXT_APART: f32 = 0.1;

/// The first of `candidates` at least `TEXT_APART` from `text`, else the
/// furthest.
fn apart_from(text: Color, candidates: &[Color]) -> Color {
    let far = |c: &Color| {
        let ([a, b, l], [x, y, z]) = (oklab(*c), oklab(text));
        ((a - x).powi(2) + (b - y).powi(2) + (l - z).powi(2)).sqrt()
    };
    candidates.iter().find(|c| far(c) >= TEXT_APART).or_else(|| candidates.iter().max_by(|a, b| far(a).total_cmp(&far(b)))).copied().unwrap_or(text)
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
const CLEAR_OWN: f32 = 40.;
const CLEAR_LINE: f32 = 25.;
const CLEAR_ELSEWHERE: f32 = 12.;
/// Colours with less OKLCH chroma than this are greys: they take no hue.
const GREY: f32 = 0.04;
/// Nick colours are legible (WCAG's 4.5:1 for text) and short of
/// near-white pastels that read as body text: the two tiers sit at
/// `NICK_CONTRAST_MIN` and `NICK_TIER` times that, apart from each other
/// by `NICK_TIER` (a plain step in lightness, not a near-match), the
/// tier nearer the theme nickname's own contrast first.
const NICK_CONTRAST_MIN: f32 = 4.5;
const NICK_TIER: f32 = 1.75;
/// Nick colours on the mention fill: just past the floor.
const NICK_CONTRAST_BAND: f32 = NICK_CONTRAST_MIN + 0.5;
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

/// Hues for other people's nicks, in two tiers. The first: as far as
/// they can be from each other and from the hues that already mean
/// something (`own`, our nick; `line`, colours with a meaning on a
/// message line; `elsewhere`, colours with a meaning elsewhere), each
/// placed in turn where it is furthest from all of those, until no hue is
/// `NICK_SEPARATION` clear of the rest or there are `NICK_HUES_MAX`. A
/// crowded wheel narrows the clearances until `NICK_HUES_MIN` fit. The
/// second: as many again, placed the same way in the room the first
/// leaves, half the separation apart, so a crowd bigger than the first
/// tier still gets a hue each (where there is no room left, a first-tier
/// hue repeats). In order of placement, so the first few are the
/// furthest apart.
fn nick_hues(own: Color, line: &[Color], elsewhere: &[Color]) -> (Vec<f32>, Vec<f32>) {
    // `near`: how much of their clearance the colours with a meaning
    // elsewhere keep (a second-tier nick, darker or lighter than they
    // are, may come closer to them; never closer to our own nick or a
    // meaning on the message line).
    let taken = |scale: f32, near: f32| -> Vec<(f32, f32)> {
        let with =
            |cs: &[Color], clear: f32| cs.iter().map(|c| chroma_hue(*c)).filter(|(c, _)| *c > GREY).map(move |(_, h)| (h, clear * scale)).collect::<Vec<_>>();
        [with(&[own], CLEAR_OWN), with(line, CLEAR_LINE), with(elsewhere, CLEAR_ELSEWHERE * near)].concat()
    };
    let place = |taken: &[(f32, f32)], mut out: Vec<f32>, separation: f32, most: usize| {
        let free: Vec<f32> = (0..360).map(|d| d as f32).filter(|h| taken.iter().all(|(t, clear)| hue_gap(*h, *t) >= *clear)).collect();
        while out.len() < most {
            let room = |h: f32| out.iter().map(|o| hue_gap(h, *o)).chain(taken.iter().map(|(t, _)| hue_gap(h, *t))).fold(360., f32::min);
            let Some(best) = free.iter().copied().max_by(|a, b| room(*a).total_cmp(&room(*b))) else { break };
            if out.iter().any(|o| hue_gap(best, *o) < separation) {
                break;
            }
            out.push(best);
        }
        out
    };
    let Some((first, scale)) = (0..=10)
        .map(|i| 1. - i as f32 / 10.)
        .map(|scale| (place(&taken(scale, 1.), vec![], NICK_SEPARATION * scale.max(0.5), NICK_HUES_MAX), scale))
        .find(|(hues, _)| hues.len() >= NICK_HUES_MIN)
    else {
        return (vec![], vec![]);
    };
    let n = first.len();
    let both = place(&taken(scale, 0.5), first.clone(), NICK_SEPARATION * scale.max(0.5) / 2., 2 * n);
    let mut second = both[n..].to_vec();
    second.extend(first.iter().skip(second.len()).take(n - second.len()));
    (first, second)
}

/// Nick colours for the two tiers of hues: each at the theme nickname's
/// chroma (at least `NICK_CHROMA_MIN`) and, against `bg`, one tier at
/// `NICK_CONTRAST_MIN` and the other `NICK_TIER` times that, the first
/// tier the one nearer the nickname's own contrast, so every nick of a
/// tier weighs the same and the tiers differ plainly in lightness.
fn nick_colours(nickname: Color, bg: Color, first: &[f32], second: &[f32]) -> Vec<Color> {
    let (lo, hi) = (NICK_CONTRAST_MIN, NICK_CONTRAST_MIN * NICK_TIER);
    let (one, two) = if contrast(nickname, bg) >= (lo + hi) / 2. { (hi, lo) } else { (lo, hi) };
    [tier(nickname, bg, first, one), tier(nickname, bg, second, two)].concat()
}

/// The same hues on the mention fill: both tiers at `NICK_CONTRAST_BAND`,
/// legible and still saturated enough to keep their hue (aimed at the
/// first tier's contrast, a light fill's nicks wash out to near white).
fn band_colours(nickname: Color, band: Color, first: &[f32], second: &[f32]) -> Vec<Color> {
    [tier(nickname, band, first, NICK_CONTRAST_BAND), tier(nickname, band, second, NICK_CONTRAST_BAND)].concat()
}

/// `hues` at the nickname's chroma and `aim` contrast against `bg`, never
/// short of the floor (where the nearest step falls just below).
fn tier(nickname: Color, bg: Color, hues: &[f32], aim: f32) -> Vec<Color> {
    let chroma = chroma_hue(nickname).0.max(NICK_CHROMA_MIN);
    hues.iter().map(|h| lift(at_contrast(*h, chroma, bg, aim), &[bg], NICK_CONTRAST_MIN)).collect()
}

/// Lines (by index) within this many of each other are about a screen
/// apart: their nicks keep different hues while there are others to
/// give. A fixed count of lines, never the window's, so a nick's colour
/// never changes as the window is resized.
const SCREEN: usize = 30;

/// Two slots whose hues are closer than this look alike at a glance: a
/// little more than the least separation, as hues that close (Latte's
/// teals, 24° apart) still read as one family.
const ALIKE: f32 = 30.;

fn alike(a: f32, b: f32) -> bool {
    hue_gap(a, b) < ALIKE
}

/// Colour slots for the nicks of one buffer. `lines` holds, for each line
/// in order, the (other people's) nicks it names; `hues` holds each
/// slot's hue, the first tier first.
///
/// People take slots in the order they first appear, by these rules in
/// turn: never the slot, nor if it can be helped a hue like it, of anyone
/// on the line next to theirs anywhere in the buffer; not the slot, nor a
/// hue like it, of anyone within a screen (`SCREEN`) of them; an unused
/// slot (the first tier first, as its hues are the furthest apart); and
/// last, the slot whose holder was seen longest ago. It depends only on
/// the buffer's lines. Keys are ASCII-lowercased, as IRC compares nicks.
pub fn slots(lines: &[Vec<String>], hues: &[f32]) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    let colours = hues.len();
    if colours == 0 {
        return out;
    }
    let lines: Vec<Vec<String>> = lines.iter().map(|l| l.iter().map(|n| n.to_ascii_lowercase()).collect()).collect();
    // How close (in lines) each pair of nicks ever comes.
    let mut near: HashMap<(&str, &str), usize> = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        for (j, other) in lines.iter().enumerate().skip(i).take(SCREEN + 1) {
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
                    let like = |s: usize| alike(hues[s], hues[slot]);
                    (
                        close(1, &exact),
                        close(1, &like),
                        close(SCREEN, &exact),
                        close(SCREEN, &like),
                        holders(slot).count(),
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
    /// The colour for someone else's nick (the whole nick, however the
    /// column shows it): its slot in `slots` (see `slots`), else its
    /// hashed slot; on the mention fill when `band`.
    pub fn nick(&self, nick: &str, slots: &HashMap<String, usize>, band: bool) -> Hsla {
        let slot = slots.get(&nick.to_ascii_lowercase()).copied().unwrap_or_else(|| nick_slot(nick, self.nick_hues));
        let colours = if band { &self.nicks_band } else { &self.nicks };
        colours.get(slot).copied().unwrap_or(self.nickname)
    }
}

pub fn palette(t: &Theme) -> Palette {
    let (first, second) = nick_hues(
        t.buffer.nickname,
        &[t.buffer.action, t.buffer.server_messages.default, t.text.error],
        &[t.text.success, t.buffer.url, t.general.unread_indicator],
    );
    let (bg, title, input, side) = (t.buffer.background, t.buffer.background_title_bar, t.buffer.background_text_input, t.general.background);
    let quiet = [t.buffer.timestamp, t.buffer.topic, t.text.secondary];
    // A mention: the theme's highlight, or where that vanishes into the
    // buffer (Latte's), a wash of the unread colour the mention badge
    // uses, so the line keeps the meaning's hue and never looks like a
    // selection; only then the neutral roles.
    let highlight = fill(bg, &[t.buffer.highlight, tint(t.general.unread_indicator, bg, FILL_CONTRAST), t.buffer.selection, t.buffer.border_selected]);
    let selected = fill_apart(
        side,
        highlight,
        &[t.buttons.secondary.background_selected, t.buffer.border_selected, t.buffer.selection, t.buttons.secondary.background_selected_hover],
    );
    // Every role at its floor on the surfaces it is drawn on: text that
    // is read (messages, states, labels, markers) 4.5:1, secondary text
    // (times, the topic, quiet wording) 3:1.
    let read = |role: Color, on: &[Color]| c(lift(role, on, TEXT_CONTRAST));
    let text = lift(t.text.primary, &[bg, title, input, side, highlight], TEXT_CONTRAST);
    // A recognised command: the theme's code colour, or where that is a
    // tint of body text (Mocha), the success colour (a command that will
    // run), never the link colour, which would read as a link.
    let code = apart_from(t.text.primary, &[t.buffer.code, t.text.success]);
    let hues: Vec<f32> = [first.as_slice(), second.as_slice()].concat();
    Palette {
        background: c(side),
        border: c(t.general.border),
        unread: read(t.general.unread_indicator, &[side]),
        text: c(text),
        // Also a mention's time, on the mention fill.
        text_dim: c(lift(lift(t.text.secondary, &[bg, title, input, side], TEXT_CONTRAST), &[highlight], QUIET_CONTRAST)),
        tertiary: read(t.text.tertiary, &[input]),
        // States are read in the header and the list, and marked by a dot
        // or a tag on the selected row too.
        success: c(lift(lift(t.text.success, &[title, side, bg], TEXT_CONTRAST), &[selected], QUIET_CONTRAST)),
        error: c(lift(lift(t.text.error, &[title, side, bg, input], TEXT_CONTRAST), &[selected], QUIET_CONTRAST)),
        buffer_bg: c(bg),
        input_bg: c(input),
        title_bg: c(title),
        hover: c(wash(side, selected, &[t.buttons.primary.background_hover, t.buttons.secondary.background_hover, title])),
        selected: c(selected),
        selected_text: read(text, &[selected]),
        nickname: read(t.buffer.nickname, &[bg]),
        nicks: nick_colours(t.buffer.nickname, bg, &first, &second).into_iter().map(c).collect(),
        nicks_band: band_colours(t.buffer.nickname, highlight, &first, &second).into_iter().map(c).collect(),
        nick_hue: hues,
        nick_hues: first.len(),
        highlight: c(highlight),
        selection: c(fill(input, &[t.buffer.selection, t.buffer.border_selected, t.buffer.highlight])),
        timestamp: c(lift(t.buffer.timestamp, &[bg], QUIET_CONTRAST)),
        timestamp_band: c(lift(lift(t.buffer.timestamp, &[bg], QUIET_CONTRAST), &[highlight], QUIET_CONTRAST)),
        tag: c(lift(t.text.secondary, &[side, title], QUIET_CONTRAST)),
        selected_tag: c(lift(t.text.secondary, &[selected], QUIET_CONTRAST)),
        away: read(t.buffer.server_messages.default, &[title]),
        quiet: c(legible(bg, &quiet)),
        hint: c(legible(input, &quiet)),
        code: read(code, &[input]),
        action: read(t.buffer.action, &[bg, highlight]),
        server: read(t.buffer.server_messages.default, &[bg, highlight]),
        topic: c(lift(t.buffer.topic, &[title, bg], QUIET_CONTRAST)),
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
    fn a_command_colour_stands_apart_from_text() {
        let (text, code, success) = (Color::rgb(0xcdd6f4), Color::rgb(0xb7bdf8), Color::rgb(0xa6e3a1));
        // Mocha's code colour is a tint of its text; the success colour is
        // not (and is no link colour).
        assert_eq!(apart_from(text, &[code, success]), success);
        // Latte's is plainly apart.
        assert_eq!(apart_from(Color::rgb(0x4c4f69), &[Color::rgb(0x7287fd), Color::rgb(0x1e66f5)]), Color::rgb(0x7287fd));
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
            let (hues, second) = nick_hues(Color::rgb(own), &line, &elsewhere);
            assert!((NICK_HUES_MIN..=NICK_HUES_MAX).contains(&hues.len()), "{own:06x}: {hues:?}");
            // The second tier: as many again, in the room between.
            assert_eq!(second.len(), hues.len());
            for (i, a) in second.iter().enumerate() {
                assert!(hues.iter().chain(&second[..i]).all(|b| a == b || hue_gap(*a, *b) >= NICK_SEPARATION / 4.), "{own:06x}: {second:?}");
                assert!(hue_gap(*a, chroma_hue(Color::rgb(own)).1) >= CLEAR_OWN, "{own:06x}: {a} is near our own");
            }
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
        // Mocha: the second tier finds room for new hues (8 in all), and
        // repeats first-tier hues only for the rest.
        let (first, second) = nick_hues(Color::rgb(0x89dceb), &themes()[0].2.map(Color::rgb), &themes()[0].3.map(Color::rgb));
        let new = second.iter().take_while(|h| !first.contains(h)).count();
        assert!(new >= 3 && second[new..].iter().all(|h| first.contains(h)), "{first:?} {second:?}");
    }

    #[test]
    fn a_crowded_wheel_still_gives_the_fewest_hues() {
        let wheel: Vec<Color> = (0..12).map(|i| at_contrast(i as f32 * 30., 0.12, Color::rgb(0x181825), 7.)).collect();
        assert!(nick_hues(Color::rgb(0x89dceb), &wheel, &[]).0.len() >= NICK_HUES_MIN);
    }

    #[test]
    fn nick_colours_are_legible_and_tiered() {
        for (own, bg, line, elsewhere) in themes() {
            let (own, bg) = (Color::rgb(own), Color::rgb(bg));
            let (hues, second) = nick_hues(own, &line.map(Color::rgb), &elsewhere.map(Color::rgb));
            let colours = nick_colours(own, bg, &hues, &second);
            assert_eq!(colours.len(), 2 * hues.len());
            let (first, second) = colours.split_at(hues.len());
            // Each tier weighs the same, and a hue's two tiers are at least
            // 1.5:1 apart (a step in lightness, not a near-match).
            let aim = contrast(first[0], bg);
            assert!(first.iter().all(|c| (contrast(*c, bg) - aim).abs() < 0.2), "{first:?}");
            for (a, b) in first.iter().zip(second) {
                assert!(contrast(*a, *b) >= 1.5, "{a:?} {b:?}: {:.2}", contrast(*a, *b));
            }
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

    /// `n` slots over `distinct` evenly spaced hues (slot `i` has hue
    /// `i % distinct`), or with `distinct == n / 2`, a second tier in the
    /// hues between the first's.
    fn hues(distinct: usize, n: usize) -> Vec<f32> {
        (0..n).map(|i| (i % distinct) as f32 * 360. / distinct as f32).collect()
    }

    fn lines(nicks: &[&str]) -> Vec<Vec<String>> {
        nicks.iter().map(|n| if n.is_empty() { vec![] } else { n.split(' ').map(str::to_owned).collect() }).collect()
    }

    #[test]
    fn a_buffer_hands_out_unused_slots_first_hues_first() {
        let got = slots(&lines(&["alice", "bob", "carol", "alice"]), &hues(4, 8));
        assert_eq!((got["alice"], got["bob"], got["carol"]), (0, 1, 2));
        assert_eq!(slots(&lines(&["Bob", "bob"]), &hues(4, 8)).len(), 1);
        assert!(slots(&lines(&["alice"]), &[]).is_empty());
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
        let got = slots(&seq, &hues(3, 6));
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
        let got = slots(&seq, &hues(5, 10));
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
        let got = slots(&lines(&["alice", "", "", "", "bob", "", "", "", "carol"]), &hues(2, 2));
        assert_eq!(got["carol"], got["alice"]);
    }

    #[test]
    fn people_on_one_screen_take_hues_of_their_own() {
        // The review's #general: eight people on one screen, and ten slots
        // with a hue each (Mocha's two tiers). No two share a slot, and
        // the first tier goes first.
        let seq =
            lines(&["alice", "bob", "carol", "dave", "erin", "alice", "bob", "dave_away", "x", "a_really_long_nickname", "carol", "bob", "alice", "x", "erin"]);
        let tiers: Vec<f32> = (0..5).map(|i| i as f32 * 72.).chain((0..5).map(|i| i as f32 * 72. + 36.)).collect();
        let got = slots(&seq, &tiers);
        let mut taken: Vec<usize> = got.values().copied().collect();
        taken.sort();
        taken.dedup();
        assert_eq!(taken.len(), got.len(), "{got:?}");
        assert_eq!((got["alice"], got["bob"], got["carol"], got["dave"], got["erin"]), (0, 1, 2, 3, 4));
        // On one screen, a hue like someone's goes before their very
        // colour; neighbours keep apart even in hue.
        let got = slots(&lines(&["alice", "", "bob", "", "carol"]), &[0., 180., 10.]);
        assert_eq!(got["carol"], 2);
        let got = slots(&lines(&["alice", "bob", "carol"]), &[0., 180., 10., 90.]);
        assert_eq!(got["carol"], 3);
    }

    #[test]
    fn a_nick_keeps_its_slot_whatever_shows_of_it() {
        // Slots come from the buffer's lines alone, and a nick the column
        // cuts is looked up whole.
        let seq = lines(&["bob", "a_really_long_nickname", "bob"]);
        let got = slots(&seq, &hues(5, 10));
        assert_ne!(got["bob"], got["a_really_long_nickname"]);
        assert_eq!(slots(&seq, &hues(5, 10)), got);
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
    fn lift_keeps_the_hue_and_reaches_the_floor() {
        // Latte's timestamp and event colours on its buffer.
        let bg = Color::rgb(0xeff1f5);
        for (role, min) in [(0x9ca0b0, QUIET_CONTRAST), (0xdf8e1d, TEXT_CONTRAST), (0x40a02b, TEXT_CONTRAST)] {
            let role = Color::rgb(role);
            let lifted = lift(role, &[bg], min);
            assert!(contrast(lifted, bg) >= min && contrast(lifted, bg) < min + 0.3, "{role:?} -> {lifted:?}");
            // Darker on a light buffer, the same hue.
            assert!(luminance(lifted) < luminance(role));
            if chroma_hue(role).0 > GREY {
                assert!(hue_gap(chroma_hue(lifted).1, chroma_hue(role).1) < 6., "{role:?} -> {lifted:?}");
            }
        }
        // Legible already: unchanged.
        assert_eq!(lift(Color::rgb(0xcdd6f4), &[Color::rgb(0x181825)], TEXT_CONTRAST), Color::rgb(0xcdd6f4));
        // On a dark surface it lightens.
        let lifted = lift(Color::rgb(0x585a6e), &[Color::rgb(0x181825)], QUIET_CONTRAST);
        assert!(contrast(lifted, Color::rgb(0x181825)) >= QUIET_CONTRAST && luminance(lifted) > luminance(Color::rgb(0x585a6e)));
    }

    /// The review's two themes (public Catppuccin palettes), as Halloy
    /// theme files.
    const MOCHA: &str = r##"
[general]
background = "#11111B"
border = "#6C7086"
unread_indicator = "#CBA6F7"
[text]
primary = "#CDD6F4"
secondary = "#A6ADC8"
tertiary = "#CBA6F7"
success = "#A6E3A1"
error = "#F38BA8"
[buffer]
action = "#FAB387"
background = "#181825"
background_text_input = "#1E1E2E"
background_title_bar = "#1E1E2E"
border = "#11111B"
border_selected = "#4C4163"
code = "#B7BDF8"
highlight = "#594C76"
nickname = "#89DCEB"
selection = "#2C324A"
timestamp = "#585A6E"
topic = "#7F849C"
url = "#89B4FA"
[buffer.server_messages]
default = "#F9E2AF"
[buttons.primary]
background_hover = "#1A1A29"
[buttons.secondary]
background_selected = "#4C4163"
background_selected_hover = "#685786"
"##;
    const LATTE: &str = r##"
[general]
background = "#DCE0E8"
border = "#9CA0B0"
unread_indicator = "#8839EF"
[text]
primary = "#4C4F69"
secondary = "#6C6F85"
tertiary = "#8839EF"
success = "#40A02B"
error = "#D20F39"
[buffer]
action = "#FE640B"
background = "#EFF1F5"
background_text_input = "#E6E9EF"
background_title_bar = "#E6E9EF"
border = "#DCE0E8"
border_selected = "#BCC0CC"
code = "#7287FD"
highlight = "#DCE0E8"
nickname = "#04A5E5"
selection = "#CCD0DA"
timestamp = "#9CA0B0"
topic = "#7C7F93"
url = "#1E66F5"
[buffer.server_messages]
default = "#DF8E1D"
[buttons.primary]
background_hover = "#E6E9EF"
[buttons.secondary]
background_selected = "#BCC0CC"
background_selected_hover = "#ACB0BE"
"##;

    fn theme(name: &str, text: &str) -> Theme {
        // One dir per call: tests run at once and each removes its own.
        static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hirc-theme-test-{}-{call}-{name}", std::process::id()));
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        std::fs::write(dir.join("themes").join(format!("{name}.toml")), text).unwrap();
        let theme = hirc_core::theme::load(&dir, name).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        theme
    }

    fn rgb(h: Hsla) -> Color {
        let c: gpui::Rgba = h.into();
        let v = |f: f32| (f * 255.).round() as u8;
        Color { r: v(c.r), g: v(c.g), b: v(c.b), a: 255 }
    }

    #[test]
    fn every_role_is_legible_where_it_is_drawn() {
        for (name, text) in [("Mocha", MOCHA), ("Latte", LATTE), ("defaults", "")] {
            let p = palette(&theme(name, text));
            let ratio = |fg: Hsla, bg: Hsla| contrast(rgb(fg), rgb(bg));
            let read = TEXT_CONTRAST - 0.01;
            let quiet = QUIET_CONTRAST - 0.01;
            // Read text: messages, states, labels and markers.
            for (what, fg, bgs) in [
                ("text", p.text, vec![p.buffer_bg, p.title_bg, p.input_bg, p.background, p.highlight]),
                ("selected label", p.selected_text, vec![p.selected]),
                ("secondary", p.text_dim, vec![p.buffer_bg, p.title_bg, p.input_bg, p.background]),
                ("success", p.success, vec![p.title_bg, p.background]),
                ("error", p.error, vec![p.title_bg, p.background, p.buffer_bg]),
                ("unread", p.unread, vec![p.background]),
                ("mode label", p.tertiary, vec![p.input_bg]),
                ("command", p.code, vec![p.input_bg]),
                ("own nick", p.nickname, vec![p.buffer_bg]),
                ("action", p.action, vec![p.buffer_bg, p.highlight]),
                ("event marker", p.server, vec![p.buffer_bg, p.highlight]),
            ] {
                for bg in bgs {
                    assert!(ratio(fg, bg) >= read, "{name} {what}: {:.2}", ratio(fg, bg));
                }
            }
            for (i, (n, b)) in p.nicks.iter().zip(&p.nicks_band).enumerate() {
                assert!(ratio(*n, p.buffer_bg) >= read && ratio(*b, p.highlight) >= read, "{name} nick {i}");
            }
            // Secondary text: times, the topic, quiet wording, hints.
            for (what, fg, bg) in [
                ("time", p.timestamp, p.buffer_bg),
                ("time on a mention", p.text_dim, p.highlight),
                ("quiet", p.quiet, p.buffer_bg),
                ("hint", p.hint, p.input_bg),
                ("topic", p.topic, p.title_bg),
                ("dot on the selected row", p.success, p.selected),
                ("fault on the selected row", p.error, p.selected),
            ] {
                assert!(ratio(fg, bg) >= quiet, "{name} {what}: {:.2}", ratio(fg, bg));
            }
        }
    }

    #[test]
    fn the_hover_wash_is_seen_and_short_of_selection() {
        // Mocha: the primary hover is 1.09:1 off the list, the secondary
        // one nearly the selected fill; the title bar is lifted to show.
        let (bg, selected) = (Color::rgb(0x11111b), Color::rgb(0x2c324a));
        let hover = wash(bg, selected, &[Color::rgb(0x1a1a29), Color::rgb(0x2b2b42), Color::rgb(0x1e1e2e)]);
        assert!(contrast(hover, bg) >= HOVER_CONTRAST - 0.01 && contrast(hover, selected) >= HOVER_APART, "{hover:?}");
        // A usable role is taken as is.
        assert_eq!(wash(bg, selected, &[Color::rgb(0x1a1a29), Color::rgb(0x202034)]), Color::rgb(0x202034));
        // Latte: every role is at or above the list's own light; the first
        // is lifted lighter still, away from the darker selected fill.
        let (bg, selected) = (Color::rgb(0xdce0e8), Color::rgb(0xacb0be));
        let hover = wash(bg, selected, &[Color::rgb(0xe6e9ef), Color::rgb(0xdce0e8)]);
        assert!(contrast(hover, bg) >= HOVER_CONTRAST - 0.01 && luminance(hover) > luminance(bg), "{hover:?}");
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
    fn a_mention_on_a_theme_without_a_usable_highlight_keeps_the_unread_hue() {
        let p = palette(&theme("Latte", LATTE));
        let (fill, unread) = (rgb(p.highlight), Color::rgb(0x8839ef));
        // A wash of the unread colour: its hue, a band off the buffer.
        assert!(hue_gap(chroma_hue(fill).1, chroma_hue(unread).1) < 10., "{fill:?}");
        assert!(contrast(fill, Color::rgb(0xeff1f5)) >= FILL_CONTRAST);
        // Mocha's own highlight is used as is.
        assert_eq!(rgb(palette(&theme("Mocha", MOCHA)).highlight), Color::rgb(0x594c76));
    }

    #[test]
    fn a_tag_reads_a_step_below_the_name() {
        for (name, text) in [("Mocha", MOCHA), ("Latte", LATTE), ("defaults", "")] {
            let p = palette(&theme(name, text));
            let ratio = |a: Hsla, b: Hsla| contrast(rgb(a), rgb(b));
            assert!(ratio(p.tag, p.background) >= QUIET_CONTRAST - 0.01 && ratio(p.tag, p.title_bg) >= QUIET_CONTRAST - 0.01, "{name}");
            assert!(ratio(p.selected_tag, p.selected) >= QUIET_CONTRAST - 0.01, "{name}");
            // Apart from the name beside it, by colour alone.
            assert!(ratio(p.tag, p.text) >= 1.5, "{name}: {:.2}", ratio(p.tag, p.text));
            assert!(ratio(p.away, p.title_bg) >= TEXT_CONTRAST - 0.01, "{name}");
            assert!(ratio(p.timestamp_band, p.highlight) >= QUIET_CONTRAST - 0.01, "{name}");
        }
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
