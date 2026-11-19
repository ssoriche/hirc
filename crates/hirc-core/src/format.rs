//! mIRC formatting codes: bold, colours and the rest, as other clients
//! and bots send them. A line is shown as its plain text, styled in
//! ranges; the codes themselves never show.
//!
//! - `\x02` bold, `\x1d` italic, `\x1f` underline, `\x1e` strikethrough,
//!   `\x11` monospace and `\x16` reverse each toggle.
//! - `\x03` takes up to two digits of text colour, then, only when a digit
//!   follows the comma, `,` and up to two of background. Bare, it ends
//!   the colour. `\x04` is the same with six hex digits.
//! - `\x0f` ends everything.

use std::ops::Range;

/// A colour a code asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// mIRC's numbered colours, 0–98 (99, "the default", is no colour).
    Mirc(u8),
    /// `\x04`'s `RRGGBB`.
    Rgb(u32),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub mono: bool,
    pub reverse: bool,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

impl Style {
    fn is_plain(&self) -> bool {
        *self == Style::default()
    }
}

/// A run of the plain text (byte offsets into it) in one style. Text in
/// no span is plain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub style: Style,
}

/// One formatting code at the start of `s`: its byte length, and what it
/// does to the style.
fn code(s: &str, style: &mut Style) -> Option<usize> {
    let b = s.as_bytes();
    let toggle = |flag: &mut bool| {
        *flag = !*flag;
        Some(1)
    };
    match *b.first()? {
        0x02 => toggle(&mut style.bold),
        0x1d => toggle(&mut style.italic),
        0x1f => toggle(&mut style.underline),
        0x1e => toggle(&mut style.strike),
        0x11 => toggle(&mut style.mono),
        0x16 => toggle(&mut style.reverse),
        0x0f => {
            *style = Style::default();
            Some(1)
        }
        0x03 => {
            let digits = |from: usize| b.get(from..).unwrap_or_default().iter().take(2).take_while(|d| d.is_ascii_digit()).count();
            let number = |at: usize, n: usize| s[at..at + n].parse::<u8>().ok().filter(|&c| c != 99).map(Color::Mirc);
            Some(pair(b, digits, number, style))
        }
        0x04 => {
            let digits = |from: usize| {
                let hex = b.get(from..).unwrap_or_default().iter().take(6).take_while(|d| d.is_ascii_hexdigit()).count();
                if hex == 6 { 6 } else { 0 }
            };
            let number = |at: usize, n: usize| u32::from_str_radix(&s[at..at + n], 16).ok().map(Color::Rgb);
            Some(pair(b, digits, number, style))
        }
        _ => None,
    }
}

/// A colour code's `FG[,BG]` after its one-byte lead: sets the style and
/// gives the code's length. No colour at all ends the colour.
fn pair(b: &[u8], digits: impl Fn(usize) -> usize, number: impl Fn(usize, usize) -> Option<Color>, style: &mut Style) -> usize {
    let fg = digits(1);
    if fg == 0 {
        style.fg = None;
        style.bg = None;
        return 1;
    }
    style.fg = number(1, fg);
    let mut len = 1 + fg;
    if b.get(len) == Some(&b',') {
        let bg = digits(len + 1);
        if bg > 0 {
            style.bg = number(len + 1, bg);
            len += 1 + bg;
        }
    }
    len
}

/// `raw`'s plain text and its styled runs.
pub fn parse(raw: &str) -> (String, Vec<Span>) {
    let mut text = String::with_capacity(raw.len());
    let mut spans: Vec<Span> = Vec::new();
    let mut style = Style::default();
    let mut rest = raw;
    while let Some(c) = rest.chars().next() {
        if let Some(len) = code(rest, &mut style) {
            rest = &rest[len..];
            continue;
        }
        let start = text.len();
        text.push(c);
        rest = &rest[c.len_utf8()..];
        if style.is_plain() {
            continue;
        }
        match spans.last_mut() {
            Some(last) if last.style == style && last.range.end == start => last.range.end = text.len(),
            _ => spans.push(Span { range: start..text.len(), style }),
        }
    }
    (text, spans)
}

/// `raw` with every formatting code removed.
pub fn strip(raw: &str) -> String {
    parse(raw).0
}

/// `s` past any formatting codes at its start.
pub fn skip(mut s: &str) -> &str {
    while let Some(len) = code(s, &mut Style::default()) {
        s = &s[len..];
    }
    s
}

/// Whether `raw` holds any formatting code.
pub fn has_codes(raw: &str) -> bool {
    raw.bytes().any(|b| matches!(b, 0x02 | 0x03 | 0x04 | 0x0f | 0x11 | 0x16 | 0x1d | 0x1e | 0x1f))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(f: impl FnOnce(&mut Style)) -> Style {
        let mut s = Style::default();
        f(&mut s);
        s
    }

    fn fg(n: u8) -> Style {
        style(|s| s.fg = Some(Color::Mirc(n)))
    }

    fn span(range: Range<usize>, style: Style) -> Span {
        Span { range, style }
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(parse("hello"), ("hello".into(), vec![]));
        assert!(!has_codes("hello 😀"));
    }

    #[test]
    fn toggles_and_reset() {
        let bold = style(|s| s.bold = true);
        assert_eq!(parse("\x02bold\x02 not"), ("bold not".into(), vec![span(0..4, bold)]));
        assert_eq!(parse("\x02hide\x0f"), ("hide".into(), vec![span(0..4, bold)]));
        let all = style(|s| (s.bold, s.italic, s.underline, s.strike, s.mono, s.reverse) = (true, true, true, true, true, true));
        assert_eq!(parse("\x02\x1d\x1f\x1e\x11\x16x\x0fy"), ("xy".into(), vec![span(0..1, all)]));
    }

    #[test]
    fn colour_takes_at_most_two_digits() {
        // Driftlane's lines, as sent.
        assert_eq!(parse("\x03035,300cr\x0f"), ("5,300cr".into(), vec![span(0..7, fg(3))]));
        assert_eq!(parse("\x031014\x0f: \x0310Ithon\x0f"), ("14: Ithon".into(), vec![span(0..2, fg(10)), span(4..9, fg(10))]));
        assert_eq!(parse("\x0307Synthetics\x0f"), ("Synthetics".into(), vec![span(0..10, fg(7))]));
        assert_eq!(parse("\x0304red\x0f plain"), ("red plain".into(), vec![span(0..3, fg(4))]));
        assert_eq!(parse("\x034x"), ("x".into(), vec![span(0..1, fg(4))]));
    }

    #[test]
    fn background_needs_a_digit_after_the_comma() {
        let teal_on_black = style(|s| (s.fg, s.bg) = (Some(Color::Mirc(10)), Some(Color::Mirc(1))));
        assert_eq!(parse("\x0310,01teal on black\x03 reset"), ("teal on black reset".into(), vec![span(0..13, teal_on_black)]));
        // A bare \x03 ends the colour; the comma is text.
        assert_eq!(parse("\x03,5"), (",5".into(), vec![]));
        assert_eq!(parse("\x034,x"), (",x".into(), vec![span(0..2, fg(4))]));
    }

    #[test]
    fn colour_99_is_no_colour() {
        assert_eq!(parse("\x0399x"), ("x".into(), vec![]));
    }

    #[test]
    fn hex_colours() {
        let orange = style(|s| s.fg = Some(Color::Rgb(0xff8800)));
        assert_eq!(parse("\x04ff8800hot\x04 cold"), ("hot cold".into(), vec![span(0..3, orange)]));
        let both = style(|s| (s.fg, s.bg) = (Some(Color::Rgb(0xff8800)), Some(Color::Rgb(0x000000))));
        assert_eq!(parse("\x04FF8800,000000x"), ("x".into(), vec![span(0..1, both)]));
        // Fewer than six hex digits: a bare \x04, then text.
        assert_eq!(parse("\x04abcx"), ("abcx".into(), vec![]));
    }

    #[test]
    fn spans_are_byte_ranges_of_the_plain_text() {
        let bold = style(|s| s.bold = true);
        let (text, spans) = parse("🏆 \x02hide\x02!");
        assert_eq!(&text[spans[0].range.clone()], "hide");
        assert_eq!(spans, vec![span(5..9, bold)]);
    }

    #[test]
    fn strip_skip_and_has_codes() {
        assert_eq!(strip("\x0310,01a\x02b\x0fc"), "abc");
        assert_eq!(skip("\x02\x0312<name>"), "<name>");
        assert!(has_codes("a\x03b") && has_codes("\x0f"));
    }
}
