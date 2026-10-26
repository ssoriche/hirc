//! IRCv3 `draft/multiline`: several lines sent and shown as one message.
//! A batch carries one PRIVMSG per line; a line too long for one IRC
//! message goes as pieces, each continuation tagged `draft/multiline-concat`
//! so the receiver joins it to the piece before with no separator.

/// The cap, its batch type and the continuation tag.
pub const CAP: &str = "draft/multiline";
pub const CONCAT: &str = "draft/multiline-concat";

/// What the server accepts in one batch, from the cap's value
/// (`max-bytes=4096,max-lines=100`). Never assumed: each server says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes of line content (the PRIVMSG texts) in one batch.
    pub max_bytes: usize,
    /// The most PRIVMSGs in one batch; `None` when the server sets none.
    pub max_lines: Option<usize>,
}

impl Limits {
    /// `max-bytes=N[,max-lines=M]`. `None` without a usable `max-bytes` (the
    /// spec requires it) or when either value is not a positive number, so
    /// a garbled value falls back to plain messages. Unknown keys are skipped.
    pub fn parse(value: &str) -> Option<Limits> {
        let (mut max_bytes, mut max_lines) = (None, None);
        for kv in value.split(',').filter(|kv| !kv.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let n = || v.parse::<usize>().ok().filter(|&n| n > 0);
            match k {
                "max-bytes" => max_bytes = Some(n()?),
                "max-lines" => max_lines = Some(n()?),
                _ => {}
            }
        }
        Some(Limits { max_bytes: max_bytes?, max_lines })
    }
}

/// One PRIVMSG of a batch: its text, and whether it continues the line
/// before it (`draft/multiline-concat`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece<'a> {
    pub text: &'a str,
    pub concat: bool,
}

/// `text` without the blank lines at its start and end: they would only
/// send empty messages. A `\r` before a `\n` goes with it.
pub fn trim(text: &str) -> &str {
    text.trim_matches(['\n', '\r'])
}

/// The lines of `text` (see `trim`), as `str::lines` splits them.
pub fn lines(text: &str) -> Vec<&str> {
    trim(text).lines().collect()
}

/// Splits `line` into pieces of at most `max` bytes that rejoin to exactly
/// `line`: a break comes after the last space that fits (the space ends the
/// piece), else mid-word on a char boundary. A piece is never empty, even if
/// one character is wider than `max`; an empty line is one empty piece.
fn split_keep(line: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while rest.len() > max.max(1) {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
        }
        if let Some(space) = rest.as_bytes()[..cut].iter().rposition(|&b| b == b' ').filter(|&i| i > 0) {
            cut = space + 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

/// The batches `text` goes out as: its lines (see `lines`) as pieces of at
/// most `budget` bytes, grouped so no batch passes `limits`. A batch that
/// fills up mid-line ends there, and the next starts with the rest of that
/// line as a new line, since a continuation can't open a batch.
pub fn batches<'a>(text: &'a str, budget: usize, limits: &Limits) -> Vec<Vec<Piece<'a>>> {
    let max = budget.min(limits.max_bytes).max(1);
    let mut out: Vec<Vec<Piece>> = Vec::new();
    let mut bytes = 0;
    for line in lines(text) {
        for (i, text) in split_keep(line, max).into_iter().enumerate() {
            let full = out.last().is_none_or(|b| bytes + text.len() > limits.max_bytes || limits.max_lines.is_some_and(|m| b.len() >= m));
            if full {
                out.push(Vec::new());
                bytes = 0;
            }
            let batch = out.last_mut().expect("pushed above");
            batch.push(Piece { text, concat: i > 0 && !batch.is_empty() });
            bytes += text.len();
        }
    }
    out
}

/// How many messages `text` goes out as, each body at most `budget`
/// bytes, as `Session` sends it: one per batch while the server takes
/// `limits` (and there is more than one line), else one per piece of each
/// line that isn't blank.
pub fn messages(text: &str, budget: usize, limits: Option<&Limits>) -> usize {
    match limits {
        Some(limits) if lines(text).len() > 1 => batches(text, budget, limits).len(),
        _ => text.lines().filter(|l| !l.is_empty()).map(|l| crate::split::split_utf8(l, budget).len()).sum(),
    }
}

/// A batch's lines as the one message they make: `\n` between lines,
/// nothing before a continuation.
pub fn join<'a>(pieces: impl IntoIterator<Item = (&'a str, bool)>) -> String {
    let mut out = String::new();
    for (i, (text, concat)) in pieces.into_iter().enumerate() {
        if i > 0 && !concat {
            out.push('\n');
        }
        out.push_str(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_bytes: usize, max_lines: Option<usize>) -> Limits {
        Limits { max_bytes, max_lines }
    }

    #[test]
    fn parses_both_keys_one_key_and_garbage() {
        assert_eq!(Limits::parse("max-bytes=4096,max-lines=100"), Some(limits(4096, Some(100))));
        assert_eq!(Limits::parse("max-lines=24,max-bytes=4096"), Some(limits(4096, Some(24))));
        assert_eq!(Limits::parse("max-bytes=40000"), Some(limits(40000, None)));
        assert_eq!(Limits::parse("max-bytes=4096,future-key=x"), Some(limits(4096, None)));
        // max-bytes is required; numbers must be positive numbers.
        for bad in ["", "max-lines=24", "max-bytes", "max-bytes=", "max-bytes=lots", "max-bytes=0", "max-bytes=4096,max-lines=x", "max-bytes=-1", "garbage"] {
            assert_eq!(Limits::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn trims_blank_edges_only() {
        assert_eq!(lines("\n\na\n\nb\r\n\n"), ["a", "", "b"]);
        assert!(lines("\n\n").is_empty());
    }

    #[test]
    fn split_pieces_rejoin_exactly() {
        assert_eq!(split_keep("aaa bbb ccc", 8), ["aaa bbb ", "ccc"]);
        assert_eq!(split_keep("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(split_keep("", 5), [""]);
        let s = "héllo wörld ".repeat(40);
        for max in [1, 2, 3, 7, 50] {
            let parts = split_keep(&s, max);
            assert_eq!(parts.concat(), s);
            assert!(parts.iter().all(|p| !p.is_empty() && (p.len() <= max || p.chars().count() == 1)), "{max}");
        }
    }

    #[test]
    fn one_batch_when_it_fits() {
        let b = batches("one\n\nthree", 400, &limits(4096, Some(24)));
        assert_eq!(b, [vec![Piece { text: "one", concat: false }, Piece { text: "", concat: false }, Piece { text: "three", concat: false }]]);
    }

    #[test]
    fn long_line_continues_with_concat() {
        let b = batches("aaaa bbbb\nc", 6, &limits(4096, None));
        assert_eq!(b, [vec![Piece { text: "aaaa ", concat: false }, Piece { text: "bbbb", concat: true }, Piece { text: "c", concat: false }]]);
        assert_eq!(join(b[0].iter().map(|p| (p.text, p.concat))), "aaaa bbbb\nc");
    }

    #[test]
    fn splits_by_max_lines() {
        let text = (1..=5).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let b = batches(&text, 400, &limits(4096, Some(2)));
        let texts: Vec<Vec<&str>> = b.iter().map(|b| b.iter().map(|p| p.text).collect()).collect();
        assert_eq!(texts, [vec!["1", "2"], vec!["3", "4"], vec!["5"]]);
    }

    #[test]
    fn splits_by_max_bytes_and_never_opens_with_concat() {
        let b = batches("aaaa\nbbbb\ncccccccc", 400, &limits(8, None));
        let texts: Vec<Vec<(&str, bool)>> = b.iter().map(|b| b.iter().map(|p| (p.text, p.concat)).collect()).collect();
        assert_eq!(texts, [vec![("aaaa", false), ("bbbb", false)], vec![("cccccccc", false)]]);
        for batch in &b {
            assert!(batch.iter().map(|p| p.text.len()).sum::<usize>() <= 8);
            assert!(!batch[0].concat);
        }
        // A line wider than max-bytes is cut to it, and no batch opens with a continuation.
        let wide = "x".repeat(20);
        let b = batches(&wide, 400, &limits(8, None));
        assert_eq!(b.len(), 3);
        assert!(b.iter().all(|b| !b[0].concat && b.iter().map(|p| p.text.len()).sum::<usize>() <= 8));
    }

    #[test]
    fn counts_the_messages_a_text_makes() {
        let six = "1\n2\n\n3\n4\n5\n6";
        assert_eq!(messages(six, 400, None), 6);
        assert_eq!(messages(&"a".repeat(30), 10, None), 3);
        assert_eq!(messages(six, 400, Some(&limits(4096, None))), 1);
        assert_eq!(messages(six, 400, Some(&limits(4096, Some(3)))), 3);
        // One line goes plain, whatever the server takes.
        assert_eq!(messages("one", 400, Some(&limits(4096, Some(3)))), 1);
    }

    #[test]
    fn join_uses_newlines_except_for_continuations() {
        assert_eq!(join([("a", false), ("", false), ("b ", false), ("c", true)]), "a\n\nb c");
    }
}
