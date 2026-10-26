//! Selecting message text, as a terminal does: a drag selects by
//! character from where the button went down to where it is, across
//! lines and the rows they wrap onto; a double click a word, a triple
//! click a whole line. A point is a line (by its `seq`, so lines that
//! arrive never move it) and a byte of the line's text as unwrapped (so
//! a resize never moves it either).
//!
//! What it copies: within one line, just the text selected; across
//! lines, each line in log form (`HH:MM nick text`, the nick column as it
//! reads in full, with the text clipped where the selection starts and
//! ends), one a line.
//!
//! Pure: the message view hit-tests and draws, this does the maths.

use std::ops::Range;

/// A point in the buffer: the line's seq and a byte of its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub seq: u64,
    pub byte: usize,
}

/// A selection in window `buffer`: from where it began (`anchor`) to
/// where it is now (`head`), either way round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub buffer: (usize, usize),
    pub anchor: Mark,
    pub head: Mark,
}

/// A point resolved against the buffer: the line's index and a byte.
pub type At = (usize, usize);

/// `a` and `b` in buffer order.
pub fn order(a: At, b: At) -> (At, At) {
    if a <= b { (a, b) } else { (b, a) }
}

/// What of line `ix` (its text `len` bytes) lies between `start` and
/// `end` (in order), or `None` when nothing of it does.
pub fn span(ix: usize, len: usize, start: At, end: At) -> Option<Range<usize>> {
    if ix < start.0 || ix > end.0 {
        return None;
    }
    let from = if ix == start.0 { start.1.min(len) } else { 0 };
    let to = if ix == end.0 { end.1.min(len) } else { len };
    (from < to).then_some(from..to)
}

/// The word around byte `at` of `text`, as a double click takes it: a
/// run of word characters (letters, digits, `_`), of spaces, or of the
/// same punctuation. Past the end, the last character's run.
pub fn word(text: &str, at: usize) -> Range<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let Some(i) = chars.iter().rposition(|(b, _)| *b <= at.min(text.len().saturating_sub(1))) else { return 0..0 };
    let class = |c: char| match c {
        c if c.is_alphanumeric() || c == '_' => 0,
        c if c.is_whitespace() => 1,
        // Each punctuation mark is a class of its own, so `...` is one
        // word but `),` is two.
        c => 2 + c as u32,
    };
    let want = class(chars[i].1);
    let first = chars[..i].iter().rposition(|(_, c)| class(*c) != want).map_or(0, |j| j + 1);
    let last = chars[i..].iter().position(|(_, c)| class(*c) != want).map_or(chars.len(), |j| i + j);
    let end = chars.get(last).map_or(text.len(), |(b, _)| *b);
    chars[first].0..end
}

/// A line as it is copied in log form: the time as shown (`HH:MM`), the
/// nick column in full (`nick`, `-nick-`, `*`, an event marker) and the
/// text as drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    pub time: String,
    pub who: String,
    pub text: String,
}

/// The text a copy takes from `lines`, the lines `start.0..=end.0` (in
/// order): within one line, the bytes selected; across lines, each line
/// that has any of it in log form, `\n` between them.
pub fn copy(lines: &[Log], start: At, end: At) -> String {
    let clip = |text: &str, r: Range<usize>| text.get(floor(text, r.start)..floor(text, r.end)).unwrap_or_default().to_owned();
    if start.0 == end.0 {
        return lines.first().and_then(|l| span(start.0, l.text.len(), start, end)).map(|r| clip(&lines[0].text, r)).unwrap_or_default();
    }
    let mut out = Vec::new();
    for (k, line) in lines.iter().enumerate() {
        let ix = start.0 + k;
        let Some(r) = span(ix, line.text.len(), start, end) else { continue };
        let text = clip(&line.text, r);
        let head = [line.time.as_str(), line.who.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
        out.push(if head.is_empty() { text } else { format!("{head} {text}") });
    }
    out.join("\n")
}

/// The char boundary at or before byte `i` of `s`.
fn floor(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// `runs` (covering a text in order) with `sel` marked by `mark`: a run
/// that crosses an end of `sel` is split there.
pub fn overlay<S: Clone>(runs: Vec<(Range<usize>, S)>, sel: Range<usize>, mark: impl Fn(&S) -> S) -> Vec<(Range<usize>, S)> {
    if sel.is_empty() {
        return runs;
    }
    let mut out = Vec::with_capacity(runs.len() + 2);
    for (r, style) in runs {
        let (a, b) = (r.start.max(sel.start), r.end.min(sel.end));
        if a >= b {
            out.push((r, style));
            continue;
        }
        if r.start < a {
            out.push((r.start..a, style.clone()));
        }
        out.push((a..b, mark(&style)));
        if b < r.end {
            out.push((b..r.end, style));
        }
    }
    out
}

/// Whether a key leaves the selection be: the copy chord (`⌘C`), and a
/// modifier pressed on its own. Every other key clears it.
pub fn keeps(key: &str, platform: bool) -> bool {
    (platform && key.eq_ignore_ascii_case("c")) || matches!(key, "shift" | "control" | "alt" | "platform" | "function" | "cmd" | "ctrl" | "fn")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(time: &str, who: &str, text: &str) -> Log {
        Log { time: time.into(), who: who.into(), text: text.into() }
    }

    #[test]
    fn a_range_runs_across_lines() {
        let (start, end) = order((4, 2), (2, 5));
        assert_eq!((start, end), ((2, 5), (4, 2)));
        // The first line from where it starts, those between whole, the
        // last up to where it ends.
        assert_eq!(span(2, 10, start, end), Some(5..10));
        assert_eq!(span(3, 7, start, end), Some(0..7));
        assert_eq!(span(4, 9, start, end), Some(0..2));
        assert_eq!(span(1, 9, start, end), None);
        assert_eq!(span(5, 9, start, end), None);
        // Starting past a line's end, or ending at its start, takes none of it.
        assert_eq!(span(2, 5, start, end), None);
        assert_eq!(span(4, 9, start, (4, 0)), None);
        // Within one line; an empty drag selects nothing.
        assert_eq!(span(3, 9, (3, 1), (3, 4)), Some(1..4));
        assert_eq!(span(3, 9, (3, 4), (3, 4)), None);
        // An end past the text stops at it.
        assert_eq!(span(3, 9, (3, 1), (3, usize::MAX)), Some(1..9));
    }

    #[test]
    fn words_are_runs_of_one_kind() {
        let s = "hey bob_2, see...  this";
        assert_eq!(&s[word(s, 0)], "hey");
        assert_eq!(&s[word(s, 2)], "hey");
        assert_eq!(&s[word(s, 5)], "bob_2");
        assert_eq!(&s[word(s, 9)], ",");
        assert_eq!(&s[word(s, 15)], "...");
        assert_eq!(&s[word(s, 18)], "  ");
        // Past the end: the last word.
        assert_eq!(&s[word(s, 99)], "this");
        // Multibyte letters are word characters.
        let s = "voilà café";
        assert_eq!(&s[word(s, 3)], "voilà");
        assert_eq!(&s[word(s, 8)], "café");
        assert_eq!(word("", 0), 0..0);
    }

    #[test]
    fn a_selection_within_a_line_copies_just_that() {
        let lines = [log("12:01", "alice", "the only drawback is speed")];
        assert_eq!(copy(&lines, (7, 4), (7, 17)), "only drawback");
        // Never inside a multibyte char.
        let lines = [log("12:01", "alice", "café ok")];
        assert_eq!(copy(&lines, (0, 0), (0, 4)), "caf");
        assert_eq!(copy(&lines, (0, 2), (0, 2)), "");
    }

    #[test]
    fn a_selection_across_lines_copies_each_in_log_form() {
        let lines =
            [log("12:01", "alice", "first line here"), log("12:02", "*", "bob waves"), log("12:03", "-->", "carol joined"), log("12:04", "dave", "last one")];
        assert_eq!(copy(&lines, (3, 6), (6, 4)), "12:01 alice line here\n12:02 * bob waves\n12:03 --> carol joined\n12:04 dave last");
        // A last line the selection only reaches the start of is left out.
        assert_eq!(copy(&lines[..2], (3, 0), (4, 0)), "12:01 alice first line here");
        // No time cell or nick: what there is.
        assert_eq!(copy(&[log("", "", "a"), log("12:00", "", "b")], (0, 0), (1, 1)), "a\n12:00 b");
    }

    #[test]
    fn the_selection_splits_the_runs_it_crosses() {
        let runs = vec![(0..4, 'a'), (4..10, 'b'), (10..12, 'c')];
        let marked = overlay(runs.clone(), 2..11, |c| c.to_ascii_uppercase());
        assert_eq!(marked, [(0..2, 'a'), (2..4, 'A'), (4..10, 'B'), (10..11, 'C'), (11..12, 'c')]);
        assert_eq!(overlay(runs.clone(), 4..10, |c| c.to_ascii_uppercase()), [(0..4, 'a'), (4..10, 'B'), (10..12, 'c')]);
        assert_eq!(overlay(runs.clone(), 3..3, |c| c.to_ascii_uppercase()), runs);
    }

    #[test]
    fn only_the_copy_chord_and_bare_modifiers_keep_it() {
        assert!(keeps("c", true));
        assert!(keeps("shift", false));
        assert!(!keeps("c", false));
        assert!(!keeps("escape", false));
        assert!(!keeps("v", true));
    }
}
