//! How a stored line reads on screen, apart from drawing: event lines split
//! into who did what (so the UI can colour nicks, verbs and free text
//! apart), the marker an event shows in the nick column, and whether a
//! message mentions us. The text formats here mirror `session`'s.

use crate::store::LineKind;

/// One piece of an event line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// Someone's nick, ours included: we are named by our nick everywhere,
    /// as in our messages and actions (irssi, weechat).
    Nick,
    /// The client's own wording ("joined", "kicked").
    Verb,
    /// The channel joined or left.
    Channel,
    /// Text someone typed: a reason, a topic, mode arguments.
    Free,
}

/// The marker an event line shows in the nick column (weechat style):
/// arrivals, departures, errors, a change of connection state (irssi's
/// `-!-`) and everything else.
pub fn marker(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Join => "-->",
        LineKind::Part | LineKind::Quit | LineKind::Kick => "<--",
        LineKind::Error => "=!=",
        LineKind::Status => "-!-",
        _ => "--",
    }
}

/// `text` as `session` worded it for an event of `kind` by `nick`, split
/// into parts. Joins and parts name `channel` (the buffer's), so a line
/// says where it happened wherever it is read. `None` when the text is not
/// in the expected shape (an older format, or a kind that is not an
/// event); show it whole.
pub fn event_parts(kind: LineKind, nick: Option<&str>, text: &str, channel: Option<&str>) -> Option<Vec<(Part, String)>> {
    let nick = nick.filter(|n| !n.is_empty())?;
    let rest = text.strip_prefix(nick)?;
    let mut parts = vec![(Part::Nick, nick.to_owned())];
    let mut push = |part: Part, s: &str| {
        if !s.is_empty() {
            parts.push((part, s.to_owned()));
        }
    };
    // " verb" then an optional " (reason)".
    let reasoned = |rest: &str, verb: &str| -> Option<(String, String)> {
        let after = rest.strip_prefix(verb)?;
        (after.is_empty() || after.starts_with(" (") && after.ends_with(')')).then(|| (verb.to_owned(), after.to_owned()))
    };
    let channel = channel.filter(|c| !c.is_empty());
    match kind {
        LineKind::Join => {
            push(Part::Verb, (rest == " joined").then_some(rest)?);
            if let Some(c) = channel {
                push(Part::Verb, " ");
                push(Part::Channel, c);
            }
        }
        LineKind::Part | LineKind::Quit => {
            let (verb, reason) = reasoned(rest, if kind == LineKind::Part { " left" } else { " quit" })?;
            push(Part::Verb, &verb);
            if let Some(c) = channel.filter(|_| kind == LineKind::Part) {
                push(Part::Verb, " ");
                push(Part::Channel, c);
            }
            push(Part::Free, &reason);
        }
        LineKind::Nick => {
            let new = rest.strip_prefix(" is now known as ")?;
            push(Part::Verb, " is now known as ");
            push(Part::Nick, new);
        }
        LineKind::Kick => {
            // "<victim> was kicked by <kicker> (reason)": the one who
            // leaves first, as on a part.
            let after = rest.strip_prefix(" was kicked by ")?;
            let (by, reason) = after.split_once(' ').map_or((after, ""), |(v, r)| (v, r));
            if by.is_empty() || !(reason.is_empty() || reason.starts_with('(') && reason.ends_with(')')) {
                return None;
            }
            push(Part::Verb, " was kicked by ");
            push(Part::Nick, by);
            push(Part::Free, &if reason.is_empty() { String::new() } else { format!(" {reason}") });
        }
        LineKind::Topic => {
            push(Part::Verb, " changed the topic to: ");
            push(Part::Free, rest.strip_prefix(" changed the topic to: ")?);
        }
        LineKind::Mode => {
            push(Part::Verb, " set mode ");
            push(Part::Free, rest.strip_prefix(" set mode ")?);
        }
        _ => return None,
    }
    Some(parts)
}

/// Characters that may appear in a nick besides letters and digits
/// (RFC 2812 "special"), so `me_` or `[me]` is not a mention of `me`.
fn nick_char(c: char) -> bool {
    c.is_alphanumeric() || "_-[]\\`^{}|".contains(c)
}

/// Does `text` mention `nick` as a whole word (ASCII case-insensitive)?
pub fn mentions(text: &str, nick: &str) -> bool {
    if nick.is_empty() {
        return false;
    }
    let (hay, want) = (text.to_ascii_lowercase(), nick.to_ascii_lowercase());
    hay.match_indices(&want).any(|(at, m)| {
        let before = hay[..at].chars().next_back();
        let after = hay[at + m.len()..].chars().next();
        !before.is_some_and(nick_char) && !after.is_some_and(nick_char)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Part::*;

    fn parts(kind: LineKind, nick: &str, text: &str, channel: Option<&str>) -> Vec<(Part, &'static str)> {
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        event_parts(kind, Some(nick), text, channel).expect("parses").into_iter().map(|(p, s)| (p, leak(s))).collect()
    }

    #[test]
    fn membership_events() {
        assert_eq!(parts(LineKind::Join, "erin", "erin joined", None), [(Nick, "erin"), (Verb, " joined")]);
        // Ours read the same way, by nick.
        assert_eq!(parts(LineKind::Join, "me", "me joined", None), [(Nick, "me"), (Verb, " joined")]);
        assert_eq!(parts(LineKind::Part, "carol", "carol left (going offline)", None), [(Nick, "carol"), (Verb, " left"), (Free, " (going offline)")]);
        assert_eq!(parts(LineKind::Part, "carol", "carol left", None), [(Nick, "carol"), (Verb, " left")]);
        assert_eq!(parts(LineKind::Quit, "frank", "frank quit (Ping timeout: 240 seconds)", None)[2], (Free, " (Ping timeout: 240 seconds)"));
        assert_eq!(parts(LineKind::Nick, "dave", "dave is now known as dave_away", None), [(Nick, "dave"), (Verb, " is now known as "), (Nick, "dave_away")]);
        assert_eq!(parts(LineKind::Nick, "me", "me is now known as me2", None)[..2], [(Nick, "me"), (Verb, " is now known as ")]);
    }

    #[test]
    fn joins_and_parts_name_the_channel() {
        assert_eq!(parts(LineKind::Join, "me", "me joined", Some("#fresh")), [(Nick, "me"), (Verb, " joined"), (Verb, " "), (Channel, "#fresh")]);
        assert_eq!(
            parts(LineKind::Part, "carol", "carol left (bye)", Some("#a")),
            [(Nick, "carol"), (Verb, " left"), (Verb, " "), (Channel, "#a"), (Free, " (bye)")]
        );
        // A quit is network-wide: no channel.
        assert_eq!(parts(LineKind::Quit, "frank", "frank quit", Some("#a")), [(Nick, "frank"), (Verb, " quit")]);
    }

    #[test]
    fn state_events() {
        assert_eq!(
            parts(LineKind::Kick, "dave_away", "dave_away was kicked by bob (afk too long)", None),
            [(Nick, "dave_away"), (Verb, " was kicked by "), (Nick, "bob"), (Free, " (afk too long)")]
        );
        assert_eq!(parts(LineKind::Kick, "dave", "dave was kicked by bob", None), [(Nick, "dave"), (Verb, " was kicked by "), (Nick, "bob")]);
        assert_eq!(
            parts(LineKind::Topic, "bob", "bob changed the topic to: a (b) c", None),
            [(Nick, "bob"), (Verb, " changed the topic to: "), (Free, "a (b) c")]
        );
        // Mode lines for user modes are `own` without being ours to word.
        assert_eq!(parts(LineKind::Mode, "srv", "srv set mode +i", None), [(Nick, "srv"), (Verb, " set mode "), (Free, "+i")]);
    }

    #[test]
    fn unexpected_text_is_shown_whole() {
        assert_eq!(event_parts(LineKind::Join, Some("erin"), "someone joined", None), None);
        assert_eq!(event_parts(LineKind::Part, Some("c"), "c left early", None), None);
        assert_eq!(event_parts(LineKind::Privmsg, Some("a"), "a hi", None), None);
        assert_eq!(event_parts(LineKind::Join, None, "joined", None), None);
    }

    #[test]
    fn markers_by_kind() {
        assert_eq!(marker(LineKind::Join), "-->");
        assert_eq!(marker(LineKind::Quit), "<--");
        assert_eq!(marker(LineKind::Topic), "--");
    }

    #[test]
    fn mentions_are_whole_words() {
        assert!(mentions("me: did you see", "me"));
        assert!(mentions("ping ME please", "me"));
        assert!(mentions("hey, me.", "me"));
        assert!(!mentions("meet me_ later", "me"));
        assert!(!mentions("some memes", "me"));
        assert!(!mentions("[me]", "me"));
        assert!(!mentions("anything", ""));
        assert!(mentions("日本 me", "me"));
    }
}
