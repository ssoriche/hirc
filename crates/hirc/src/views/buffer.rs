//! The selected buffer: a title bar (name, network or state, topic) and
//! its lines.
//!
//! A message line is four columns on the monospace grid: the time, a nick
//! column (nicks right-aligned against the text, irssi/weechat style), and
//! the text, which wraps with a hanging indent on its own column (breaking
//! by `wrap`, so punctuation stays with its word). The nick column is one
//! width in every buffer (it follows the view alone), so the text column
//! and the input never move when you switch windows; a longer nick ends
//! in `…` (its whole shows on hover), so the text column is one edge for
//! every line. In a narrow view the time column goes first. The nick
//! column says what kind of line it is without relying on colour: a nick
//! for a message, `-nick-` for a notice, `*` for an action, and a marker
//! for events (`-->` joins, `<--` parts and quits, `<-x` kicks, `--` the rest, `=!=`
//! errors).
//!
//! Links in what people wrote (see `links`) are underlined in the link
//! colour, on every row they wrap onto, with a pointer over them; a
//! click (a press that does not drag) opens one, and never takes focus
//! from the input or changes its mode.
//!
//! Text can be selected as in a terminal (see `selection`): the selected
//! part of each line is drawn on the input's selection fill, and every
//! line's text registers where it was drawn (`Spot`), so a press or a
//! drag finds the character under it.

use crate::app::HircApp;
use crate::links;
use crate::metrics::{Columns, Metrics};
use crate::theme::Palette;
use crate::views::selection::{self, Log, Mark};
use crate::wrap;
use gpui::{
    AnyElement, FontWeight, HighlightStyle, Hsla, InteractiveText, IntoElement, Pixels, Point, SharedString, StyledText, TextLayout, TextRun, UnderlineStyle,
    Window, div, prelude::*, px,
};
use hirc_core::display::{self, Part};
use hirc_core::event::{ConnStatus, Target};
use hirc_core::store::{Delivery, HistoryState, Line, LineKind, Peer, Presence};
use std::collections::HashMap;

/// The title bar, one field rhythm for every window: the window's
/// identity (its name, in bold, and the network tag as the window list
/// shows it, a cell apart), then two cells on, its state when it is not
/// simply usable (a mark and a word, then what to do about it), then two
/// cells on, the topic (for a query, the peer's `user@host`). The state
/// is the network's when it is down, else a channel's when we are not in
/// it, else a query peer's presence (online, away, quit). A state reads
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
                // Why it went down is the log's to say (it stays true);
                // the header keeps the live state.
                let state = match &net.status {
                    ConnStatus::Connected { nick } => vec![("● connected".to_owned(), p.success), (format!(" as {nick}"), p.text_dim)],
                    ConnStatus::Connecting | ConnStatus::Idle => vec![("● connecting".to_owned(), p.text_dim)],
                    _ => network_state(p, net, now),
                };
                (net.name.clone(), None, state, vec![])
            }
            target => {
                let channel = matches!(target, Target::Channel(_));
                let name = target.name().unwrap_or_default();
                let peer = net.peer(name);
                // The way back in is the input line's to say.
                let state = match () {
                    _ if net.status.is_down() => network_state(p, net, now),
                    _ if channel && !buf.joined => vec![("○ not joined".to_owned(), p.text_dim)],
                    _ if channel => vec![],
                    _ => peer_state(p, peer),
                };
                let topic = match &buf.topic {
                    Some(t) if channel && !t.is_empty() => vec![(t.clone(), p.topic)],
                    // The last-known topic in every state, so the slot
                    // never comes and goes with it.
                    _ if channel => vec![("(no topic)".to_owned(), p.topic)],
                    // A query's third field: who the peer is, when known.
                    _ => match peer.and_then(|peer| peer.host.clone()) {
                        Some(host) => vec![(host, p.topic)],
                        None if state.is_empty() => vec![("private".to_owned(), p.topic)],
                        None => vec![],
                    },
                };
                // The tag in the window list's role whatever the font: with
                // no bold face the name still leads by colour (primary
                // against the quieter tag), and spacing groups the tag
                // with it.
                (target.name().unwrap_or_default().to_owned(), Some((net.name.clone(), p.tag)), state, topic)
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
    // What is left of a line beside the message view, half above the
    // text and half below, as the input bar takes it (see `viewport`).
    let (above, below) = app.bar_extra;
    div()
        .flex()
        .px(px(m.gutter))
        .pt(px(m.bar_pad() + above))
        .pb(px(m.bar_pad() + below))
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

/// A query peer's presence, as a state field: a mark and a word, then
/// what goes with it. Empty while nothing is known (or the network is
/// down, which the network's state says).
fn peer_state(p: &Palette, peer: Option<&Peer>) -> Vec<Piece> {
    let Some(peer) = peer else { return vec![] };
    let detail = |s: &str| (!s.is_empty()).then(|| (format!(" — {s}"), p.text_dim));
    match (&peer.presence, &peer.away) {
        // Away is a state, not identity: its own role (the event colour),
        // apart from the network tag beside it.
        (Presence::Online, Some(away)) => [Some(("● away".to_owned(), p.away)), detail(away)].into_iter().flatten().collect(),
        (Presence::Online, None) => vec![("● online".to_owned(), p.success)],
        (Presence::Quit(reason), _) => [Some(("○ quit".to_owned(), p.text_dim)), detail(reason)].into_iter().flatten().collect(),
        (Presence::Renamed(new), _) => vec![("○ renamed".to_owned(), p.text_dim), (format!(" — now {new}"), p.text_dim)],
        (Presence::Unknown, _) => vec![],
    }
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
/// note on how to put it away (the server buffer keeps a copy). Its first
/// line sits on the window list's first row (as the header's does without
/// it), and it stays whole lines tall, so the header under it keeps the
/// grid too.
pub fn banner(app: &HircApp, message: String) -> impl IntoElement {
    let p = &app.palette;
    let m = &app.metrics;
    let (above, _) = app.bar_extra;
    div()
        .flex()
        .px(px(m.gutter))
        .pt(px(m.bar_pad() + above))
        .pb(px(m.bar_pad() - above))
        .bg(p.title_bg)
        .border_b_1()
        .border_color(p.border)
        .text_color(p.error)
        .child(div().flex_none().child(display::marker(LineKind::Error)))
        .child(div().flex_1().min_w(px(0.)).ml(px(m.cell)).child({
            let mut runs = Runs::default();
            // The marker carries the error, as on a message line; the
            // sentence is text.
            // One Esc puts it away in any mode (see `vim`).
            runs.push(&message, p.text, false).push(" (Esc to dismiss)", p.text_dim, false);
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
    /// The time and nick columns, in cells.
    pub cols: Columns,
    /// Each nick's colour slot in this buffer (`theme::slots`).
    pub slots: &'a HashMap<String, usize>,
    /// What of the line's text is selected (bytes of it unwrapped).
    pub selected: Option<std::ops::Range<usize>>,
}

/// A line's text as drawn this frame: its layout (laid out and placed),
/// where wrapping inserted breaks (`wrap::inserted`) and its length
/// unwrapped. A press or drag finds its character here (`hit`).
#[derive(Clone)]
pub struct Spot {
    pub seq: u64,
    pub layout: TextLayout,
    pub inserted: Vec<usize>,
    pub len: usize,
}

/// The point of the text under `pos` (window px), among the lines drawn
/// (`spots`, top to bottom): on a line's rows, the character boundary
/// there (left of the text is its row's start, right its end); between
/// lines (a rule), the start of the next; below them all, the end of the
/// last.
pub fn hit(spots: &[Spot], pos: Point<Pixels>) -> Option<Mark> {
    let last = spots.last()?;
    let Some(spot) = spots.iter().find(|s| pos.y < s.layout.bounds().bottom()) else {
        return Some(Mark { seq: last.seq, byte: last.len });
    };
    let bounds = spot.layout.bounds();
    if pos.y < bounds.top() {
        return Some(Mark { seq: spot.seq, byte: 0 });
    }
    let at = Point { x: pos.x.max(bounds.left()), y: pos.y };
    let ix = spot.layout.index_for_position(at).unwrap_or_else(|ix| ix);
    Some(Mark { seq: spot.seq, byte: wrap::unshift(&spot.inserted, ix).min(spot.len) })
}

/// Text with coloured (and possibly bold) runs, the links in it, and
/// once wrapped, where it inserted breaks (`wrap::inserted`).
#[derive(Default)]
struct Runs {
    text: String,
    runs: Vec<(std::ops::Range<usize>, HighlightStyle)>,
    links: Vec<(std::ops::Range<usize>, String)>,
    inserted: Vec<usize>,
}

impl Runs {
    fn push(&mut self, s: &str, color: Hsla, bold: bool) -> &mut Runs {
        let start = self.text.len();
        self.text.push_str(s);
        let weight = bold.then_some(FontWeight::BOLD);
        self.runs.push((start..self.text.len(), HighlightStyle { color: Some(color), font_weight: weight, ..Default::default() }));
        self
    }

    /// What someone wrote, in `color`, its links (see `links`) in `link`
    /// and underlined.
    fn push_text(&mut self, s: &str, color: Hsla, link: Hsla) -> &mut Runs {
        let mut at = 0;
        for l in links::find(s) {
            if l.range.start > at {
                self.push(&s[at..l.range.start], color, false);
            }
            let start = self.text.len();
            self.text.push_str(&s[l.range.clone()]);
            let underline = UnderlineStyle { thickness: px(1.), color: Some(link), wavy: false };
            self.runs.push((start..self.text.len(), HighlightStyle { color: Some(link), underline: Some(underline), ..Default::default() }));
            self.links.push((start..self.text.len(), l.url));
            at = l.range.end;
        }
        if at < s.len() || s.is_empty() {
            self.push(&s[at..], color, false);
        }
        self
    }

    fn element(self) -> StyledText {
        StyledText::new(self.text).with_highlights(self.runs)
    }

    /// Bytes `sel` of the text as it was before wrapping on the
    /// selection fill `color`.
    fn select(&mut self, sel: std::ops::Range<usize>, color: Hsla) {
        let shift = |i: usize| i + self.inserted.iter().filter(|at| **at <= i).count();
        let sel = shift(sel.start)..shift(sel.end);
        let runs = std::mem::take(&mut self.runs);
        self.runs = selection::overlay(runs, sel, |style| HighlightStyle { background_color: Some(color), ..*style });
    }

    /// The text, its links clickable: a press and release in one link,
    /// with no drag between (`links::is_click`, against where the message
    /// view last saw the button go down), opens it. `id` keeps the
    /// element's press state from frame to frame. A double or triple
    /// click selects (see `selection`), so only a single one opens.
    /// Returns the text's layout too, for hit-testing.
    fn linked(self, app: &HircApp, id: SharedString) -> (AnyElement, TextLayout) {
        let text = StyledText::new(self.text).with_highlights(self.runs);
        let layout = text.layout().clone();
        if self.links.is_empty() {
            return (text.into_any_element(), layout);
        }
        let (ranges, urls): (Vec<_>, Vec<_>) = self.links.into_iter().unzip();
        #[cfg(feature = "snapshot")]
        app.link_spots.borrow_mut().push((text.layout().clone(), ranges.clone(), urls.clone()));
        let (press, clicks, dry) = (app.press.clone(), app.clicks.clone(), app.dry_open);
        let slop = app.metrics.cell / 2.;
        let element = InteractiveText::new(id, text)
            .on_click(ranges, move |ix, window, cx| {
                let up = window.mouse_position();
                let still = clicks.get() <= 1 && press.get().is_some_and(|down| links::is_click(down, (f32::from(up.x), f32::from(up.y)), slop));
                if let Some(url) = urls.get(ix).filter(|url| still && links::openable(url)) {
                    if dry {
                        println!("snapshot: open {url}");
                    } else {
                        cx.open_url(url);
                    }
                }
            })
            .into_any_element();
        (element, layout)
    }

    /// The text broken onto lines `width` wide (see `wrap`), in the
    /// app's font as it will be drawn, so its lines always fit and gpui
    /// never breaks them again. A message of several lines keeps its own
    /// line breaks, and each of its lines wraps by itself.
    fn wrap(self, app: &HircApp, window: &Window, width: f32) -> Runs {
        if self.text.is_empty() || width <= 0. {
            return self;
        }
        // Each line shaped alone, in its runs' fonts.
        let shape = |line: std::ops::Range<usize>| {
            let runs: Vec<TextRun> = self
                .runs
                .iter()
                .filter_map(|(range, style)| {
                    let len = range.end.min(line.end).saturating_sub(range.start.max(line.start));
                    (len > 0).then(|| TextRun {
                        len,
                        font: gpui::Font { weight: style.font_weight.unwrap_or(app.font.weight), ..app.font.clone() },
                        color: style.color.unwrap_or_default(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    })
                })
                .collect();
            let shaped = window.text_system().shape_line(self.text[line].to_owned().into(), px(app.metrics.size), &runs, None);
            move |i: usize| f32::from(shaped.x_for_index(i))
        };
        // Half a px short, so rounding never tips a line over.
        let breaks = wrap::breaks_lines(&self.text, shape, width - 0.5);
        if breaks.is_empty() {
            return self;
        }
        let (text, shift) = wrap::apply(&self.text, &breaks);
        let runs = self.runs.into_iter().map(|(r, style)| (shift(r.start)..shift(r.end), style)).collect();
        // A link keeps its whole range, across the rows it wraps onto.
        let links = self.links.into_iter().map(|(r, url)| (shift(r.start)..shift(r.end), url)).collect();
        Runs { text, runs, links, inserted: wrap::inserted(&breaks) }
    }
}

/// A nick, as the nick column or an event shows it (`shown`, which may be
/// cut to fit): ours in the theme's nickname colour and bold, anyone
/// else's in their own colour, looked up by the whole `nick`, so a cut
/// nick keeps its colour; on the mention fill when `band`.
fn nick_run(runs: &mut Runs, p: &Palette, shown: &str, nick: &str, cx: &Context, band: bool) {
    if cx.me.is_some_and(|me| me.eq_ignore_ascii_case(nick)) {
        runs.push(shown, p.nickname, true);
    } else {
        runs.push(shown, p.nick(nick, cx.slots, band), false);
    }
}

/// A line's columns before they are laid out: the nick column's runs,
/// the text's, the whole of a nick cut to fit, and whether it mentions us.
struct Composed {
    prefix: Runs,
    body: Runs,
    cut: Option<String>,
    mention: bool,
}

/// The time a line shows (`HH:MM`, local).
fn clock(line: &Line) -> String {
    line.time.with_timezone(&chrono::Local).format("%H:%M").to_string()
}

/// A line as a copy takes it (see `selection::copy`): its time, its nick
/// column in full and its text as drawn.
pub fn log(app: &HircApp, line: &Line, cx: &Context) -> Log {
    Log { time: clock(line), who: who(line), text: compose(app, line, cx).body.text }
}

/// What a line's nick column says, in full: the nick, `-nick-` for a
/// notice, `*` for an action, else the event's marker.
fn who(line: &Line) -> String {
    let nick = line.nick.as_deref().unwrap_or_default();
    match line.kind {
        LineKind::Privmsg => nick.to_owned(),
        LineKind::Notice if !nick.is_empty() => format!("-{nick}-"),
        LineKind::Action => "*".to_owned(),
        kind => display::marker(kind).to_owned(),
    }
}

fn compose(app: &HircApp, line: &Line, cx: &Context) -> Composed {
    let p = &app.palette;
    let nick = line.nick.as_deref().unwrap_or_default();
    let cols = cx.cols;
    let own = line.own || cx.me.is_some_and(|me| me.eq_ignore_ascii_case(nick));
    let mention =
        !own && matches!(line.kind, LineKind::Privmsg | LineKind::Action | LineKind::Notice) && cx.me.is_some_and(|me| display::mentions(&line.text, me));
    let (mut prefix, mut body) = (Runs::default(), Runs::default());
    // In a narrow view there is no time cell: a line that never went out
    // says so before the nick.
    if cols.time == 0 && line.delivery == Delivery::Unsent {
        prefix.push("✕ ", p.error, false);
    }
    // A nick longer than the column ends in `…` (its whole shows on
    // hover), so every line's text starts on one edge. `extra` is what
    // the nick has around it (a notice's dashes).
    let marks = prefix.text.chars().count();
    let mut cut = None;
    let mut fit_nick = |extra: usize| {
        let shown = fit(nick, cols.nick.saturating_sub(extra + marks).max(1));
        if shown != nick {
            cut = Some(nick.to_owned());
        }
        shown
    };
    // Ours, not yet echoed by the server: the time cell says so (`··:··`)
    // and the text reads as said. Without a time cell, the text is in the
    // quiet role until it is confirmed.
    let pending = line.delivery != Delivery::Sent && cols.time == 0;
    // A link in a line still on its way reads as quiet as its text.
    let link = if pending { p.quiet } else { p.url };
    match line.kind {
        LineKind::Privmsg => {
            nick_run(&mut prefix, p, &fit_nick(0), nick, cx, mention);
            body.push_text(&line.text, if pending { p.quiet } else { p.text }, link);
        }
        // A notice: irssi's `-nick-`, the dashes in the event colour so
        // they read as a marker, and the text a step below conversation.
        LineKind::Notice if !nick.is_empty() => {
            prefix.push("-", p.server, false);
            nick_run(&mut prefix, p, &fit_nick(2), nick, cx, mention);
            prefix.push("-", p.server, false);
            body.push_text(&line.text, p.text_dim, link);
        }
        LineKind::Action => {
            prefix.push("*", p.action, false);
            nick_run(&mut body, p, nick, nick, cx, mention);
            let color = if pending { p.quiet } else { p.action };
            body.push(" ", color, false).push_text(&line.text, color, link);
        }
        // The marker carries the error, as arrows carry joins; the
        // sentence is text, so red never covers a whole line.
        LineKind::Error => {
            prefix.push(display::marker(line.kind), p.error, false);
            body.push_text(&line.text, p.text, link);
        }
        // hirc's own voice (a quit, an answer to a command): irssi's
        // `-!-`, so it never reads as the server's `--` chatter, and the
        // text a step below conversation.
        LineKind::Status => {
            prefix.push(display::marker(line.kind), p.server, false);
            body.push_text(&line.text, p.text_dim, link);
        }
        LineKind::Server | LineKind::Notice => {
            prefix.push(display::marker(line.kind), p.server, false);
            body.push_text(&line.text, p.text_dim, link);
        }
        kind => {
            prefix.push(display::marker(kind), p.server, false);
            // One ladder for every event: the marker carries the event's
            // colour, then the nicks (nick-coloured), then what someone
            // typed (a reason, a new topic, mode arguments) in
            // text.secondary, and last the client's own wording ("joined",
            // "kicked") in the quietest legible role, so what changed
            // always reads before how hirc says it. A join or part is
            // shown in its own channel's window, which the title names, so
            // the line leaves the channel out.
            let (verb, free) = (p.quiet, p.text_dim);
            match display::event_parts(kind, line.nick.as_deref(), &line.text, None) {
                Some(parts) => {
                    for (part, s) in parts {
                        match part {
                            Part::Nick => nick_run(&mut body, p, &s, &s, cx, false),
                            Part::Verb | Part::Channel => _ = body.push(&s, verb, false),
                            Part::Free => _ = body.push_text(&s, free, link),
                        }
                    }
                }
                None => _ = body.push(&line.text, verb, false),
            }
        }
    }
    Composed { prefix, body, cut, mention }
}

pub fn line(app: &HircApp, window: &Window, line: &Line, cx: &Context) -> AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let cols = cx.cols;
    let time = clock(line);
    let Composed { prefix, body, cut, mention } = compose(app, line, cx);
    // The time cell says where a line we sent stands until the server
    // has it: an empty clock (`··:··`, the time it will show) while it
    // waits for the echo, `✕` once the connection dropped first (it may
    // never have gone out). `…` only ever means a cut.
    let (time, time_color) = match line.delivery {
        Delivery::Sent => (time, if mention { p.timestamp_band } else { p.timestamp }),
        Delivery::Pending => ("··:··".to_owned(), p.timestamp),
        Delivery::Unsent => ("✕".to_owned(), p.error),
    };
    // `w_full`: gpui's list measures each row as a root with a definite width,
    // and an auto-width flex root would size to its content and never wrap.
    // Rows have no vertical padding: every row is whole text lines, so the
    // view scrolls on one line grid, as a terminal does.
    let len = body.text.len();
    let mut body = body.wrap(app, window, app.pane() - 2. * m.gutter - m.text_column(cols));
    if let Some(sel) = cx.selected.clone() {
        body.select(sel, p.selection);
    }
    let inserted = body.inserted.clone();
    let (text, layout) = body.linked(app, SharedString::from(format!("text-{}", line.seq)));
    // Once placed, the text says where it is, for a press or a drag.
    let spot = Spot { seq: line.seq, layout, inserted, len };
    let spots = app.spots.clone();
    div()
        .flex()
        .w_full()
        .min_w(px(0.))
        .px(px(m.gutter))
        .when(mention, |d| d.bg(p.highlight))
        .when(cols.time > 0, |d| d.child(div().flex_none().w(px(m.grid(cols.time))).whitespace_nowrap().text_color(time_color).child(time)))
        .child(nick_column(app, cols, prefix, cut.map(|full| (full, line.seq))))
        .child(div().flex_1().min_w(px(0.)).ml(px(m.cell)).on_children_prepainted(move |_, _, _| push_spot(&spots, spot.clone())).child(text))
        .into_any_element()
}

/// Register where a line's text was drawn, once per frame (a line laid
/// out twice keeps its last place).
fn push_spot(spots: &std::cell::RefCell<Vec<Spot>>, spot: Spot) {
    let mut spots = spots.borrow_mut();
    spots.retain(|s| s.seq != spot.seq);
    spots.push(spot);
    spots.sort_by(|a, b| a.layout.bounds().top().partial_cmp(&b.layout.bounds().top()).unwrap_or(std::cmp::Ordering::Equal));
}

/// `line` with the read marker above it: where the unread lines began
/// when the window was entered (weechat's read marker). One row on the
/// line grid: "new" in the unread colour on the nick column's right edge,
/// as markers sit, and a hairline in the separators' colour across the
/// text column.
pub fn with_marker(app: &HircApp, line: AnyElement) -> AnyElement {
    with_rule(app, Some("new"), line)
}

/// `line` with a rule above it and `label` (in the unread colour) on the
/// nick column's edge: the read marker, or with no label, the quiet break
/// a server buffer draws where one connection's lines end.
pub fn with_rule(app: &HircApp, label: Option<&'static str>, line: AnyElement) -> AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let rule = div().flex_1().min_w(px(0.)).ml(px(m.cell)).h(px(m.line)).flex().items_center().child(div().w_full().h(px(1.)).bg(p.border));
    let cols = app.cols();
    let marker = div()
        .flex()
        .w_full()
        .px(px(m.gutter))
        .when(cols.time > 0, |d| d.child(div().flex_none().w(px(m.grid(cols.time)))))
        .child(div().flex_none().flex().justify_end().w(px(m.grid(cols.nick))).when(cols.time > 0, |d| d.ml(px(m.cell))).text_color(p.unread).children(label))
        .child(rule);
    div().flex().flex_col().w_full().child(marker).child(line).into_any_element()
}

/// Drawn over the top row when it is the wrapped tail of `line`: the
/// time of the message it belongs to (in the time column's role), and
/// its nick after an `↑` that says the row goes on from above (the arrow
/// the window list's "more" rows use; `…` only ever means a cut), in the
/// quiet role the scroll notes share, so the row says whose it is
/// without reading as a new message.
pub fn sticky(app: &HircApp, line: &Line, cols: Columns) -> gpui::AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let time = line.time.with_timezone(&chrono::Local).format("%H:%M").to_string();
    let who = who(line);
    // The arrow when it fits beside the whole of who it is (a buffer's
    // nick column is only as wide as its nicks); else who, cut to the
    // column, in the quiet role, which already says it is no new line.
    let who = if who.chars().count() + 2 <= cols.nick { format!("↑ {who}") } else { fit(&who, cols.nick) };
    let mut prefix = Runs::default();
    prefix.push(&who, p.quiet, false);
    div()
        .flex()
        .w_full()
        .px(px(m.gutter))
        .text_color(p.timestamp)
        .when(cols.time > 0, |d| d.child(div().flex_none().w(px(m.grid(cols.time))).whitespace_nowrap().child(time)))
        .child(nick_column(app, cols, prefix, None))
        .into_any_element()
}

/// The nick column, nicks right-aligned against the text, a space after
/// the time column when there is one. `cut` is the whole of a nick
/// shortened to fit (and the line's seq, for a stable id): hovering the
/// column shows it.
fn nick_column(app: &HircApp, cols: Columns, prefix: Runs, cut: Option<(String, u64)>) -> AnyElement {
    let m = &app.metrics;
    let column = div()
        .flex_none()
        .flex()
        .justify_end()
        .overflow_hidden()
        .whitespace_nowrap()
        .w(px(m.grid(cols.nick)))
        .when(cols.time > 0, |d| d.ml(px(m.cell)))
        .child(prefix.element());
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

/// What the scroll note says of the oldest line, once it shows: plain
/// top of buffer, an older page on its way, or nothing older to get.
/// Empty while the top isn't on screen.
pub fn top_note(top: Option<HistoryState>) -> &'static str {
    match top {
        None => "",
        Some(HistoryState::Idle) => "top of buffer",
        Some(HistoryState::Loading) => "loading older messages",
        Some(HistoryState::Start) => "start of history",
    }
}

/// The bottom row while the view is scrolled away from the newest line,
/// irssi's `-- more --`: how many lines are below, in the unit `j` and
/// `k` step (a line of the grid: a message that wraps to three is three),
/// and, once the oldest line shows, where history stands (`top_note`),
/// after the count, so the count never moves. A window that all fits
/// shows the row only to say an older page is loading (see
/// `app::scroll_note`): there is nothing below to count. In the quiet role the sticky
/// head shares; the count takes the unread colour once new lines have
/// arrived below. The viewport gives it a row of its own: the note never
/// takes a row at the top, which would move the lines.
pub fn more(app: &HircApp, below: usize, top: Option<HistoryState>, fresh: bool) -> AnyElement {
    let p = &app.palette;
    let m = &app.metrics;
    let mut runs = Runs::default();
    let note = top_note(top);
    if below > 0 || top.is_none() {
        runs.push("-- more: ", p.quiet, false).push(&below.to_string(), if fresh { p.unread } else { p.quiet }, fresh).push(
            if below == 1 { " line below" } else { " lines below" },
            p.quiet,
            false,
        );
        runs.push(&if note.is_empty() { " --".to_owned() } else { format!(" · {note} --") }, p.quiet, false);
    } else {
        runs.push(&format!("-- {note} --"), p.quiet, false);
    }
    div().w_full().h(px(m.line)).px(px(m.gutter)).child(runs.element()).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scroll_note_says_where_history_stands() {
        use hirc_core::store::HistoryState;
        assert_eq!(top_note(None), "");
        assert_eq!(top_note(Some(HistoryState::Idle)), "top of buffer");
        assert_eq!(top_note(Some(HistoryState::Loading)), "loading older messages");
        assert_eq!(top_note(Some(HistoryState::Start)), "start of history");
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
