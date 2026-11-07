//! Completion in the input line: Tab cycles nicks (and `/commands` at the
//! start of the line) in place, irssi style; `:` and two name characters
//! open the emoji popup. Pure: the input keeps the state and draws it.

use crate::command::COMMANDS;
use crate::emoji;
use crate::event::Target;
use crate::store::{Buffer, LineKind};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// The emoji popup shows at most this many rows.
pub const POPUP_ROWS: usize = 8;

/// A Tab cycle under way: what was being completed, the matches, which
/// one is in, and where it was put. A Tab that finds the text no longer
/// as it left it starts afresh.
#[derive(Clone, Debug, PartialEq)]
pub struct Cycle {
    start: usize,
    matches: Vec<String>,
    index: usize,
    inserted: String,
}

/// The word before `cursor`: where it starts.
fn word_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind(char::is_whitespace).map_or(0, |i| i + text[i..].chars().next().map_or(1, char::len_utf8))
}

/// What Tab does at `cursor`: the next completion, or the first of a new
/// cycle. `None` when there is nothing to complete.
pub fn tab(text: &str, cursor: usize, nicks: &[String], cycle: Option<&Cycle>) -> Option<(String, usize, Cycle)> {
    if let Some(c) = cycle
        && text.get(c.start..cursor) == Some(c.inserted.as_str())
        && c.matches.len() > 1
    {
        let index = (c.index + 1) % c.matches.len();
        return Some(put(text, c.start, cursor, Cycle { index, ..c.clone() }));
    }
    let start = word_start(text, cursor);
    let word = &text[start..cursor];
    let matches: Vec<String> = if start == 0 && word.starts_with('/') && word.len() > 1 {
        let q = word[1..].to_ascii_lowercase();
        let mut cmds: Vec<&str> = COMMANDS.iter().copied().filter(|c| c.starts_with(q.as_str()) && *c != q).collect();
        cmds.sort_unstable();
        cmds.into_iter().map(|c| format!("/{c} ")).collect()
    } else if !word.is_empty() {
        let q = word.to_ascii_lowercase();
        let suffix = if start == 0 { ": " } else { " " };
        nicks.iter().filter(|n| n.to_ascii_lowercase().starts_with(q.as_str())).map(|n| format!("{n}{suffix}")).collect()
    } else {
        Vec::new()
    };
    if matches.is_empty() {
        return None;
    }
    Some(put(text, start, cursor, Cycle { start, matches, index: 0, inserted: String::new() }))
}

/// `cycle`'s current match in place of `text[start..end]`.
fn put(text: &str, start: usize, end: usize, mut cycle: Cycle) -> (String, usize, Cycle) {
    let m = cycle.matches[cycle.index].clone();
    let out = format!("{}{m}{}", &text[..start], &text[end..]);
    cycle.inserted = m;
    let cursor = start + cycle.inserted.len();
    (out, cursor, cycle)
}

/// Tab looks back no further than this many lines for who spoke last.
const RECENT_LINES: usize = 2000;

/// Who Tab offers in `buf`: in a channel, its members and the people its
/// bridges relayed, those who spoke last first (newest first, among the
/// newest `RECENT_LINES` lines), then the other members by name. Never `me`,
/// and never a bridge seen relaying there. In a query, the other person.
pub fn nicks(buf: &Buffer, me: Option<&str>) -> Vec<String> {
    let not_me = |n: &str| me.is_none_or(|m| !m.eq_ignore_ascii_case(n));
    match &buf.target {
        Target::Query(n) => vec![n.clone()],
        Target::Server => Vec::new(),
        Target::Channel(_) => {
            let recent = || buf.lines.iter().rev().take(RECENT_LINES);
            let bridges: HashSet<String> = recent().filter_map(|l| l.via.as_deref()).map(str::to_ascii_lowercase).collect();
            let offered = |n: &str| not_me(n) && !bridges.contains(&n.to_ascii_lowercase());
            let members: HashMap<String, &String> = buf.members.iter().filter(|m| offered(m)).map(|m| (m.to_ascii_lowercase(), m)).collect();
            let mut placed: HashSet<String> = HashSet::new();
            let mut out: Vec<String> = Vec::new();
            let spoke = recent().filter(|l| matches!(l.kind, LineKind::Privmsg | LineKind::Notice | LineKind::Action));
            for l in spoke {
                let Some(nick) = l.nick.as_deref() else { continue };
                let key = nick.to_ascii_lowercase();
                // A member as the member list spells them; someone a bridge
                // relayed (not on IRC, so in no member list) as it did.
                let name = match members.get(&key) {
                    Some(m) => m.as_str(),
                    None if l.via.is_some() && offered(nick) => nick,
                    None => continue,
                };
                if placed.insert(key) {
                    out.push(name.to_owned());
                }
            }
            let mut rest: Vec<String> = members.iter().filter(|(k, _)| !placed.contains(*k)).map(|(_, m)| (*m).clone()).collect();
            rest.sort_by_key(|m| m.to_ascii_lowercase());
            out.extend(rest);
            out
        }
    }
}

/// The `:name` the cursor is at the end of, colon included, when it opens
/// the popup: the colon starts the line or follows whitespace, and two or
/// more name characters follow it.
pub fn trigger(text: &str, cursor: usize) -> Option<Range<usize>> {
    let before = text.get(..cursor)?;
    if text[cursor..].chars().next().is_some_and(emoji::is_name_char) {
        return None;
    }
    let colon = before.rfind(':')?;
    let name = &before[colon + 1..];
    let at_word_start = before[..colon].chars().next_back().is_none_or(char::is_whitespace);
    (at_word_start && name.chars().count() >= 2 && name.chars().all(emoji::is_name_char)).then_some(colon..cursor)
}

/// The emoji popup: the `:name` it completes (from `start`), its rows and
/// the one selected.
#[derive(Clone, Debug, PartialEq)]
pub struct Popup {
    pub start: usize,
    pub query: String,
    pub rows: Vec<(&'static str, &'static str)>,
    pub selected: usize,
}

/// The popup for `text` at `cursor`, if one shows: none when the trigger
/// doesn't match, when nothing matches, or when the user put away the
/// one starting at `dismissed`. The selection stays while the query does.
pub fn popup(text: &str, cursor: usize, dismissed: Option<usize>, previous: Option<&Popup>) -> Option<Popup> {
    let r = trigger(text, cursor)?;
    if dismissed == Some(r.start) {
        return None;
    }
    let query = text[r.start + 1..r.end].to_owned();
    let rows = emoji::search(&query, POPUP_ROWS);
    if rows.is_empty() {
        return None;
    }
    let selected = previous.filter(|p| p.start == r.start && p.query == query).map_or(0, |p| p.selected.min(rows.len() - 1));
    Some(Popup { start: r.start, query, rows, selected })
}

impl Popup {
    /// Move the selection by `by` rows, round the ends.
    pub fn step(&mut self, by: isize) {
        let n = self.rows.len() as isize;
        self.selected = (self.selected as isize + by).rem_euclid(n) as usize;
    }

    /// The selected emoji in place of the `:name` (up to `cursor`).
    pub fn accept(&self, text: &str, cursor: usize) -> (String, usize) {
        let emoji = self.rows[self.selected].0;
        let out = format!("{}{emoji}{}", &text[..self.start], &text[cursor..]);
        (out, self.start + emoji.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Target;
    use crate::store::{Buffer, Delivery, Line, LineKind};

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Tab `n` times from `text` (cursor at its end): each result.
    fn tabs(text: &str, nicks: &[&str], n: usize) -> Vec<String> {
        let nicks = names(nicks);
        let (mut t, mut c, mut cy) = (text.to_owned(), text.len(), None::<Cycle>);
        (0..n)
            .map(|_| {
                if let Some((nt, nc, ncy)) = tab(&t, c, &nicks, cy.as_ref()) {
                    (t, c, cy) = (nt, nc, Some(ncy));
                }
                t.clone()
            })
            .collect()
    }

    #[test]
    fn a_nick_at_line_start_gets_a_colon_and_cycles() {
        assert_eq!(tabs("ali", &["alice", "alistair", "bob"], 3), ["alice: ", "alistair: ", "alice: "]);
    }

    #[test]
    fn a_nick_mid_line_gets_a_space() {
        assert_eq!(tabs("thanks AL", &["alice", "bob"], 1), ["thanks alice "]);
    }

    #[test]
    fn commands_complete_at_line_start_only() {
        let got = tabs("/jo", &[], 1);
        assert_eq!(got, ["/join "]);
        // Not a command word: a nick (none here), so nothing.
        assert_eq!(tabs("say /jo", &[], 1), ["say /jo"]);
    }

    #[test]
    fn no_match_changes_nothing() {
        assert!(tab("zz", 2, &names(&["alice"]), None).is_none());
        assert!(tab("", 0, &names(&["alice"]), None).is_none());
    }

    #[test]
    fn an_edit_starts_a_fresh_cycle() {
        let nicks = names(&["alice", "alistair", "bob"]);
        let (t, c, cy) = tab("ali", 3, &nicks, None).unwrap();
        assert_eq!(t, "alice: ");
        // The user typed after it: the old cycle no longer fits, so "b" completes afresh.
        let t2 = format!("{t}b");
        let (t3, _, _) = tab(&t2, c + 1, &nicks, Some(&cy)).unwrap();
        assert_eq!(t3, "alice: bob ");
    }

    fn said(nick: &str, kind: LineKind) -> Line {
        Line {
            seq: 0,
            time: chrono::Utc::now(),
            msgid: None,
            kind,
            nick: Some(nick.into()),
            text: String::new(),
            own: false,
            delivery: Delivery::Sent,
            boundary: false,
            replayed: false,
            via: None,
        }
    }

    #[test]
    fn recent_speakers_come_first() {
        let mut b = Buffer::new(Target::Channel("#a".into()));
        b.members = names(&["alice", "Bob", "carol", "dave", "erin"]);
        b.lines = vec![said("carol", LineKind::Privmsg), said("erin", LineKind::Join), said("bob", LineKind::Action), said("alice", LineKind::Privmsg)];
        // alice is me; bob spoke last, then carol; erin only joined.
        assert_eq!(nicks(&b, Some("alice")), ["Bob", "carol", "dave", "erin"]);
        let q = Buffer::new(Target::Query("Zed".into()));
        assert_eq!(nicks(&q, Some("alice")), ["Zed"]);
    }

    #[test]
    fn a_speaker_older_than_the_scan_cap_sorts_with_the_rest() {
        let mut b = Buffer::new(Target::Channel("#a".into()));
        b.members = names(&["alice", "bob", "carol", "zed"]);
        let mut lines = vec![said("zed", LineKind::Privmsg)];
        lines.extend((0..RECENT_LINES).map(|_| said("carol", LineKind::Privmsg)));
        b.lines = lines;
        // zed spoke before the newest RECENT_LINES lines: alphabetical.
        assert_eq!(nicks(&b, Some("alice")), ["carol", "bob", "zed"]);
    }

    #[test]
    fn people_a_bridge_relayed_are_offered_and_the_bridge_is_not() {
        let mut b = Buffer::new(Target::Channel("#a".into()));
        b.members = names(&["alice", "IrcBot", "Bob", "carol"]);
        let relayed = |nick: &str| Line { via: Some("ircbot".into()), ..said(nick, LineKind::Privmsg) };
        b.lines = vec![
            relayed("dana"),
            said("carol", LineKind::Privmsg),
            said("IrcBot", LineKind::Privmsg),
            relayed("bob"),
            relayed("alice"),
            relayed("IrcBot"),
            relayed("Dana"),
        ];
        // Dana (on Discord) spoke last, as relayed; bob is a member too, so
        // spelled as the member list has him. Not me, not the bridge, even
        // in its own words; dana only once.
        assert_eq!(nicks(&b, Some("alice")), ["Dana", "Bob", "carol"]);
        // Someone gone from IRC, unrelayed, isn't offered.
        b.lines.push(said("zed", LineKind::Privmsg));
        assert_eq!(nicks(&b, Some("alice")), ["Dana", "Bob", "carol"]);
    }

    #[test]
    fn a_command_is_never_offered_as_itself() {
        assert_eq!(tabs("/j", &[], 1), ["/join "]);
        let got = tabs("/q", &[], 4);
        assert_eq!(got[0], "/query ");
        assert_eq!(got[1], "/quit ");
        assert_eq!(got[2], "/quote ");
        assert_eq!(got[3], "/query ");
    }

    #[test]
    fn the_popup_wants_a_colon_at_a_word_start_and_two_chars() {
        assert_eq!(trigger(":ta", 3), Some(0..3));
        assert_eq!(trigger("hi :tad", 7), Some(3..7));
        assert_eq!(trigger(":t", 2), None);
        assert_eq!(trigger("http://x", 8), None);
        assert_eq!(trigger("a:tada", 6), None);
        assert_eq!(trigger(":ta da", 6), None);
        // The cursor must be at the end of the name.
        assert_eq!(trigger(":tada x", 3), None);
    }

    #[test]
    fn the_popup_steps_accepts_and_stays_dismissed() {
        let mut p = popup("hi :tad", 7, None, None).unwrap();
        assert_eq!(p.rows[0].1, "tada");
        assert!(p.rows.len() <= POPUP_ROWS);
        p.step(-1);
        assert_eq!(p.selected, p.rows.len() - 1, "wraps");
        p.step(1);
        assert_eq!(p.selected, 0);
        assert_eq!(p.accept("hi :tad x", 7), ("hi 🎉 x".to_owned(), "hi 🎉".len()));
        // Esc on it: hidden while the same `:` starts the word.
        assert!(popup("hi :tada", 8, Some(3), None).is_none());
        // Typing on keeps the selection when the rows are the same query's.
        let mut q = popup("hi :th", 6, None, None).unwrap();
        q.step(1);
        assert_eq!(popup("hi :th", 6, None, Some(&q)).unwrap().selected, 1);
        assert_eq!(popup("hi :thu", 7, None, Some(&q)).unwrap().selected, 0);
    }
}
