//! Keeping messages under IRC's 512-byte line limit without cutting a
//! UTF-8 character in half.

/// Longest PRIVMSG/NOTICE body to `target` that still fits in 512 bytes
/// once the server prepends our `:nick!user@host` source, whose length is
/// `source_len`. Before we know our host, pass `nick.len() + 75` (a 10-byte
/// user and a 63-byte host, plus `!` and `@`).
pub fn body_budget(command: &str, target: &str, source_len: usize) -> usize {
    // ":" source " " command " " target " :" body "\r\n"
    512usize.saturating_sub(1 + source_len + 1 + command.len() + 1 + target.len() + 2 + 2)
}

/// Splits `text` into pieces of at most `max_bytes`, breaking at the last
/// space that fits, or mid-word on a char boundary. A piece is never empty,
/// even if one character is wider than `max_bytes`. A `max_bytes` of 0
/// means no limit.
pub fn split_utf8(text: &str, max_bytes: usize) -> Vec<&str> {
    if max_bytes == 0 {
        return vec![text];
    }
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > max_bytes {
        let mut cut = max_bytes;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            cut = rest.chars().next().map_or(rest.len(), char::len_utf8);
        }
        // A space at index `max_bytes` still fits: the piece is `rest[..i]`,
        // so search one byte past the window. Bytes avoid char-boundary checks.
        let window = &rest.as_bytes()[..(max_bytes + 1).min(rest.len())];
        match window.iter().rposition(|&b| b == b' ').filter(|&i| i > 0) {
            Some(space) => {
                out.push(&rest[..space]);
                rest = &rest[space + 1..];
            }
            None => {
                out.push(&rest[..cut]);
                rest = &rest[cut..];
            }
        }
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_leaves_room_for_source_and_crlf() {
        // ":" + 79 + " PRIVMSG #test :" + body + "\r\n" == 512
        assert_eq!(body_budget("PRIVMSG", "#test", 79), 512 - (1 + 79 + 1 + 7 + 1 + 5 + 2 + 2));
        assert_eq!(body_budget("PRIVMSG", &"#".repeat(600), 79), 0);
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(split_utf8("hello", 10), ["hello"]);
    }

    #[test]
    fn splits_on_last_space_within_limit() {
        assert_eq!(split_utf8("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
    }

    #[test]
    fn hard_splits_long_words_on_char_boundaries() {
        let chunks = split_utf8("ééééé", 3); // each é is 2 bytes
        assert_eq!(chunks, ["é", "é", "é", "é", "é"]);
        for c in split_utf8(&"🙂".repeat(10), 9) {
            assert!(c.len() <= 9);
        }
    }

    #[test]
    fn tiny_limit_still_makes_progress() {
        assert_eq!(split_utf8("🙂🙂", 1), ["🙂", "🙂"]);
        assert_eq!(split_utf8("abc", 0), ["abc"]);
    }
}
