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

/// A bridge's line `text` as the relayed person's: `<name> rest` is a
/// message from `name`, `* name rest` an action. `None` for anything else
/// (the bot's own words, a malformed prefix), which stays the bot's.
pub fn unwrap(text: &str) -> Option<(LineKind, String, &str)> {
    if let Some(after) = text.strip_prefix('<') {
        let (name, rest) = after.split_once('>')?;
        let rest = if rest.is_empty() { rest } else { rest.strip_prefix(' ')? };
        let name = clean(name).filter(|n| !n.contains('<'))?;
        return Some((LineKind::Privmsg, name, rest));
    }
    let after = text.strip_prefix("* ")?;
    let (name, rest) = after.split_once(' ').unwrap_or((after, ""));
    Some((LineKind::Action, clean(name)?, rest))
}

/// `name` without its zero-width characters, if anything nameable is left.
fn clean(name: &str) -> Option<String> {
    let name: String = name.chars().filter(|c| !ZERO_WIDTH.contains(c)).collect();
    (!name.is_empty() && !name.contains(char::is_whitespace)).then_some(name)
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
    fn an_empty_message_still_has_its_author() {
        assert_eq!(unwrap("<hacker>"), Some((LineKind::Privmsg, "hacker".into(), "")));
        assert_eq!(unwrap("<hacker> "), Some((LineKind::Privmsg, "hacker".into(), "")));
        assert_eq!(unwrap("* hacker"), Some((LineKind::Action, "hacker".into(), "")));
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
