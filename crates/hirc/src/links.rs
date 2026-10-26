//! Links in message text: `http://`, `https://` and bare `www.` URLs, as
//! a person would point at them. Punctuation that ends the sentence
//! around a URL is not the URL's (`see https://x.org.`), and neither is
//! a closing bracket it never opened (`(https://x.org)`), while one it
//! did open is (`…/wiki/Rust_(language)`). A URL ends at whitespace and
//! at `<`, `>` and `"`, which no URL holds unescaped, so `<https://…>`
//! is the URL inside.

use std::ops::Range;

/// A link in a text: its bytes, and the URL it opens (a bare `www.` one
/// as `https://`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub range: Range<usize>,
    pub url: String,
}

/// The schemes a link may open; nothing else is ever handed to the
/// system's opener.
const SCHEMES: [&str; 2] = ["https://", "http://"];
const WWW: &str = "www.";

/// Every link in `text`, in order.
pub fn find(text: &str) -> Vec<Link> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        if starts_link(text, i)
            && let Some(len) = link_len(&text[i..])
        {
            let s = &text[i..i + len];
            let url = if has_prefix(s, WWW) { format!("https://{s}") } else { s.to_owned() };
            out.push(Link { range: i..i + len, url });
            i += len;
        } else {
            i += c.len_utf8();
        }
    }
    out
}

/// Whether `url` may be opened: only `http` and `https`.
pub fn openable(url: &str) -> bool {
    SCHEMES.iter().any(|s| has_prefix(url, s))
}

/// Whether a press at `down` and a release at `up` (logical px) are a
/// click rather than a drag: the pointer moved no more than `slop`.
pub fn is_click(down: (f32, f32), up: (f32, f32), slop: f32) -> bool {
    (down.0 - up.0).hypot(down.1 - up.1) <= slop
}

/// Whether a link may start at byte `i` of `text`: a scheme or `www.`,
/// not inside a word, a host or a path (`xhttp://`, `a.www.b`).
fn starts_link(text: &str, i: usize) -> bool {
    let rest = &text[i..];
    let prefixed = SCHEMES.iter().any(|s| has_prefix(rest, s)) || has_prefix(rest, WWW);
    let inside = text[..i].chars().next_back().is_some_and(|p| p.is_alphanumeric() || matches!(p, '.' | '@' | '/' | '-' | '_' | ':'));
    prefixed && !inside
}

/// The length of the link at the start of `s` (which starts with a scheme
/// or `www.`), trimmed of what is not the URL's, if it names a host.
fn link_len(s: &str) -> Option<usize> {
    let end = s.find(|c: char| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"')).unwrap_or(s.len());
    let mut url = &s[..end];
    while let Some(last) = url.chars().next_back() {
        let unbalanced = |open: char| url.matches(open).count() < url.matches(last).count();
        let trim = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => true,
            ')' => unbalanced('('),
            ']' => unbalanced('['),
            '}' => unbalanced('{'),
            _ => false,
        };
        if !trim {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    let prefix = SCHEMES.iter().chain([&WWW]).find(|p| has_prefix(url, p))?;
    // Something nameable after the prefix: a host, not `https://` alone.
    url[prefix.len()..].chars().next().filter(|c| c.is_alphanumeric() || *c == '[').map(|_| url.len())
}

/// Whether `s` starts with the ASCII `prefix`, ignoring case.
fn has_prefix(s: &str, prefix: &str) -> bool {
    s.as_bytes().get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URLs `find` sees in `text`, and the text each covers.
    fn urls(text: &str) -> Vec<(&str, String)> {
        find(text).into_iter().map(|l| (&text[l.range], l.url)).collect()
    }

    #[test]
    fn a_trailing_period_ends_the_sentence() {
        assert_eq!(urls("see https://example.org/a."), [("https://example.org/a", "https://example.org/a".into())]);
        assert_eq!(urls("really? https://example.org/?q=1!?"), [("https://example.org/?q=1", "https://example.org/?q=1".into())]);
        assert_eq!(urls("'https://example.org/x', he said"), [("https://example.org/x", "https://example.org/x".into())]);
    }

    #[test]
    fn balanced_brackets_are_the_urls() {
        let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(urls(wiki), [(wiki, wiki.to_owned())]);
        assert_eq!(urls(&format!("{wiki}.")), [(wiki, wiki.to_owned())]);
        assert_eq!(urls("https://example.org/a[1]"), [("https://example.org/a[1]", "https://example.org/a[1]".into())]);
    }

    #[test]
    fn a_url_in_brackets_leaves_them() {
        assert_eq!(urls("(see https://example.org/a)"), [("https://example.org/a", "https://example.org/a".into())]);
        let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(urls(&format!("({wiki}).")), [(wiki, wiki.to_owned())]);
        assert_eq!(urls("<https://example.org/a>"), [("https://example.org/a", "https://example.org/a".into())]);
        assert_eq!(urls("[https://example.org/a]"), [("https://example.org/a", "https://example.org/a".into())]);
    }

    #[test]
    fn every_url_on_a_line() {
        assert_eq!(
            urls("http://a.example and https://b.example/x, ok"),
            [("http://a.example", "http://a.example".into()), ("https://b.example/x", "https://b.example/x".into())]
        );
        // Ranges are bytes into the text, past other scripts.
        let text = "é https://a.example/ü é";
        assert_eq!(find(text)[0].range, 3..23);
    }

    #[test]
    fn bare_www_opens_over_https() {
        assert_eq!(urls("try www.example.org."), [("www.example.org", "https://www.example.org".into())]);
        assert_eq!(urls("WWW.Example.org/a"), [("WWW.Example.org/a", "https://WWW.Example.org/a".into())]);
        assert_eq!(urls("https://www.example.org"), [("https://www.example.org", "https://www.example.org".into())]);
    }

    #[test]
    fn no_link_without_a_scheme_or_www() {
        for text in ["foo.bar", "see foo.bar/baz", "a.www.example", "xhttps://example.org", "https://", "https:// x", "www.", "www..x", "ftp://example.org", ""]
        {
            assert_eq!(find(text), [], "{text:?}");
        }
    }

    #[test]
    fn only_http_and_https_open() {
        assert!(openable("https://example.org"));
        assert!(openable("HTTP://example.org"));
        assert!(!openable("file:///etc/passwd"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("www.example.org"));
    }

    #[test]
    fn a_click_is_a_press_that_stays_put() {
        assert!(is_click((10., 10.), (10., 10.), 4.));
        assert!(is_click((10., 10.), (13., 10.), 4.));
        assert!(!is_click((10., 10.), (15., 10.), 4.));
        assert!(!is_click((10., 10.), (10., 30.), 4.));
    }
}
