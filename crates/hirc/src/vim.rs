//! Vim-style modal keys for the input line, as a pure state machine.
//!
//! The input box is the only widget with keyboard focus, so "not typing"
//! means Normal mode in that box. There are three modes:
//! - Insert: every key goes to the text input (`Effect::Pass`), except
//!   Escape, which enters Normal.
//! - Normal: no character is ever inserted. Keys move the cursor, edit the
//!   line, scroll the buffer (`j`/`k`/ctrl-d/ctrl-u/`gg`/`G`) or switch
//!   windows (`gt`/`gT`). The scroll keys live here because there is no
//!   other focused widget to receive them while the user is not typing.
//!   While the text has more than one line, `j` and `k` move between its
//!   lines instead (as in vim, not past the first or last); the other
//!   scroll keys still scroll.
//! - Ex: entered with `:`; typing behaves like Insert and Enter runs the line.
//!
//! This module has no gpui types so it can be unit-tested. `input.rs` converts
//! keystrokes into [`Key`]s and applies the returned [`Effect`]s.

/// A key press as vim sees it: a character, or a named key, with ctrl.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub ch: Option<char>,
    pub named: Option<Named>,
    pub ctrl: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Escape,
    Enter,
    Backspace,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Insert,
    Normal,
    Ex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// Positive scrolls down, toward newer lines.
    Lines(i32),
    /// Positive scrolls down.
    HalfPage(i32),
    Top,
    /// Also means "follow new lines again".
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Next,
    Prev,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The text input should handle this key itself.
    Pass,
    /// Replace the line and put the cursor (a byte offset) at `cursor`.
    Edit {
        text: String,
        cursor: usize,
    },
    Scroll(Scroll),
    Nav(Nav),
    /// Submit the line exactly as Insert-mode Enter does.
    Submit,
    /// Run the ex line (without the leading `:`).
    Ex(String),
    /// Escape, out of Insert or in Normal: put away a notice (the config
    /// banner), so one Esc does it whatever the mode.
    Dismiss,
}

/// What an ex line means. See [`ex`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ex {
    None,
    Quit,
    Window(usize),
    Buffer(String),
    /// Anything else is an IRC-style command with the slash put back.
    Slash(String),
}

pub fn ex(line: &str) -> Ex {
    let line = line.trim();
    match line {
        "" => return Ex::None,
        "q" | "quit" | "qa" => return Ex::Quit,
        _ => {}
    }
    if let Ok(n) = line.parse::<usize>()
        && n >= 1
    {
        return Ex::Window(n);
    }
    for prefix in ["buffer ", "b "] {
        if let Some(name) = line.strip_prefix(prefix) {
            return Ex::Buffer(name.trim().to_owned());
        }
    }
    Ex::Slash(format!("/{line}"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Pending {
    #[default]
    None,
    D,
    C,
    G,
    /// After `ci`.
    CI,
}

#[derive(Debug, Default)]
pub struct Vim {
    pub mode: Mode,
    pending: Pending,
    register: String,
    undo: Option<(String, usize)>,
    /// The line and cursor to come back to when the ex line is abandoned.
    saved: Option<(String, usize)>,
}

impl Vim {
    /// Back to Insert after a submit.
    pub fn reset(&mut self) {
        self.mode = Mode::Insert;
        self.pending = Pending::None;
    }

    /// While the ex line is open: the draft it was opened over, which
    /// comes back when it closes.
    pub fn parked(&self) -> Option<&str> {
        self.saved.as_ref().filter(|_| self.mode == Mode::Ex).map(|(text, _)| text.as_str())
    }

    /// The input is about to show another window's line. Anything tied to
    /// the old line goes; an open ex line is abandoned, and the draft it was
    /// opened over is returned (that, not the ex line, is the old window's).
    pub fn switch_line(&mut self) -> Option<String> {
        self.pending = Pending::None;
        self.undo = None;
        if self.mode != Mode::Ex {
            return None;
        }
        self.mode = Mode::Normal;
        Some(self.saved.take().map(|(text, _)| text).unwrap_or_default())
    }

    pub fn key(&mut self, key: &Key, text: &str, cursor: usize) -> Vec<Effect> {
        match self.mode {
            Mode::Insert => self.insert(key, text, cursor),
            Mode::Normal => self.normal(key, text, cursor),
            Mode::Ex => self.ex_key(key, text),
        }
    }

    fn insert(&mut self, key: &Key, text: &str, cursor: usize) -> Vec<Effect> {
        if key.named != Some(Named::Escape) {
            return vec![Effect::Pass];
        }
        self.mode = Mode::Normal;
        self.pending = Pending::None;
        let cursor = clamp_normal(text, prev_char(text, cursor));
        vec![edit(text, cursor), Effect::Dismiss]
    }

    fn ex_key(&mut self, key: &Key, text: &str) -> Vec<Effect> {
        match key.named {
            Some(Named::Escape) => self.leave_ex(),
            Some(Named::Backspace) if text == ":" => self.leave_ex(),
            Some(Named::Enter) => {
                // Run the line, then bring the unsent draft back: vim never loses text.
                let line = text.strip_prefix(':').unwrap_or(text).to_owned();
                let mut effects = vec![Effect::Ex(line)];
                effects.extend(self.leave_ex());
                effects
            }
            _ => vec![Effect::Pass],
        }
    }

    fn leave_ex(&mut self) -> Vec<Effect> {
        self.mode = Mode::Normal;
        let (text, cursor) = self.saved.take().unwrap_or_default();
        let cursor = clamp_normal(&text, cursor);
        vec![Effect::Edit { text, cursor }]
    }

    fn normal(&mut self, key: &Key, text: &str, cursor: usize) -> Vec<Effect> {
        let pending = std::mem::take(&mut self.pending);
        let cursor = clamp_normal(text, cursor);
        if key.named == Some(Named::Enter) {
            self.reset();
            return vec![Effect::Submit];
        }
        // Escape cancels a pending prefix; with none, it puts a notice away.
        if key.named == Some(Named::Escape) {
            return if pending == Pending::None { vec![Effect::Dismiss] } else { vec![] };
        }
        if key.ctrl {
            return match key.ch {
                Some('d') => vec![Effect::Scroll(Scroll::HalfPage(1))],
                Some('u') => vec![Effect::Scroll(Scroll::HalfPage(-1))],
                _ => vec![],
            };
        }
        let c = match (key.ch, key.named) {
            (Some(c), _) => c,
            (None, Some(Named::Backspace)) => 'h',
            _ => return vec![],
        };
        match pending {
            Pending::None => self.normal_plain(c, text, cursor),
            Pending::D => match c {
                'd' => self.change(text, cursor, 0..text.len(), text.to_owned(), false),
                'w' => {
                    let end = next_word_start(text, cursor);
                    self.change(text, cursor, cursor..end, text[cursor..end].to_owned(), false)
                }
                _ => vec![],
            },
            Pending::C => match c {
                'c' => self.change(text, cursor, 0..text.len(), text.to_owned(), true),
                'w' => {
                    let r = word_run(text, cursor);
                    self.change(text, cursor, cursor..r.end, text[cursor..r.end].to_owned(), true)
                }
                'i' => {
                    self.pending = Pending::CI;
                    vec![]
                }
                _ => vec![],
            },
            Pending::CI => match c {
                'w' => {
                    let r = word_run(text, cursor);
                    self.change(text, cursor, r.clone(), text[r].to_owned(), true)
                }
                _ => vec![],
            },
            Pending::G => match c {
                'g' => vec![Effect::Scroll(Scroll::Top)],
                't' => vec![Effect::Nav(Nav::Next)],
                'T' => vec![Effect::Nav(Nav::Prev)],
                _ => vec![],
            },
        }
    }

    fn normal_plain(&mut self, c: char, text: &str, cursor: usize) -> Vec<Effect> {
        let moved = |to: usize| vec![edit(text, clamp_normal(text, to))];
        match c {
            'h' => moved(prev_char(text, cursor)),
            'l' => moved(next_char(text, cursor)),
            '0' => moved(0),
            '$' => moved(text.len()),
            'w' => moved(next_word_start(text, cursor)),
            'b' => moved(prev_word_start(text, cursor)),
            'e' => moved(word_end(text, cursor)),
            'i' => self.enter_insert(text, cursor),
            'a' => self.enter_insert(text, if text.is_empty() { 0 } else { next_char(text, cursor) }),
            'I' => self.enter_insert(text, 0),
            'A' => self.enter_insert(text, text.len()),
            'x' if !text.is_empty() => {
                let end = next_char(text, cursor);
                self.change(text, cursor, cursor..end, text[cursor..end].to_owned(), false)
            }
            'D' if !text.is_empty() => self.change(text, cursor, cursor..text.len(), text[cursor..].to_owned(), false),
            'C' => self.change(text, cursor, cursor..text.len(), text[cursor..].to_owned(), true),
            'p' => self.paste(text, cursor, if text.is_empty() { 0 } else { next_char(text, cursor) }),
            'P' => self.paste(text, cursor, cursor),
            'u' => match self.undo.take() {
                Some((t, c)) => {
                    let c = clamp_normal(&t, c);
                    vec![Effect::Edit { text: t, cursor: c }]
                }
                None => vec![],
            },
            'd' => self.prefix(Pending::D),
            'c' => self.prefix(Pending::C),
            'g' => self.prefix(Pending::G),
            'j' | 'k' if text.contains('\n') => line_step(text, cursor, c == 'j').map_or_else(Vec::new, moved),
            'j' => vec![Effect::Scroll(Scroll::Lines(1))],
            'k' => vec![Effect::Scroll(Scroll::Lines(-1))],
            'G' => vec![Effect::Scroll(Scroll::Bottom)],
            ':' => {
                self.saved = Some((text.to_owned(), cursor));
                self.mode = Mode::Ex;
                vec![Effect::Edit { text: ":".into(), cursor: 1 }]
            }
            _ => vec![],
        }
    }

    fn prefix(&mut self, p: Pending) -> Vec<Effect> {
        self.pending = p;
        vec![]
    }

    fn enter_insert(&mut self, text: &str, cursor: usize) -> Vec<Effect> {
        self.mode = Mode::Insert;
        vec![edit(text, cursor)]
    }

    /// Delete `range`, remember it in the register and snapshot for undo.
    fn change(&mut self, text: &str, cursor: usize, range: std::ops::Range<usize>, removed: String, insert: bool) -> Vec<Effect> {
        if range.is_empty() && !insert {
            return vec![];
        }
        self.undo = Some((text.to_owned(), cursor));
        self.register = removed;
        let new = format!("{}{}", &text[..range.start], &text[range.end..]);
        let at = range.start;
        if insert {
            self.mode = Mode::Insert;
            vec![Effect::Edit { text: new, cursor: at }]
        } else {
            let cursor = clamp_normal(&new, at);
            vec![Effect::Edit { text: new, cursor }]
        }
    }

    fn paste(&mut self, text: &str, cursor: usize, at: usize) -> Vec<Effect> {
        if self.register.is_empty() {
            return vec![];
        }
        self.undo = Some((text.to_owned(), cursor));
        let new = format!("{}{}{}", &text[..at], self.register, &text[at..]);
        let cursor = clamp_normal(&new, at + self.register.len());
        vec![Effect::Edit { text: new, cursor }]
    }
}

fn edit(text: &str, cursor: usize) -> Effect {
    Effect::Edit { text: text.to_owned(), cursor }
}

/// In Normal mode the cursor sits on a char: clamp to the start of the last
/// char (0 when empty) and onto a char boundary.
pub fn clamp_normal(text: &str, cursor: usize) -> usize {
    let last = text.char_indices().next_back().map_or(0, |(i, _)| i);
    let mut c = cursor.min(last);
    while !text.is_char_boundary(c) {
        c -= 1;
    }
    c
}

/// The cursor one line down (or up) in a text of several lines, in the
/// same column (in chars) or at the end of a shorter line; `None` on the
/// last (or first) line.
fn line_step(text: &str, at: usize, down: bool) -> Option<usize> {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let column = text[start..at].chars().count();
    let (from, to) = if down {
        let end = at + text[at..].find('\n')?;
        (end + 1, text[end + 1..].find('\n').map_or(text.len(), |i| end + 1 + i))
    } else {
        let end = start.checked_sub(1)?;
        (text[..end].rfind('\n').map_or(0, |i| i + 1), end)
    };
    Some(text[from..to].char_indices().nth(column).map_or(to, |(i, _)| from + i))
}

fn prev_char(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].char_indices().next_back().map_or(0, |(i, _)| i)
}

fn next_char(text: &str, at: usize) -> usize {
    match text.get(at..).and_then(|s| s.chars().next()) {
        Some(c) => at + c.len_utf8(),
        None => text.len(),
    }
}

/// 0: whitespace, 1: word (alphanumeric or `_`), 2: other.
fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

fn char_at(text: &str, at: usize) -> Option<char> {
    text.get(at..).and_then(|s| s.chars().next())
}

/// Start of the next word, or `text.len()` (unclamped; `dw` needs that).
fn next_word_start(text: &str, at: usize) -> usize {
    let mut i = at;
    if let Some(c) = char_at(text, i)
        && class(c) != 0
    {
        let k = class(c);
        while char_at(text, i).is_some_and(|c| class(c) == k) {
            i = next_char(text, i);
        }
    }
    while char_at(text, i).is_some_and(|c| class(c) == 0) {
        i = next_char(text, i);
    }
    i
}

fn prev_word_start(text: &str, at: usize) -> usize {
    let mut i = prev_char(text, at);
    while i > 0 && char_at(text, i).is_some_and(|c| class(c) == 0) {
        i = prev_char(text, i);
    }
    let Some(k) = char_at(text, i).map(class) else { return 0 };
    while i > 0 && char_at(text, prev_char(text, i)).is_some_and(|c| class(c) == k) {
        i = prev_char(text, i);
    }
    i
}

/// Start of the last char of the next word end.
fn word_end(text: &str, at: usize) -> usize {
    let mut i = next_char(text, at);
    while char_at(text, i).is_some_and(|c| class(c) == 0) {
        i = next_char(text, i);
    }
    let Some(k) = char_at(text, i).map(class) else { return at };
    while char_at(text, next_char(text, i)).is_some_and(|c| class(c) == k) {
        i = next_char(text, i);
    }
    i
}

/// The run of same-class chars around `at`, as a byte range.
fn word_run(text: &str, at: usize) -> std::ops::Range<usize> {
    let Some(k) = char_at(text, at).map(class) else { return at..at };
    let mut start = at;
    while start > 0 && char_at(text, prev_char(text, start)).is_some_and(|c| class(c) == k) {
        start = prev_char(text, start);
    }
    let mut end = at;
    while char_at(text, end).is_some_and(|c| class(c) == k) {
        end = next_char(text, end);
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: char) -> Key {
        Key { ch: Some(c), named: None, ctrl: false }
    }
    fn ctrl(c: char) -> Key {
        Key { ch: Some(c), named: None, ctrl: true }
    }
    fn named(n: Named) -> Key {
        Key { ch: None, named: Some(n), ctrl: false }
    }
    fn esc() -> Key {
        named(Named::Escape)
    }

    /// A tiny stand-in for the input box: applies `Edit`s, collects effects.
    struct Sim {
        vim: Vim,
        text: String,
        cursor: usize,
        effects: Vec<Effect>,
    }

    impl Sim {
        fn new(text: &str, cursor: usize) -> Sim {
            Sim { vim: Vim::default(), text: text.into(), cursor, effects: vec![] }
        }
        fn normal(text: &str, cursor: usize) -> Sim {
            let mut s = Sim::new(text, cursor);
            s.vim.mode = Mode::Normal;
            s
        }
        fn feed(&mut self, key: Key) -> &mut Sim {
            for e in self.vim.key(&key, &self.text, self.cursor) {
                if let Effect::Edit { text, cursor } = &e {
                    self.text = text.clone();
                    self.cursor = *cursor;
                }
                self.effects.push(e);
            }
            self
        }
        fn keys(&mut self, s: &str) -> &mut Sim {
            for c in s.chars() {
                self.feed(k(c));
            }
            self
        }
        fn type_str(&mut self, s: &str) -> &mut Sim {
            self.text.insert_str(self.cursor, s);
            self.cursor += s.len();
            self
        }
        fn state(&self) -> (&str, usize, Mode) {
            (&self.text, self.cursor, self.vim.mode)
        }
        fn edits(&self) -> usize {
            self.effects.iter().filter(|e| matches!(e, Effect::Edit { .. })).count()
        }
    }

    #[test]
    fn escape_moves_left_and_enters_normal() {
        assert_eq!(Sim::new("hello", 5).feed(esc()).state(), ("hello", 4, Mode::Normal));
        assert_eq!(Sim::new("hello", 0).feed(esc()).state(), ("hello", 0, Mode::Normal));
        assert_eq!(Sim::new("", 0).feed(esc()).state(), ("", 0, Mode::Normal));
        assert_eq!(Sim::new("a🙂", 5).feed(esc()).state(), ("a🙂", 1, Mode::Normal));
    }

    #[test]
    fn escape_in_normal_dismisses_a_notice() {
        let mut s = Sim::normal("ab", 1);
        s.feed(esc());
        assert_eq!(s.effects, vec![Effect::Dismiss]);
        assert_eq!(s.state(), ("ab", 1, Mode::Normal));
        // Out of Insert, the same one Esc puts it away.
        let mut s = Sim::new("ab", 2);
        s.feed(esc());
        assert_eq!(s.effects.last(), Some(&Effect::Dismiss));
        assert_eq!(s.state(), ("ab", 1, Mode::Normal));
    }

    #[test]
    fn insert_passes_everything_else() {
        let mut s = Sim::new("", 0);
        assert_eq!(s.feed(k('x')).effects, vec![Effect::Pass]);
        assert_eq!(s.feed(named(Named::Enter)).effects.last(), Some(&Effect::Pass));
    }

    #[test]
    fn x_deletes_a_multibyte_char() {
        let mut s = Sim::normal("héllo", 1);
        s.feed(k('x'));
        assert_eq!(s.state(), ("hllo", 1, Mode::Normal));
        assert_eq!(s.vim.register, "é");
    }

    #[test]
    fn x_on_empty_is_nothing() {
        let mut s = Sim::normal("", 0);
        s.feed(k('x'));
        assert!(s.effects.is_empty());
    }

    #[test]
    fn x_at_end_clamps_cursor() {
        assert_eq!(Sim::normal("ab", 1).feed(k('x')).state(), ("a", 0, Mode::Normal));
    }

    #[test]
    fn dollar_and_zero() {
        let mut s = Sim::normal("ab🙂", 0);
        s.feed(k('$'));
        assert_eq!(s.cursor, 2);
        s.feed(k('0'));
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn h_and_l_are_multibyte_safe_and_clamped() {
        let mut s = Sim::normal("a🙂b", 0);
        s.feed(k('l'));
        assert_eq!(s.cursor, 1);
        s.feed(k('l'));
        assert_eq!(s.cursor, 5);
        s.feed(k('l'));
        assert_eq!(s.cursor, 5);
        s.feed(k('h'));
        assert_eq!(s.cursor, 1);
        s.feed(k('h')).feed(k('h'));
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn word_motions() {
        let mut s = Sim::normal("foo bar.baz", 0);
        s.feed(k('w'));
        assert_eq!(s.cursor, 4);
        s.feed(k('w'));
        assert_eq!(s.cursor, 7);
        s.feed(k('w'));
        assert_eq!(s.cursor, 8);
        s.feed(k('b'));
        assert_eq!(s.cursor, 7);
        s.feed(k('b'));
        assert_eq!(s.cursor, 4);
        s.feed(k('b'));
        assert_eq!(s.cursor, 0);
        s.feed(k('b'));
        assert_eq!(s.cursor, 0);
        s.feed(k('e'));
        assert_eq!(s.cursor, 2);
        s.feed(k('e'));
        assert_eq!(s.cursor, 6);
    }

    #[test]
    fn w_at_last_word_clamps_to_last_char() {
        assert_eq!(Sim::normal("foo bar", 4).feed(k('w')).cursor, 6);
    }

    #[test]
    fn dd_and_p() {
        let mut s = Sim::normal("foo bar", 3);
        s.keys("dd");
        assert_eq!(s.state(), ("", 0, Mode::Normal));
        assert_eq!(s.vim.register, "foo bar");
        s.feed(k('p'));
        assert_eq!(s.state(), ("foo bar", 6, Mode::Normal));
    }

    #[test]
    fn p_and_capital_p_position() {
        let mut s = Sim::normal("ab", 0);
        s.keys("x");
        assert_eq!(s.state(), ("b", 0, Mode::Normal));
        s.keys("p");
        assert_eq!(s.text, "ba");
        let mut s = Sim::normal("ab", 0);
        s.keys("xP");
        assert_eq!(s.text, "ab");
    }

    #[test]
    fn p_with_empty_register_is_nothing() {
        assert!(Sim::normal("ab", 0).feed(k('p')).effects.is_empty());
    }

    #[test]
    fn cw_changes_to_word_end() {
        assert_eq!(Sim::normal("foo bar", 0).keys("cw").state(), (" bar", 0, Mode::Insert));
    }

    #[test]
    fn dw_deletes_to_next_word() {
        assert_eq!(Sim::normal("foo bar", 0).keys("dw").state(), ("bar", 0, Mode::Normal));
        assert_eq!(Sim::normal("foo bar", 4).keys("dw").state(), ("foo ", 3, Mode::Normal));
    }

    #[test]
    fn ciw_changes_inner_word() {
        assert_eq!(Sim::normal("foo bar baz", 5).keys("ciw").state(), ("foo  baz", 4, Mode::Insert));
    }

    #[test]
    fn cc_clears_and_inserts() {
        assert_eq!(Sim::normal("foo", 1).keys("cc").state(), ("", 0, Mode::Insert));
    }

    #[test]
    fn capital_d_and_c() {
        assert_eq!(Sim::normal("foo bar", 4).feed(k('D')).state(), ("foo ", 3, Mode::Normal));
        assert_eq!(Sim::normal("foo bar", 4).feed(k('C')).state(), ("foo ", 4, Mode::Insert));
    }

    #[test]
    fn undo_restores_once() {
        let mut s = Sim::normal("hello", 1);
        s.feed(k('x'));
        assert_eq!(s.text, "hllo");
        s.feed(k('u'));
        assert_eq!(s.state(), ("hello", 1, Mode::Normal));
        let edits = s.edits();
        s.feed(k('u'));
        assert_eq!(s.edits(), edits, "no snapshot left");
    }

    #[test]
    fn undo_with_no_snapshot_is_nothing() {
        assert!(Sim::normal("a", 0).feed(k('u')).effects.is_empty());
    }

    #[test]
    fn insert_commands() {
        assert_eq!(Sim::normal("abc", 1).feed(k('A')).state(), ("abc", 3, Mode::Insert));
        assert_eq!(Sim::normal("abc", 1).feed(k('I')).state(), ("abc", 0, Mode::Insert));
        assert_eq!(Sim::normal("ab", 0).feed(k('a')).state(), ("ab", 1, Mode::Insert));
        assert_eq!(Sim::normal("ab", 1).feed(k('i')).state(), ("ab", 1, Mode::Insert));
        assert_eq!(Sim::normal("", 0).feed(k('a')).state(), ("", 0, Mode::Insert));
    }

    #[test]
    fn the_ex_line_shows_the_draft_it_parked() {
        let mut s = Sim::normal("my draft", 2);
        assert_eq!(s.vim.parked(), None);
        s.feed(k(':'));
        assert_eq!(s.vim.parked(), Some("my draft"));
        s.feed(esc());
        assert_eq!(s.vim.parked(), None);
    }

    #[test]
    fn insert_then_escape_round_trip() {
        let mut s = Sim::normal("ab", 0);
        s.feed(k('A')).type_str("c").feed(esc());
        assert_eq!(s.state(), ("abc", 2, Mode::Normal));
    }

    #[test]
    fn scroll_keys() {
        let mut s = Sim::normal("", 0);
        s.keys("jk").feed(ctrl('d')).feed(ctrl('u')).keys("ggG");
        assert_eq!(
            s.effects,
            vec![
                Effect::Scroll(Scroll::Lines(1)),
                Effect::Scroll(Scroll::Lines(-1)),
                Effect::Scroll(Scroll::HalfPage(1)),
                Effect::Scroll(Scroll::HalfPage(-1)),
                Effect::Scroll(Scroll::Top),
                Effect::Scroll(Scroll::Bottom),
            ]
        );
    }

    #[test]
    fn j_and_k_move_between_lines_of_a_multiline_text() {
        // "one\nfour five\nsix": `k`/`j` keep the column, or end a short line.
        let mut s = Sim::normal("one\nfour five\nsix", 6);
        s.keys("k");
        assert_eq!(s.state(), ("one\nfour five\nsix", 2, Mode::Normal));
        s.keys("j");
        assert_eq!(s.cursor, 6);
        s.keys("j");
        assert_eq!(s.cursor, 16);
        // Not past the last line or the first, and the view never scrolls.
        s.keys("j");
        assert_eq!(s.cursor, 16);
        s.keys("kkk");
        assert_eq!(s.cursor, 2);
        assert!(!s.effects.iter().any(|e| matches!(e, Effect::Scroll(_))), "{:?}", s.effects);
        // An empty line in between is a line of its own.
        assert_eq!(line_step("ab\n\ncd", 1, true), Some(3));
        assert_eq!(line_step("ab\n\ncd", 3, true), Some(4));
        assert_eq!(line_step("ab\n\ncd", 4, false), Some(3));
    }

    #[test]
    fn window_nav() {
        let mut s = Sim::normal("", 0);
        s.keys("gtgT");
        assert_eq!(s.effects, vec![Effect::Nav(Nav::Next), Effect::Nav(Nav::Prev)]);
    }

    #[test]
    fn pending_prefix_cancels() {
        let mut s = Sim::normal("abc", 0);
        s.keys("gx");
        assert!(s.effects.is_empty());
        // The prefix is gone: a following `x` is a plain delete.
        s.keys("x");
        assert_eq!(s.text, "bc");
        let mut s = Sim::normal("abc", 0);
        s.keys("dz");
        assert_eq!(s.text, "abc");
        let mut s = Sim::normal("abc", 0);
        s.keys("d").feed(esc());
        assert!(s.effects.is_empty());
        assert_eq!(s.vim.pending, Pending::None);
        let mut s = Sim::normal("abc", 0);
        s.keys("cix");
        assert_eq!(s.state(), ("abc", 0, Mode::Normal));
    }

    #[test]
    fn unknown_letters_do_nothing() {
        let mut s = Sim::normal("abc", 1);
        s.keys("zqy");
        s.feed(ctrl('z'));
        assert!(s.effects.is_empty());
    }

    #[test]
    fn normal_never_passes() {
        let mut s = Sim::normal("abc", 1);
        for key in [k('z'), k('1'), esc(), named(Named::Backspace), ctrl('x')] {
            assert!(!s.feed(key).effects.contains(&Effect::Pass));
        }
    }

    #[test]
    fn normal_enter_submits_and_returns_to_insert() {
        let mut s = Sim::normal("hi", 0);
        s.feed(named(Named::Enter));
        assert_eq!(s.effects, vec![Effect::Submit]);
        assert_eq!(s.vim.mode, Mode::Insert);
    }

    #[test]
    fn ex_escape_restores() {
        let mut s = Sim::normal("draft", 2);
        s.feed(k(':'));
        assert_eq!(s.state(), (":", 1, Mode::Ex));
        s.feed(esc());
        assert_eq!(s.state(), ("draft", 2, Mode::Normal));
    }

    #[test]
    fn ex_backspace_on_lone_colon_restores() {
        let mut s = Sim::normal("draft", 2);
        s.feed(k(':')).feed(named(Named::Backspace));
        assert_eq!(s.state(), ("draft", 2, Mode::Normal));
    }

    #[test]
    fn ex_backspace_elsewhere_passes() {
        let mut s = Sim::normal("draft", 2);
        s.feed(k(':')).feed(k('3'));
        s.type_str("3");
        s.effects.clear();
        s.feed(named(Named::Backspace));
        assert_eq!(s.effects, vec![Effect::Pass]);
        assert_eq!(s.vim.mode, Mode::Ex);
    }

    #[test]
    fn ex_enter_runs_the_line() {
        let mut s = Sim::normal("draft", 2);
        s.feed(k(':'));
        s.effects.clear();
        s.feed(k('3'));
        assert_eq!(s.effects, vec![Effect::Pass]);
        s.type_str("3").feed(named(Named::Enter));
        assert!(s.effects.contains(&Effect::Ex("3".into())));
        assert_eq!(s.vim.mode, Mode::Normal);
    }

    #[test]
    fn ex_enter_keeps_the_draft() {
        let mut s = Sim::new("hello", 5);
        s.feed(esc()).feed(k(':'));
        s.type_str("3").feed(named(Named::Enter));
        assert_eq!(s.effects.iter().filter(|e| matches!(e, Effect::Ex(_))).count(), 1);
        assert!(s.effects.contains(&Effect::Ex("3".into())));
        assert_eq!(s.state(), ("hello", 4, Mode::Normal));
    }

    #[test]
    fn undo_after_paste_restores_original_cursor() {
        let mut s = Sim::normal("ab", 0);
        s.keys("xp");
        assert_eq!(s.text, "ba");
        s.keys("u");
        assert_eq!(s.state(), ("b", 0, Mode::Normal));
    }

    #[test]
    fn ex_table() {
        assert_eq!(ex(""), Ex::None);
        assert_eq!(ex("  "), Ex::None);
        assert_eq!(ex("q"), Ex::Quit);
        assert_eq!(ex("quit"), Ex::Quit);
        assert_eq!(ex("qa"), Ex::Quit);
        assert_eq!(ex("3"), Ex::Window(3));
        assert_eq!(ex("0"), Ex::Slash("/0".into()));
        assert_eq!(ex("b ibm"), Ex::Buffer("ibm".into()));
        assert_eq!(ex("buffer ibm"), Ex::Buffer("ibm".into()));
        assert_eq!(ex("join #x"), Ex::Slash("/join #x".into()));
        assert_eq!(ex("window move 2"), Ex::Slash("/window move 2".into()));
    }

    #[test]
    fn helpers_survive_multibyte_and_empty() {
        for t in ["", "é", "a🙂 b🙂", "  "] {
            for i in 0..=t.len() {
                if !t.is_char_boundary(i) {
                    continue;
                }
                let _ = (next_word_start(t, i), prev_word_start(t, i), word_end(t, i), word_run(t, i), clamp_normal(t, i));
            }
        }
    }

    #[test]
    fn switching_lines_abandons_the_ex_line_for_its_draft() {
        let mut s = Sim::new("my draft", 8);
        s.feed(esc()).feed(k(':'));
        assert_eq!(s.vim.switch_line(), Some("my draft".into()));
        assert_eq!(s.vim.mode, Mode::Normal);
        // Outside ex there is nothing to hand back.
        assert_eq!(s.vim.switch_line(), None);
    }
}
