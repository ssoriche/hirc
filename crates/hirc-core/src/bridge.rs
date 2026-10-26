//! Relay bots (a Discord bridge, say) post other people's words as their
//! own: `PRIVMSG #chan :<hacker> hello`. For a nick the user lists under a
//! server's `bridges`, such a line is unwrapped to read as if `hacker` had
//! said it. Only listed nicks are unwrapped; nothing here guesses.

use crate::store::LineKind;

/// What bridges put in a relayed name so it never pings its namesake on
/// IRC: zero-width spaces, joiners and the BOM.
const ZERO_WIDTH: [char; 5] = ['\u{200B}', '\u{200C}', '\u{200D}', '\u{2060}', '\u{FEFF}'];

/// Whether `nick` is one of the `bridges`, matched as IRC nicks are
/// elsewhere in hirc (ignoring ASCII case).
pub fn is_bridge(bridges: &[String], nick: &str) -> bool {
    bridges.iter().any(|b| b.eq_ignore_ascii_case(nick))
}

/// IRC formatting toggles that take no argument: bold, reset, monospace,
/// reverse, italics, strikethrough and underline.
const FORMAT_TOGGLES: [char; 7] = ['\x02', '\x0f', '\x11', '\x16', '\x1d', '\x1e', '\x1f'];

/// A bridge's line `text` as the relayed person's: `<name> rest` is a
/// message from `name`, `* name rest` an action. `None` for anything else
/// (the bot's own words, a malformed prefix), which stays the bot's. Many
/// bridges bold or colour the name (`\x02<name>\x02`, `<\x0312name\x03>`),
/// so formatting codes around and inside the name are ignored.
pub fn unwrap(text: &str) -> Option<(LineKind, String, &str)> {
    let text = skip_formatting(text);
    if let Some(after) = text.strip_prefix('<') {
        let (name, rest) = after.split_once('>')?;
        let rest = skip_formatting(rest);
        let rest = if rest.is_empty() { rest } else { rest.strip_prefix(' ')? };
        let name = clean(name).filter(|n| !n.contains('<'))?;
        return Some((LineKind::Privmsg, name, rest));
    }
    let after = text.strip_prefix("* ")?;
    let (name, rest) = after.split_once(' ').unwrap_or((after, ""));
    Some((LineKind::Action, clean(name)?, rest))
}

/// `unwrap` for a message of several lines (`draft/multiline`): `Some`
/// only when every line is relayed from the same name as the same kind,
/// with the relayed words joined back by `\n`. For one line it is `unwrap`.
pub fn unwrap_lines(text: &str) -> Option<(LineKind, String, String)> {
    let mut out: Option<(LineKind, String, String)> = None;
    for line in text.split('\n') {
        let (kind, name, rest) = unwrap(line)?;
        match &mut out {
            None => out = Some((kind, name, rest.to_owned())),
            Some((k, n, words)) if *k == kind && *n == name => {
                words.push('\n');
                words.push_str(rest);
            }
            Some(_) => return None,
        }
    }
    out
}

/// `name` without its zero-width characters and formatting codes, if
/// anything nameable is left.
fn clean(name: &str) -> Option<String> {
    let name: String = strip_formatting(name).chars().filter(|c| !ZERO_WIDTH.contains(c)).collect();
    (!name.is_empty() && !name.contains(char::is_whitespace)).then_some(name)
}

/// The byte length of the formatting code at the start of `s`, if any: a
/// toggle, or `\x03` with an optional `NN[,NN]` colour pair.
fn code_len(s: &str) -> Option<usize> {
    let mut chars = s.chars();
    match chars.next()? {
        c if FORMAT_TOGGLES.contains(&c) => Some(1),
        '\x03' => {
            let b = s.as_bytes();
            let digits = |from: usize| b[from..].iter().take(2).take_while(|d| d.is_ascii_digit()).count();
            let mut len = 1;
            let fg = digits(len);
            len += fg;
            if fg > 0 && b.get(len) == Some(&b',') {
                let bg = digits(len + 1);
                if bg > 0 {
                    len += 1 + bg;
                }
            }
            Some(len)
        }
        _ => None,
    }
}

/// `s` past any formatting codes at its start.
fn skip_formatting(mut s: &str) -> &str {
    while let Some(len) = code_len(s) {
        s = &s[len..];
    }
    s
}

/// `s` with every formatting code removed.
fn strip_formatting(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        match code_len(rest) {
            Some(len) => rest = &rest[len..],
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relayed_message_reads_as_its_author() {
        assert_eq!(unwrap("<hacker> The only drawback"), Some((LineKind::Privmsg, "hacker".into(), "The only drawback")));
    }

    #[test]
    fn zero_width_characters_leave_the_name() {
        assert_eq!(unwrap("<ha\u{200B}cker> hi"), Some((LineKind::Privmsg, "hacker".into(), "hi")));
        let all = "<\u{FEFF}h\u{200C}a\u{200D}c\u{2060}k> x";
        assert_eq!(unwrap(all), Some((LineKind::Privmsg, "hack".into(), "x")));
        // Nothing but zero-width: no name, so the line stays the bot's.
        assert_eq!(unwrap("<\u{200B}> x"), None);
    }

    #[test]
    fn a_relayed_action_is_an_action() {
        assert_eq!(unwrap("* hacker waves"), Some((LineKind::Action, "hacker".into(), "waves")));
        assert_eq!(unwrap("* ha\u{200B}cker waves at all"), Some((LineKind::Action, "hacker".into(), "waves at all")));
    }

    #[test]
    fn other_lines_stay_the_bots() {
        for text in ["Bridge restarted", "<hacker>hello", "<> hello", "< hacker> hi", "<two words> hi", "<a<b> hi", "*hacker waves", "* ", "", "hi <hacker> x"]
        {
            assert_eq!(unwrap(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_formatted_name_still_unwraps() {
        for text in [
            "<\x02hacker\x02> hi",
            "<\x0312hacker\x03> hi",
            "\x02<hacker>\x02 hi",
            "<\x034,12hacker\x0f> hi",
            "<\x1d\x1fha\x16ck\x11er\x1e\x1f> hi",
            "\x0307<ha\u{200B}cker>\x03 hi",
        ] {
            assert_eq!(unwrap(text), Some((LineKind::Privmsg, "hacker".into(), "hi")), "{text:?}");
        }
        assert_eq!(unwrap("* \x02hacker\x02 waves"), Some((LineKind::Action, "hacker".into(), "waves")));
        assert_eq!(unwrap("\x02<hacker>\x02"), Some((LineKind::Privmsg, "hacker".into(), "")));
        // Formatting inside the relayed words is the words' own.
        assert_eq!(unwrap("<\x02hacker\x02> \x02loud\x02"), Some((LineKind::Privmsg, "hacker".into(), "\x02loud\x02")));
    }

    #[test]
    fn colour_codes_take_at_most_two_digits_and_a_background() {
        // `\x03` then `1` `2`: the third digit is the name's.
        assert_eq!(unwrap("<\x031234> hi"), Some((LineKind::Privmsg, "34".into(), "hi")));
        // A comma with no digit after it is the name's.
        assert_eq!(unwrap("<\x034,x> hi"), Some((LineKind::Privmsg, ",x".into(), "hi")));
        // Nothing but formatting: no name.
        assert_eq!(unwrap("<\x02\x03> hi"), None);
        assert_eq!(unwrap("\x02Bridge restarted"), None);
    }

    #[test]
    fn an_empty_message_still_has_its_author() {
        assert_eq!(unwrap("<hacker>"), Some((LineKind::Privmsg, "hacker".into(), "")));
        assert_eq!(unwrap("<hacker> "), Some((LineKind::Privmsg, "hacker".into(), "")));
        assert_eq!(unwrap("* hacker"), Some((LineKind::Action, "hacker".into(), "")));
    }

    #[test]
    fn several_lines_unwrap_only_from_one_name() {
        let one = |k, n: &str, t: &str| Some((k, n.to_owned(), t.to_owned()));
        assert_eq!(unwrap_lines("<hacker> hi"), one(LineKind::Privmsg, "hacker", "hi"));
        assert_eq!(unwrap_lines("<hacker> hi\n<ha\u{200B}cker> there"), one(LineKind::Privmsg, "hacker", "hi\nthere"));
        assert_eq!(unwrap_lines("* hacker waves\n* hacker again"), one(LineKind::Action, "hacker", "waves\nagain"));
        // Two names, a mix of kinds, or a line with no name: not one person's.
        for text in ["<a> hi\n<b> hi", "<a> hi\n* a waves", "<a> hi\nplain", "<a> hi\n"] {
            assert_eq!(unwrap_lines(text), None, "{text:?}");
        }
    }

    #[test]
    fn only_listed_nicks_are_bridges() {
        let bridges = vec!["IrcBot".to_owned()];
        assert!(is_bridge(&bridges, "ircbot"));
        assert!(is_bridge(&bridges, "IRCBOT"));
        assert!(!is_bridge(&bridges, "IrcBot_"));
        assert!(!is_bridge(&[], "IrcBot"));
    }
}
