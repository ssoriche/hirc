//! IRC lines: parsing, serializing and IRCv3 tag escaping. Parsing is
//! forgiving (extra spaces, lowercase commands, unknown tags) and never
//! panics; servers and other clients send all sorts.

/// Longest line we accept: 8191 bytes of tags plus a 512-byte message.
pub const MAX_LINE: usize = 8191 + 512;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    /// In wire order. Values are unescaped; `None` for a tag with no value.
    pub tags: Vec<(String, Option<String>)>,
    pub source: Option<String>,
    /// Uppercased.
    pub command: String,
    pub params: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("empty line")]
    Empty,
    #[error("no command")]
    NoCommand,
    #[error("line too long")]
    TooLong,
}

impl Message {
    pub fn new<I, S>(command: &str, params: I) -> Message
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Message { command: command.to_owned(), params: params.into_iter().map(Into::into).collect(), ..Default::default() }
    }

    /// A tag's value. Tags without a value give `Some("")`. If a key repeats,
    /// the last one wins.
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_deref().unwrap_or(""))
    }

    /// The nick part of `nick!user@host`, or the whole source for servers.
    pub fn nick(&self) -> Option<&str> {
        self.source.as_deref().map(|s| s.split_once('!').map_or(s, |(n, _)| n))
    }

    pub fn parse(line: &str) -> Result<Message, ParseError> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.len() > MAX_LINE {
            return Err(ParseError::TooLong);
        }
        let mut rest = line.trim_start_matches(' ');
        if rest.is_empty() {
            return Err(ParseError::Empty);
        }
        let mut msg = Message::default();
        if let Some(r) = rest.strip_prefix('@') {
            let (tags, r) = r.split_once(' ').unwrap_or((r, ""));
            msg.tags = parse_tags(tags);
            rest = r.trim_start_matches(' ');
        }
        if let Some(r) = rest.strip_prefix(':') {
            let (src, r) = r.split_once(' ').unwrap_or((r, ""));
            msg.source = Some(src.to_owned());
            rest = r.trim_start_matches(' ');
        }
        let (cmd, r) = rest.split_once(' ').unwrap_or((rest, ""));
        if cmd.is_empty() {
            return Err(ParseError::NoCommand);
        }
        msg.command = cmd.to_ascii_uppercase();
        rest = r;
        loop {
            rest = rest.trim_start_matches(' ');
            if rest.is_empty() {
                break;
            }
            if let Some(trailing) = rest.strip_prefix(':') {
                msg.params.push(trailing.to_owned());
                break;
            }
            let (p, r) = rest.split_once(' ').unwrap_or((rest, ""));
            msg.params.push(p.to_owned());
            rest = r;
        }
        Ok(msg)
    }

    /// The line to send, without CRLF. CR, LF and NUL in params become
    /// spaces so a message can never smuggle in a second command.
    pub fn to_wire(&self) -> String {
        let mut s = String::new();
        if !self.tags.is_empty() {
            s.push('@');
            for (i, (k, v)) in self.tags.iter().enumerate() {
                if i > 0 {
                    s.push(';');
                }
                s.push_str(k);
                if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                    s.push('=');
                    s.push_str(&escape_tag_value(v));
                }
            }
            s.push(' ');
        }
        if let Some(src) = &self.source {
            s.push(':');
            s.push_str(src);
            s.push(' ');
        }
        s.push_str(&self.command);
        for (i, p) in self.params.iter().enumerate() {
            let p = p.replace(['\r', '\n', '\0'], " ");
            s.push(' ');
            if i + 1 == self.params.len() && (p.is_empty() || p.contains(' ') || p.starts_with(':')) {
                s.push(':');
            }
            s.push_str(&p);
        }
        s
    }
}

fn parse_tags(s: &str) -> Vec<(String, Option<String>)> {
    s.split(';')
        .filter(|t| !t.is_empty())
        .map(|t| match t.split_once('=') {
            Some((k, v)) => (k.to_owned(), Some(unescape_tag_value(v))),
            None => (t.to_owned(), None),
        })
        .collect()
}

pub fn escape_tag_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            ';' => out.push_str("\\:"),
            ' ' => out.push_str("\\s"),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

pub fn unescape_tag_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_source_command_params() {
        let m = Message::parse("@time=2026-10-22T10:00:00.000Z;msgid=abc;+draft/reply :bob!b@host PRIVMSG #chan :hello there\r\n").unwrap();
        assert_eq!(m.tag("time"), Some("2026-10-22T10:00:00.000Z"));
        assert_eq!(m.tag("msgid"), Some("abc"));
        assert_eq!(m.tag("+draft/reply"), Some(""));
        assert_eq!(m.tag("missing"), None);
        assert_eq!(m.source.as_deref(), Some("bob!b@host"));
        assert_eq!(m.nick(), Some("bob"));
        assert_eq!(m.command, "PRIVMSG");
        assert_eq!(m.params, ["#chan", "hello there"]);
    }

    #[test]
    fn tolerates_extra_spaces_and_lowercase_commands() {
        let m = Message::parse(":srv   001  alice   :Welcome").unwrap();
        assert_eq!(m.command, "001");
        assert_eq!(m.params, ["alice", "Welcome"]);
        assert_eq!(Message::parse("ping :x").unwrap().command, "PING");
    }

    #[test]
    fn empty_trailing_is_kept() {
        assert_eq!(Message::parse("TOPIC #a :").unwrap().params, ["#a", ""]);
    }

    #[test]
    fn tag_escapes_round_trip() {
        let raw = "a;b c\\d\re\nf";
        assert_eq!(escape_tag_value(raw), "a\\:b\\sc\\\\d\\re\\nf");
        assert_eq!(unescape_tag_value(&escape_tag_value(raw)), raw);
        // Unknown escapes drop the backslash; a trailing lone backslash is dropped.
        assert_eq!(unescape_tag_value("\\x\\"), "x");
    }

    #[test]
    fn later_duplicate_tag_wins() {
        assert_eq!(Message::parse("@a=1;a=2 PING").unwrap().tag("a"), Some("2"));
    }

    #[test]
    fn serializes_with_trailing_colon_only_when_needed() {
        assert_eq!(Message::new("PRIVMSG", ["#a", "hi"]).to_wire(), "PRIVMSG #a hi");
        assert_eq!(Message::new("PRIVMSG", ["#a", "hi there"]).to_wire(), "PRIVMSG #a :hi there");
        assert_eq!(Message::new("PRIVMSG", ["#a", ":)"]).to_wire(), "PRIVMSG #a ::)");
        assert_eq!(Message::new("TOPIC", ["#a", ""]).to_wire(), "TOPIC #a :");
        let tagged = Message { tags: vec![("+draft/reply".into(), Some("x y".into())), ("+typing".into(), None)], ..Message::new("TAGMSG", ["#a"]) };
        assert_eq!(tagged.to_wire(), "@+draft/reply=x\\sy;+typing TAGMSG #a");
    }

    #[test]
    fn serializer_strips_line_breaks_from_params() {
        assert_eq!(Message::new("PRIVMSG", ["#a", "one\r\nQUIT"]).to_wire(), "PRIVMSG #a :one  QUIT");
    }

    #[test]
    fn rejects_empty_and_oversized() {
        assert_eq!(Message::parse("\r\n"), Err(ParseError::Empty));
        assert_eq!(Message::parse("@a=b"), Err(ParseError::NoCommand));
        assert_eq!(Message::parse(":src"), Err(ParseError::NoCommand));
        assert_eq!(Message::parse(&"A".repeat(MAX_LINE + 1)), Err(ParseError::TooLong));
    }

    #[test]
    fn never_panics_on_garbage() {
        let samples =
            ["@", ":", "@;;; :", "@=;=x CMD", ": :", "@a\\", "CMD :\\", "\u{0}\u{1}", "@é=ü :ñ PRIVMSG #ü :ü", "CMD a b c d e f g h i j k l m n o p q"];
        for s in samples {
            let _ = Message::parse(s);
        }
        // Every char boundary prefix of a real line.
        let line = "@time=2026-10-22T10:00:00.000Z;msgid=x :bob!b@h PRIVMSG #c :héllo wörld";
        for (i, _) in line.char_indices() {
            let _ = Message::parse(&line[..i]);
        }
    }
}
