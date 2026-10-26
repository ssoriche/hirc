//! Where a message's text breaks onto its next line: a small part of the
//! Unicode line-breaking rules (UAX #14), enough for chat. A line may
//! break after spaces, between ideographs (CJK, kana, emoji), and after a
//! `/` inside a path or URL; never before closing or terminal punctuation
//! (`? ! . , ) 」 。` …), which stays with the word it ends, and never after
//! opening punctuation. A run with no break that is wider than the line
//! breaks between characters. gpui's own wrapper breaks before `?` and
//! `/` (for paths), which orphans a question mark onto a line of its own;
//! breaking here first means its lines always fit.

use unicode_segmentation::UnicodeSegmentation;

/// One break: the next line starts at byte `at`. When `space`, the byte
/// before it is a space, which the break replaces; otherwise a break is
/// inserted there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Break {
    pub at: usize,
    pub space: bool,
}

/// Breaks for `text` on lines `width` wide, given `x(i)`, the advance up
/// to byte `i` of the unbroken line.
pub fn breaks(text: &str, x: impl Fn(usize) -> f32, width: f32) -> Vec<Break> {
    let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    // Where a line may start, in order, then the end.
    let mut ops: Vec<usize> = graphemes.windows(2).filter(|w| allowed(w[0].1, w[1].1)).map(|w| w[1].0).collect();
    ops.push(text.len());
    // A line from `start` to `end`: a space it ends on is the one the
    // break replaces, and takes no room.
    let fits = |start: usize, end: usize| {
        let end = if end < text.len() && text[..end].ends_with(' ') { end - 1 } else { end };
        x(end) - x(start) <= width
    };
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    loop {
        let mut last = None;
        while i < ops.len() && fits(start, ops[i]) {
            last = Some(ops[i]);
            i += 1;
        }
        if i == ops.len() {
            break;
        }
        let at = match last {
            Some(at) if at > start => at,
            // Nothing fits whole: the run breaks between characters, as
            // many as fit (at least one, so the line moves on).
            _ => {
                let mut ends = graphemes.iter().map(|(b, _)| *b).chain([text.len()]).filter(|b| *b > start);
                let first = ends.next().unwrap_or(text.len());
                ends.take_while(|b| x(*b) - x(start) <= width).last().unwrap_or(first).max(first)
            }
        };
        if at >= text.len() {
            break;
        }
        out.push(Break { at, space: text[..at].ends_with(' ') });
        start = at;
    }
    out
}

/// May a line break between `prev` and `next` (two graphemes)?
fn allowed(prev: &str, next: &str) -> bool {
    let (p, n) = (last_char(prev), first_char(next));
    if n == ' ' || closing(n) || opening(p) {
        return false;
    }
    p == ' ' || ideographic(p) || ideographic(n) || (p == '/' && n.is_alphanumeric())
}

fn first_char(s: &str) -> char {
    s.chars().next().unwrap_or(' ')
}

fn last_char(s: &str) -> char {
    s.chars().next_back().unwrap_or(' ')
}

/// Characters a line never starts with (UAX #14 CL, CP, EX, IS, NS and
/// closing quotes).
fn closing(c: char) -> bool {
    matches!(
        c,
        '?' | '!'
            | '.'
            | ','
            | ':'
            | ';'
            | ')'
            | ']'
            | '}'
            | '…'
            | '"'
            | '\''
            | '”'
            | '’'
            | '»'
            | '%'
            | '、'
            | '。'
            | '，'
            | '．'
            | '！'
            | '？'
            | '：'
            | '；'
            | '）'
            | '」'
            | '』'
            | '】'
            | '〕'
            | '〉'
            | '》'
            | '〙'
            | '〛'
            | 'ー'
            | '々'
            | '〜'
            | '・'
            | 'ぁ'
            | 'ぃ'
            | 'ぅ'
            | 'ぇ'
            | 'ぉ'
            | 'っ'
            | 'ゃ'
            | 'ゅ'
            | 'ょ'
            | 'ァ'
            | 'ィ'
            | 'ゥ'
            | 'ェ'
            | 'ォ'
            | 'ッ'
            | 'ャ'
            | 'ュ'
            | 'ョ'
    )
}

/// Characters a line never ends with (UAX #14 OP and opening quotes).
fn opening(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '“' | '‘' | '«' | '（' | '「' | '『' | '【' | '〔' | '〈' | '《' | '〘' | '〚')
}

/// Ideographs, kana, hangul syllables and emoji: a line may break on
/// either side of each (UAX #14 ID).
fn ideographic(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF
        | 0x2E80..=0x2FFF
        | 0x3000..=0x303F
        | 0x3040..=0x30FF
        | 0x3100..=0x31FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF
        | 0xFF00..=0xFFEF
        | 0x1F000..=0x1FAFF
        | 0x20000..=0x3FFFF
        | 0x2600..=0x27BF)
}

/// `text` with `breaks` made: each a `\n`, replacing the space before it
/// or inserted. Returns the new text and, for each byte offset of the
/// old text, how far it moved (so styled ranges can follow).
pub fn apply(text: &str, breaks: &[Break]) -> (String, impl Fn(usize) -> usize + use<>) {
    let mut out = String::with_capacity(text.len() + breaks.len());
    let mut inserted: Vec<usize> = Vec::new();
    let mut from = 0;
    for b in breaks {
        if b.space {
            out.push_str(&text[from..b.at - 1]);
            out.push('\n');
        } else {
            out.push_str(&text[from..b.at]);
            out.push('\n');
            inserted.push(b.at);
        }
        from = b.at;
    }
    out.push_str(&text[from..]);
    // A range ending at an inserted break keeps its end; one starting
    // there starts after it.
    let shift = move |i: usize| i + inserted.iter().filter(|at| **at <= i).count();
    (out, shift)
}

/// Where `breaks` insert a `\n` (rather than replace a space), in the
/// old text's bytes: what `unshift` needs to map back.
pub fn inserted(breaks: &[Break]) -> Vec<usize> {
    breaks.iter().filter(|b| !b.space).map(|b| b.at).collect()
}

/// A byte of the broken text back in the old text (`inserted` as
/// `inserted` gives them): an inserted `\n` is where the next line
/// starts.
pub fn unshift(inserted: &[usize], i: usize) -> usize {
    i - inserted.iter().enumerate().filter(|(k, at)| *at + k < i).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monospace face: 1 px a char.
    fn mono(text: &str) -> impl Fn(usize) -> f32 + '_ {
        move |i| text[..i].chars().count() as f32
    }

    fn lines(text: &str, width: f32) -> Vec<String> {
        let (out, _) = apply(text, &breaks(text, mono(text), width));
        out.split('\n').map(str::to_owned).collect()
    }

    #[test]
    fn closing_punctuation_stays_with_its_word() {
        // "yesterday?" fits whole on the next line; `?` is never alone.
        assert_eq!(lines("did you see the notes from yesterday?", 36.), ["did you see the notes from", "yesterday?"]);
        for mark in ["!", ".", ",", ")", "…", "”"] {
            let text = format!("aaaa bbbb{mark}");
            assert_eq!(lines(&text, 9.), ["aaaa", &format!("bbbb{mark}")], "{mark}");
        }
        // An opening bracket keeps to its word too.
        assert_eq!(lines("aaa (bbb)", 6.), ["aaa", "(bbb)"]);
    }

    #[test]
    fn spaces_break_and_fit_exactly() {
        assert_eq!(lines("aaa bbb ccc", 7.), ["aaa bbb", "ccc"]);
        assert_eq!(lines("aaa bbb ccc", 11.), ["aaa bbb ccc"]);
        assert!(breaks("short", mono("short"), 80.).is_empty());
    }

    #[test]
    fn a_long_run_breaks_between_characters() {
        assert_eq!(lines("aaaaaaaaaa", 4.), ["aaaa", "aaaa", "aa"]);
        assert_eq!(lines("x aaaaaaaaaa", 4.), ["x", "aaaa", "aaaa", "aa"]);
        // Too narrow for even one: one a line, never stuck.
        assert_eq!(lines("abc", 0.), ["a", "b", "c"]);
    }

    #[test]
    fn ideographs_break_between_but_not_before_closing_marks() {
        assert_eq!(lines("日本語です。", 5.), ["日本語で", "す。"]);
        assert_eq!(lines("「日本」語", 3.), ["「日", "本」語"]);
    }

    #[test]
    fn urls_break_after_a_slash() {
        assert_eq!(lines("see https://ex.am/path/to?q=1", 18.), ["see https://ex.am/", "path/to?q=1"]);
    }

    #[test]
    fn inserted_breaks_shift_what_follows() {
        let (out, shift) = apply("abcdef", &[Break { at: 3, space: false }]);
        assert_eq!(out, "abc\ndef");
        assert_eq!((shift(0), shift(2), shift(3), shift(6)), (0, 2, 4, 7));
        let (out, shift) = apply("ab cd", &[Break { at: 3, space: true }]);
        assert_eq!(out, "ab\ncd");
        assert_eq!(shift(5), 5);
        // And back: every byte of the broken text to the old text's.
        let breaks = [Break { at: 2, space: false }, Break { at: 5, space: true }, Break { at: 7, space: false }];
        let (out, shift) = apply("abcd fgh", &breaks);
        assert_eq!(out, "ab\ncd\nfg\nh");
        let ins = inserted(&breaks);
        assert_eq!(ins, [2, 7]);
        for i in 0..="abcd fgh".len() {
            assert_eq!(unshift(&ins, shift(i)), i, "{i}");
        }
        // An inserted `\n` maps to where its next line starts.
        assert_eq!(unshift(&ins, 2), 2);
    }
}
