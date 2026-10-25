//! The selected buffer: a title bar (name, network or state, topic) and
//! its lines.
//!
//! A message line is four columns on the monospace grid: the time, a nick
//! column (nicks right-aligned against the text, irssi/weechat style), and
//! the text, which wraps with a hanging indent on its own column. The nick
//! column says what kind of line it is without relying on colour: a nick
//! for a message, `-nick-` for a notice, `*` for an action, and a marker
//! for events (`-->` joins, `<--` parts and quits, `--` the rest, `=!=`
//! errors).

use crate::app::HircApp;
use crate::metrics::{Metrics, TIME_CELLS};
use crate::theme::Palette;
use gpui::{AnyElement, FontWeight, HighlightStyle, Hsla, IntoElement, SharedString, StyledText, TextRun, Window, div, prelude::*, px};
use hirc_core::display::{self, Part};
use hirc_core::event::{ConnStatus, Target};
use hirc_core::store::{Line, LineKind};
use std::collections::HashMap;

/// The title bar, one field rhythm for every window: the window's
/// identity (its name, in bold, and the network tag as the window list
/// shows it, a cell apart), then two cells on, its state when it is not
/// simply usable (a mark and a word, then what to do about it), then two
/// cells on, the topic (or, for a server, why it went down). A state reads
/// the same in a channel as in its server buffer, and only the state is
/// ever red. Each field is cut to fit with an ellipsis that ends on the
/// bar's padding, and the name never takes more than `NAME_SHARE` while
/// there is more to show. `pane` is the width of the buffer pane.
pub fn title(app: &HircApp, window: &Window, pane: f32) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    let now = app.store.clock.unwrap_or_else(chrono::Utc::now);
    let (name, tag, state, rest): (String, Option<Piece>, Vec<Piece>, Vec<Piece>) = match app.current() {
        Some((net, buf)) => match &buf.target {
            Target::Server => {
                let state = match &net.status {
                    ConnStatus::Connected { nick } => vec![(format!("● connected as {nick}"), p.success)],
                    ConnStatus::Connecting | ConnStatus::Idle => vec![("● connecting".to_owned(), p.text_dim)],
                    _ => network_state(p, net, now),
                };
                let reason = match &net.status {
                    ConnStatus::Disconnected { reason, .. } | ConnStatus::Failed { reason } if !reason.is_empty() => vec![(reason.clone(), p.topic)],
                    _ => vec![],
                };
                (net.name.clone(), None, state, reason)
            }
            target => {
                let channel = matches!(target, Target::Channel(_));
                let state = match () {
                    _ if net.status.is_down() => network_state(p, net, now),
                    _ if channel && !buf.joined => vec![("○ not joined".to_owned(), p.text_dim), (" — /join to rejoin".to_owned(), p.text_dim)],
                    _ => vec![],
                };
                let topic = match &buf.topic {
                    Some(t) if channel && !t.is_empty() => vec![(t.clone(), p.topic)],
                    _ if channel && state.is_empty() => vec![("(no topic)".to_owned(), p.topic)],
                    _ => vec![],
                };
                (target.name().unwrap_or_default().to_owned(), Some((net.name.clone(), p.text_dim)), state, topic)
            }
        },
        None => ("No servers configured".to_owned(), None, vec![], app.hint().map(|h| (h.to_owned(), p.topic)).into_iter().collect()),
    };
    let bold = gpui::Font { weight: FontWeight::BOLD, ..app.font.clone() };
    let measure = |s: &str, font: &gpui::Font| {
        let run = TextRun { len: s.len(), font: font.clone(), color: p.text, background_color: None, underline: None, strikethrough: None };
        let line = window.text_system().shape_line(s.to_owned().into(), px(m.size), &[run], None);
        move |i: usize| f32::from(line.x_for_index(i))
    };
    let fit = |s: &str, font: &gpui::Font, max: f32| -> (String, f32) {
        let x = measure(s, font);
        let ellipsis = measure("…", font)(3);
        match cut(s, &x, max, ellipsis) {
            None => (s.to_owned(), x(s.len())),
            Some(i) => {
                let head = s[..i].trim_end();
                (format!("{head}…"), x(head.len()) + ellipsis)
            }
        }
    };
    let inner = (pane - 2. * m.gutter).max(0.);
    // One cell inside the window's identity (name, network), two between
    // fields, so neither the state nor the topic reads as part of what
    // comes before it.
    let gap = m.grid(1);
    let field_gap = m.grid(2);
    let (tag, tag_w) = match tag {
        Some((t, c)) => {
            let (t, w) = fit(&t, &app.font, (inner * TAG_SHARE).max(m.tag_max()));
            (Some((t, c)), w + gap)
        }
        None => (None, 0.),
    };
    let more = !state.is_empty() || !rest.is_empty();
    let name_max = if more { inner * NAME_SHARE } else { inner - tag_w };
    let (name, name_w) = fit(&name, &bold, name_max.max(0.));
    // A field's pieces, cut as one line: each keeps its colour up to the
    // cut, and the ellipsis takes the colour it ends.
    let field = |pieces: &[Piece], max: f32| -> Option<(StyledText, f32)> {
        if pieces.is_empty() {
            return None;
        }
        let text: String = pieces.iter().map(|(t, _)| t.as_str()).collect();
        let (shown, w) = fit(&text, &app.font, max.max(0.));
        let head = if shown == text { text.len() } else { shown.len() - '…'.len_utf8() };
        let mut runs = Runs::default();
        for (s, color) in clip_pieces(pieces, head) {
            runs.push(&s, color, false);
        }
        Some((runs.element(), w))
    };
    let left = inner - name_w - tag_w;
    let state = field(&state, left - field_gap);
    let left = left - state.as_ref().map_or(0., |(_, w)| w + field_gap);
    let rest = field(&rest, left - field_gap);
    let part = || div().flex_none().whitespace_nowrap().overflow_hidden();
    div()
        .flex()
        .px(px(m.gutter))
        .py(px(m.bar_pad()))
        .bg(p.title_bg)
        .border_b_1()
        .border_color(p.border)
        .child(part().text_color(p.text).font_weight(FontWeight::BOLD).child(name))
        .children(tag.map(|(t, c)| part().ml(px(gap)).text_color(c).child(t)))
        .children(state.map(|(t, _)| part().ml(px(field_gap)).child(t)))
        .children(rest.map(|(t, _)| part().flex_shrink(1.).min_w(px(0.)).ml(px(field_gap)).child(t)))
}

/// A down network's state, as every header says it: a mark and a word
/// (red only for a fault; a ring while there is no connection to wait
/// on, a dot once hirc has given up), then what happens next or what to
/// do, counting down to a retry.
fn network_state(p: &Palette, net: &hirc_core::store::Network, now: chrono::DateTime<chrono::Utc>) -> Vec<Piece> {
    let (mark, color, next) = match &net.status {
        ConnStatus::Disconnected { .. } => match net.retry_in(now) {
            Some(0) | None => ("○ disconnected", p.error, "reconnecting…".to_owned()),
            Some(s) => ("○ disconnected", p.error, format!("reconnecting in {s}s")),
        },
        ConnStatus::Failed { .. } => ("● failed", p.error, "/reconnect to try again".to_owned()),
        _ => ("○ offline", p.text_dim, "/reconnect".to_owned()),
    };
    vec![(mark.to_owned(), color), (format!(" — {next}"), p.text_dim)]
}

/// `pieces` joined and cut after byte `head` of the joined text: each
/// piece keeps its colour up to the cut, and an ellipsis in the colour of
/// the piece it cuts ends the line.
fn clip_pieces<C: Copy>(pieces: &[(String, C)], head: usize) -> Vec<(String, C)> {
    let mut out = Vec::new();
    let mut at = 0;
    for (piece, color) in pieces {
        let end = head.min(at + piece.len()).saturating_sub(at);
        if end > 0 {
            out.push((piece[..end].to_owned(), *color));
        }
        at += piece.len();
        if at > head {
            out.push(("…".to_owned(), *color));
            break;
        }
    }
    out
}

/// A piece of the title bar: its text and colour.
type Piece = (String, Hsla);

/// The most of the title bar the name takes while a topic is shown.
const NAME_SHARE: f32 = 0.4;
/// The most of the title bar the network tag takes.
const TAG_SHARE: f32 = 0.2;

/// Where to cut `s` so it and an `ellipsis` wide ellipsis fit in `max`,
/// given `x(i)`, the advance up to byte `i`: the last char boundary that
/// leaves room, or `None` when the whole of `s` fits.
fn cut(s: &str, x: impl Fn(usize) -> f32, max: f32, ellipsis: f32) -> Option<usize> {
    if x(s.len()) <= max {
        return None;
    }
    Some(s.char_indices().map(|(i, _)| i).take_while(|&i| x(i) + ellipsis <= max).last().unwrap_or(0))
}

/// A config that could not be used: a bar like the title bar, with the
/// error marker message lines use (`=!=`), in the error role, and a quiet
/// note on how to put it away (the server buffer keeps a copy).
pub fn banner(app: &HircApp, message: String) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    div()
        .flex()
        .px(px(m.gutter))
        .py(px(m.bar_pad()))
        .bg(p.title_bg)
        .border_b_1()
        .border_color(p.border)
        .text_color(p.error)
        .child(div().flex_none().child(display::marker(LineKind::Error)))
        .child(div().flex_1().min_w(px(0.)).ml(px(m.cell)).child({
            let mut runs = Runs::default();
            runs.push(&message, p.error, false).push(" (Esc in NORMAL mode to dismiss)", p.text_dim, false);
            runs.element()
        }))
}

/// `s` in at most `cells` characters, ending in an ellipsis when cut.
pub fn fit(s: &str, cells: usize) -> String {
    if s.chars().count() <= cells {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(cells.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// What a line needs to know about its buffer.
pub struct Context<'a> {
    /// Our nick on this network.
    pub me: Option<&'a str>,
    /// Width of the nick column, in cells.
    pub nick_cells: usize,
    /// The channel, in a channel's buffer (join and part lines name it).
    pub channel: Option<&'a str>,
    /// Each nick's colour slot in this buffer (`theme::slots`).
    pub slots: &'a HashMap<String, usize>,
}

/// Text with coloured (and possibly bold) runs.
#[derive(Default)]
struct Runs {
    text: String,
    runs: Vec<(std::ops::Range<usize>, HighlightStyle)>,
}

impl Runs {
    fn push(&mut self, s: &str, color: Hsla, bold: bool) -> &mut Runs {
        let start = self.text.len();
        self.text.push_str(s);
        let weight = bold.then_some(FontWeight::BOLD);
        self.runs.push((start..self.text.len(), HighlightStyle { color: Some(color), font_weight: weight, ..Default::default() }));
        self
    }

    fn element(self) -> StyledText {
        StyledText::new(self.text).with_highlights(self.runs)
    }
}

/// A nick, as the nick column or an event shows it: ours in the theme's
/// nickname colour and bold, anyone else's in their own colour.
fn nick_run(runs: &mut Runs, p: &Palette, nick: &str, cx: &Context) {
    if cx.me.is_some_and(|me| me.eq_ignore_ascii_case(nick)) {
        runs.push(nick, p.nickname, true);
    } else {
        runs.push(nick, p.nick(nick, cx.slots), false);
    }
}

pub fn line(app: &HircApp, line: &Line, cx: &Context) -> AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let time = line.time.with_timezone(&chrono::Local).format("%H:%M").to_string();
    let nick = line.nick.as_deref().unwrap_or_default();
    let cells = cx.nick_cells;
    let own = line.own || cx.me.is_some_and(|me| me.eq_ignore_ascii_case(nick));
    let mention =
        !own && matches!(line.kind, LineKind::Privmsg | LineKind::Action | LineKind::Notice) && cx.me.is_some_and(|me| display::mentions(&line.text, me));
    let (mut prefix, mut body) = (Runs::default(), Runs::default());
    // A nick cut to fit the column; the whole of it shows on hover.
    let mut cut = None;
    let mut fit_nick = |cells: usize| {
        let shown = fit(nick, cells);
        if shown != nick {
            cut = Some(nick.to_owned());
        }
        shown
    };
    match line.kind {
        LineKind::Privmsg => {
            nick_run(&mut prefix, p, &fit_nick(cells), cx);
            body.push(&line.text, p.text, false);
        }
        LineKind::Notice if !nick.is_empty() => {
            prefix.push("-", p.text_dim, false);
            nick_run(&mut prefix, p, &fit_nick(cells.saturating_sub(2).max(1)), cx);
            prefix.push("-", p.text_dim, false);
            body.push(&line.text, p.text, false);
        }
        LineKind::Action => {
            prefix.push("*", p.action, false);
            nick_run(&mut body, p, nick, cx);
            body.push(" ", p.action, false).push(&line.text, p.action, false);
        }
        LineKind::Error => {
            prefix.push(display::marker(line.kind), p.error, false);
            body.push(&line.text, p.error, false);
        }
        // A change of connection state: status, so it reads above the
        // server's own chatter (`--`, quiet text) but is not an error.
        LineKind::Status => {
            prefix.push(display::marker(line.kind), p.server, false);
            body.push(&line.text, p.text, false);
        }
        LineKind::Server | LineKind::Notice => {
            prefix.push(display::marker(line.kind), p.server, false);
            body.push(&line.text, p.text_dim, false);
        }
        kind => {
            prefix.push(display::marker(kind), p.server, false);
            // The marker carries the event's colour; the client's own
            // wording is quiet, so what changed reads first. Membership
            // noise (joins, parts, quits, renames) recedes to the timestamp
            // role; changes to the channel (kicks, topics, modes) keep
            // their wording in text.secondary. Nicks are nick-coloured, a
            // reason is text.secondary, a new topic is in the topic role
            // (as the title bar shows it) and mode arguments are text.
            let noise = display::is_noise(kind);
            let verb = if noise { p.timestamp } else { p.text_dim };
            let free = match kind {
                LineKind::Topic => p.topic,
                LineKind::Mode => p.text,
                _ => p.text_dim,
            };
            match display::event_parts(kind, line.nick.as_deref(), &line.text, cx.channel) {
                Some(parts) => {
                    for (part, s) in parts {
                        match part {
                            Part::Nick => nick_run(&mut body, p, &s, cx),
                            Part::Verb | Part::Channel => _ = body.push(&s, verb, false),
                            Part::Free => _ = body.push(&s, free, false),
                        }
                    }
                }
                None => _ = body.push(&line.text, verb, false),
            }
        }
    }
    // `w_full`: gpui's list measures each row as a root with a definite width,
    // and an auto-width flex root would size to its content and never wrap.
    // Rows have no vertical padding: every row is whole text lines, so the
    // view scrolls on one line grid, as a terminal does.
    div()
        .flex()
        .w_full()
        .min_w(px(0.))
        .px(px(m.gutter))
        .when(mention, |d| d.bg(p.highlight))
        .child(div().flex_none().w(px(m.grid(TIME_CELLS))).whitespace_nowrap().text_color(if mention { p.text_dim } else { p.timestamp }).child(time))
        .child(nick_column(app, cells, prefix, cut.map(|full| (full, line.seq))))
        .child(div().flex_1().min_w(px(0.)).ml(px(m.cell)).child(body.element()))
        .into_any_element()
}

/// `line` with the read marker above it: where the unread lines began
/// when the window was entered (weechat's read marker). One row on the
/// line grid: "new" in the unread colour on the nick column's right edge,
/// as markers sit, and a hairline in the separators' colour across the
/// text column.
pub fn with_marker(app: &HircApp, line: AnyElement) -> AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let rule = div().flex_1().min_w(px(0.)).ml(px(m.cell)).h(px(m.line)).flex().items_center().child(div().w_full().h(px(1.)).bg(p.border));
    let marker = div()
        .flex()
        .w_full()
        .px(px(m.gutter))
        .child(div().flex_none().w(px(m.grid(TIME_CELLS))))
        .child(div().flex_none().flex().justify_end().w(px(m.grid(app.nick_cells()))).ml(px(m.cell)).text_color(p.unread).child("new"))
        .child(rule);
    div().flex().flex_col().w_full().child(marker).child(line).into_any_element()
}

/// Drawn over the top row when it is the wrapped tail of `line`: the
/// time and nick of the message it belongs to, all in the timestamp role,
/// so the row says whose it is without reading as a new message.
pub fn sticky(app: &HircApp, line: &Line, cells: usize) -> gpui::AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let time = line.time.with_timezone(&chrono::Local).format("%H:%M").to_string();
    let nick = line.nick.as_deref().unwrap_or_default();
    let who = match line.kind {
        LineKind::Privmsg => fit(nick, cells),
        LineKind::Notice if !nick.is_empty() => format!("-{}-", fit(nick, cells.saturating_sub(2).max(1))),
        LineKind::Action => "*".to_owned(),
        kind => display::marker(kind).to_owned(),
    };
    let mut prefix = Runs::default();
    prefix.push(&who, p.timestamp, false);
    div()
        .flex()
        .w_full()
        .px(px(m.gutter))
        .text_color(p.timestamp)
        .child(div().flex_none().w(px(m.grid(TIME_CELLS))).whitespace_nowrap().child(time))
        .child(nick_column(app, cells, prefix, None))
        .into_any_element()
}

/// The nick column, `cells` wide, nicks right-aligned against the text.
/// `cut` is the whole of a nick the column shortened (and the line's
/// seq, for a stable id): hovering the column shows it.
fn nick_column(app: &HircApp, cells: usize, prefix: Runs, cut: Option<(String, u64)>) -> AnyElement {
    let m = &app.metrics;
    let column = div().flex_none().flex().justify_end().overflow_hidden().whitespace_nowrap().w(px(m.grid(cells))).ml(px(m.cell)).child(prefix.element());
    match cut {
        None => column.into_any_element(),
        Some((full, seq)) => {
            let tip = Tip { text: full.into(), palette: app.palette.clone(), metrics: app.metrics, font: app.font.family.clone() };
            column.id(SharedString::from(format!("nick-{seq}"))).tooltip(move |_, cx| cx.new(|_| tip.clone()).into()).into_any_element()
        }
    }
}

/// A hover note in the title bar's colours: one line of text.
#[derive(Clone)]
struct Tip {
    text: SharedString,
    palette: Palette,
    metrics: Metrics,
    font: SharedString,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let (p, m) = (&self.palette, &self.metrics);
        div()
            .font_family(self.font.clone())
            .text_size(px(m.size))
            .line_height(px(m.line))
            .px(px(m.cell))
            .bg(p.title_bg)
            .border_1()
            .border_color(p.border)
            .text_color(p.text)
            .whitespace_nowrap()
            .child(self.text.clone())
    }
}

/// The bottom row while the view is scrolled away from the newest line:
/// how many rows are below (the `j` presses back to the bottom), irssi's
/// `-- more --` style. The count takes the unread colour once new lines
/// have arrived below. The viewport gives it a row of its own.
pub fn more(app: &HircApp, below: usize, top: bool, fresh: bool) -> AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let mut runs = Runs::default();
    runs.push(if top { "-- top: " } else { "-- more: " }, p.text_dim, false).push(&below.to_string(), if fresh { p.unread } else { p.text_dim }, fresh).push(
        if below == 1 { " line below --" } else { " lines below --" },
        p.text_dim,
        false,
    );
    div().w_full().h(px(m.line)).px(px(m.gutter)).child(runs.element()).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_nicks_end_in_an_ellipsis() {
        assert_eq!(fit("bob", 12), "bob");
        assert_eq!(fit("a_really_long_nickname", 12), "a_really_lo…");
        assert_eq!(fit("a_really_long_nickname", 12).chars().count(), 12);
        assert_eq!(fit("abc", 3), "abc");
        assert_eq!(fit("abcd", 1), "…");
    }

    #[test]
    fn slot_pieces_keep_their_colours_to_the_cut() {
        let pieces = [("(not joined)".to_owned(), 1), (" a topic".to_owned(), 2)];
        let whole = clip_pieces(&pieces, 20);
        assert_eq!(whole, [("(not joined)".to_owned(), 1), (" a topic".to_owned(), 2)]);
        // Cut inside the topic: the ellipsis takes the topic's colour.
        assert_eq!(clip_pieces(&pieces, 15), [("(not joined)".to_owned(), 1), (" a ".to_owned(), 2), ("…".to_owned(), 2)]);
        // Cut inside the state: the topic never shows.
        assert_eq!(clip_pieces(&pieces, 4), [("(not".to_owned(), 1), ("…".to_owned(), 1)]);
    }

    #[test]
    fn cuts_leave_room_for_the_ellipsis() {
        // A monospace face: 10px a char.
        let x = |i: usize| i as f32 * 10.;
        assert_eq!(cut("abcdef", x, 60., 10.), None);
        assert_eq!(cut("abcdef", x, 59., 10.), Some(4));
        assert_eq!(cut("abcdef", x, 50., 10.), Some(4));
        assert_eq!(cut("abcdef", x, 5., 10.), Some(0));
        // Never inside a multibyte char.
        let s = "aé€b";
        let i = cut(s, |i| s[..i].chars().count() as f32 * 10., 25., 10.).unwrap();
        assert!(s.is_char_boundary(i) && i == 1);
    }
}
