//! The selected buffer: a title bar (name and topic) and its lines.
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
use gpui::{AnyElement, FontWeight, HighlightStyle, Hsla, IntoElement, StyledText, div, prelude::*, px};
use hirc_core::display::{self, Part};
use hirc_core::store::{Line, LineKind};

pub fn title(app: &HircApp) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    let (name, topic) = match app.current() {
        Some((net, buf)) => (buf.target.name().unwrap_or(&net.name).to_owned(), buf.topic.clone().unwrap_or_default()),
        None => ("No servers configured".to_owned(), String::new()),
    };
    div()
        .flex()
        .gap(px(m.gutter))
        .px(px(m.gutter))
        .py(px(m.bar_pad()))
        .bg(p.title_bg)
        .border_b_1()
        .border_color(p.border)
        .child(div().flex_none().text_color(p.text).child(name))
        .child(div().flex_1().truncate().text_color(p.topic).child(topic))
}

/// Cells the nick column needs for `line` (its nick, or its marker).
pub fn prefix_cells(line: &Line) -> usize {
    let nick = || line.nick.as_deref().map_or(0, |n| n.chars().count());
    match line.kind {
        LineKind::Privmsg => nick(),
        LineKind::Notice => nick() + 2,
        LineKind::Action => 1,
        kind => display::marker(kind).len(),
    }
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

/// A nick, as the nick column or an event shows it: ours in the primary
/// text colour and bold, anyone else's in their own colour.
fn nick_run(runs: &mut Runs, p: &Palette, nick: &str, me: Option<&str>) {
    if me.is_some_and(|me| me.eq_ignore_ascii_case(nick)) {
        runs.push(nick, p.text, true);
    } else {
        runs.push(nick, p.nick(nick), false);
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
    match line.kind {
        LineKind::Privmsg => {
            nick_run(&mut prefix, p, &fit(nick, cells), cx.me);
            body.push(&line.text, p.text, false);
        }
        LineKind::Notice if !nick.is_empty() => {
            prefix.push("-", p.text_dim, false);
            nick_run(&mut prefix, p, &fit(nick, cells.saturating_sub(2).max(1)), cx.me);
            prefix.push("-", p.text_dim, false);
            body.push(&line.text, p.text, false);
        }
        LineKind::Action => {
            prefix.push("*", p.action, false);
            nick_run(&mut body, p, nick, cx.me);
            body.push(" ", p.action, false).push(&line.text, p.action, false);
        }
        LineKind::Error => {
            prefix.push(display::marker(line.kind), p.error, false);
            body.push(&line.text, p.error, false);
        }
        LineKind::Server | LineKind::Notice => {
            prefix.push(display::marker(line.kind), p.server, false);
            body.push(&line.text, p.text_dim, false);
        }
        kind => {
            prefix.push(display::marker(kind), p.server, false);
            // Membership noise recedes; changes to the channel keep the
            // event colour. Nicks are nick-coloured and free text (reasons,
            // topics) uses the topic role, so the client's wording stands
            // apart from what people typed.
            let verb = if display::is_noise(kind) { p.text_dim } else { p.server };
            match display::event_parts(kind, line.nick.as_deref(), &line.text, line.own) {
                Some(parts) => {
                    for (part, s) in parts {
                        match part {
                            Part::Nick => nick_run(&mut body, p, &s, cx.me),
                            Part::You => _ = body.push(&s, p.text, true),
                            Part::Verb => _ = body.push(&s, verb, false),
                            Part::Free => _ = body.push(&s, p.topic, false),
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
        .child(nick_column(m, cells, prefix))
        .child(div().flex_1().min_w(px(0.)).ml(px(m.cell)).child(body.element()))
        .into_any_element()
}

fn nick_column(m: &Metrics, cells: usize, prefix: Runs) -> impl IntoElement {
    div().flex_none().flex().justify_end().overflow_hidden().whitespace_nowrap().w(px(m.grid(cells))).ml(px(m.cell)).child(prefix.element())
}

/// The bottom line while the view is scrolled away from the newest line:
/// how many lines are below, irssi's `-- more --` style. The count takes
/// the unread colour once new lines have arrived below.
pub fn more(app: &HircApp, below: usize, top: bool, fresh: bool) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    let mut runs = Runs::default();
    runs.push(if top { "-- top: " } else { "-- more: " }, p.text_dim, false).push(&below.to_string(), if fresh { p.unread } else { p.text_dim }, fresh).push(
        " below --",
        p.text_dim,
        false,
    );
    div().absolute().left_0().right_0().bottom(px(m.edge)).h(px(m.line)).px(px(m.gutter)).bg(p.buffer_bg).child(runs.element())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn l(kind: LineKind, nick: Option<&str>) -> Line {
        Line { seq: 0, time: Utc::now(), msgid: None, kind, nick: nick.map(Into::into), text: String::new(), own: false }
    }

    #[test]
    fn long_nicks_end_in_an_ellipsis() {
        assert_eq!(fit("bob", 12), "bob");
        assert_eq!(fit("a_really_long_nickname", 12), "a_really_lo…");
        assert_eq!(fit("a_really_long_nickname", 12).chars().count(), 12);
        assert_eq!(fit("abc", 3), "abc");
        assert_eq!(fit("abcd", 1), "…");
    }

    #[test]
    fn prefixes_by_kind() {
        assert_eq!(prefix_cells(&l(LineKind::Privmsg, Some("alice"))), 5);
        assert_eq!(prefix_cells(&l(LineKind::Notice, Some("alice"))), 7);
        assert_eq!(prefix_cells(&l(LineKind::Action, Some("alice"))), 1);
        assert_eq!(prefix_cells(&l(LineKind::Join, Some("alice"))), 3);
        assert_eq!(prefix_cells(&l(LineKind::Server, None)), 2);
    }
}
