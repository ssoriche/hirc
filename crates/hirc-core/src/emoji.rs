//! Slack's `:shortcode:` emoji, from iamcal's emoji-data (MIT), generated
//! into `data/emoji.tsv` by `scripts/gen-emoji.sh`. Typed codes expand on
//! send; codes others send render as the emoji when shown, the stored
//! line keeping what came. Only a known `:name:` changes: `std::vec::`,
//! times and URLs stay as written.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

const TSV: &str = include_str!("../data/emoji.tsv");

/// Skin tones as Slack writes them, `skin-tone-2` to `skin-tone-6`.
const TONES: [(&str, char); 5] =
    [("skin-tone-2", '\u{1F3FB}'), ("skin-tone-3", '\u{1F3FC}'), ("skin-tone-4", '\u{1F3FD}'), ("skin-tone-5", '\u{1F3FE}'), ("skin-tone-6", '\u{1F3FF}')];

struct Emoji {
    emoji: &'static str,
    names: Vec<&'static str>,
    /// A tone goes after its first code point.
    skin: bool,
}

struct Table {
    /// In emoji-data's order (Slack's picker order).
    all: Vec<Emoji>,
    by_name: HashMap<&'static str, usize>,
}

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    let all: Vec<Emoji> = TSV
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| {
            let mut cols = l.split('\t');
            let emoji = cols.next()?;
            let names = cols.next()?.split(',').collect();
            Some(Emoji { emoji, names, skin: cols.next() == Some("s") })
        })
        .collect();
    let by_name = all.iter().enumerate().flat_map(|(i, e)| e.names.iter().map(move |n| (*n, i))).collect();
    Table { all, by_name }
});

/// A character a shortcode's name can hold.
pub fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')
}

fn entry(name: &str) -> Option<&'static Emoji> {
    let t = &*TABLE;
    t.by_name.get(name.to_ascii_lowercase().as_str()).map(|&i| &t.all[i])
}

/// The emoji named `name` (no colons), any case.
pub fn lookup(name: &str) -> Option<&'static str> {
    entry(name).map(|e| e.emoji)
}

/// Emoji whose names hold `query`: those with a name starting with it
/// first, then the rest, each in Slack's order, one row per emoji under
/// the name that matched. At most `limit`.
pub fn search(query: &str, limit: usize) -> Vec<(&'static str, &'static str)> {
    let lower = query.to_ascii_lowercase();
    let q = lower.as_str();
    let pick = |prefix: bool| {
        TABLE.all.iter().filter_map(move |e| {
            let hit = |n: &&&str| if prefix { n.starts_with(q) } else { n.contains(q) };
            e.names.iter().find(hit).map(|n| (e.emoji, *n))
        })
    };
    let mut out: Vec<(&str, &str)> = pick(true).take(limit).collect();
    let rest: Vec<_> = pick(false).filter(|(e, _)| !out.iter().any(|(o, _)| o == e)).take(limit.saturating_sub(out.len())).collect();
    out.extend(rest);
    out
}

/// `emoji` with skin tone `tone` put after its first code point (which
/// loses a following U+FE0F).
fn toned(emoji: &str, tone: char) -> String {
    let mut chars = emoji.chars();
    let first = chars.next().unwrap_or_default();
    let rest: String = chars.as_str().strip_prefix('\u{FE0F}').unwrap_or(chars.as_str()).to_owned();
    format!("{first}{tone}{rest}")
}

/// The `:name:` starting at byte `at` of `text` (which is a `:`): its
/// name and where it ends, past the closing colon.
fn code_at(text: &str, at: usize) -> Option<(&str, usize)> {
    if !text.get(at..)?.starts_with(':') {
        return None;
    }
    let body = &text[at + 1..];
    let len = body.find(|c: char| !is_name_char(c))?;
    (len > 0 && body[len..].starts_with(':')).then(|| (&body[..len], at + 1 + len + 1))
}

/// Every known `:name:` (with a `:skin-tone-N:` after it, for an emoji
/// that takes tones) as its emoji; anything else as written.
pub fn render(text: &str) -> Cow<'_, str> {
    if !text.contains(':') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let (mut i, mut copied, mut changed) = (0, 0, false);
    while let Some(off) = text[i..].find(':') {
        let at = i + off;
        match code_at(text, at).and_then(|(name, end)| entry(name).map(|e| (e, end))) {
            Some((e, end)) => {
                out.push_str(&text[copied..at]);
                let tone =
                    code_at(text, end).filter(|_| e.skin).and_then(|(n, end)| TONES.iter().find(|(t, _)| n.eq_ignore_ascii_case(t)).map(|(_, c)| (*c, end)));
                match tone {
                    Some((c, end)) => {
                        out.push_str(&toned(e.emoji, c));
                        i = end;
                    }
                    None => {
                        out.push_str(e.emoji);
                        i = end;
                    }
                }
                copied = i;
                changed = true;
            }
            None => i = at + 1,
        }
    }
    if !changed {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copied..]);
    Cow::Owned(out)
}

/// `render`, owned: what goes out when the user sends `text`.
pub fn expand(text: &str) -> String {
    render(text).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_aliases_look_up_their_emoji() {
        assert_eq!(lookup("tada"), Some("🎉"));
        assert_eq!(lookup("+1"), Some("👍"));
        assert_eq!(lookup("thumbsup"), Some("👍"));
        assert_eq!(lookup("TADA"), Some("🎉"));
        assert_eq!(lookup("no-such-emoji"), None);
    }

    #[test]
    fn search_puts_prefix_matches_before_the_rest() {
        let got = search("thu", 8);
        assert!(!got.is_empty());
        assert!(got.iter().all(|(_, n)| n.contains("thu")), "{got:?}");
        let first_inner = got.iter().position(|(_, n)| !n.starts_with("thu")).unwrap_or(got.len());
        assert!(got[first_inner..].iter().all(|(_, n)| !n.starts_with("thu")), "prefixes first: {got:?}");
        // One row per emoji: 👍 shows once, as the name that matched.
        assert_eq!(got.iter().filter(|(e, _)| *e == "👍").count(), 1);
        assert!(search("thu", 3).len() <= 3);
        assert!(search("zzzzqq", 8).is_empty());
    }

    #[test]
    fn expand_turns_known_codes_into_emoji() {
        assert_eq!(expand("party :tada: now"), "party 🎉 now");
        assert_eq!(expand(":+1::tada:"), "👍🎉");
        assert_eq!(expand(":wave::skin-tone-3:"), "👋🏼");
        // A tone on an emoji that takes none: left as typed after the emoji.
        assert_eq!(expand(":tada::skin-tone-3:"), "🎉:skin-tone-3:");
    }

    #[test]
    fn expand_leaves_everything_else_alone() {
        for s in ["std::vec::Vec", "10:30:45", "http://x.org/a:b", ":nope:", "a :tada b", "::", ":", "", "C:\\x"] {
            assert_eq!(expand(s), s, "{s:?}");
        }
        assert_eq!(expand(":nope::tada:"), ":nope:🎉");
    }

    #[test]
    fn render_borrows_when_nothing_changes() {
        assert!(matches!(render("plain text"), std::borrow::Cow::Borrowed(_)));
        assert_eq!(render("hi :wave:"), "hi 👋");
    }
}
