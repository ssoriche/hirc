//! App state and the event loop. The network thread sends `Event`s; we
//! apply them to the `Store` and redraw. Input goes back as `Command`s.

use crate::input::{self, Note, Submit, TextInput, VimEvent, WantTab};
use crate::links;
use crate::metrics::{Columns, Metrics, Shape};
use crate::scroll::{self, Pos};
use crate::theme::{self, Palette, palette};
use crate::views::selection::{self, Mark, Selection};
use crate::views::{buffer, sidebar, viewport};
use crate::vim::{self, Effect, Ex, Nav, Scroll};
use chrono::{DateTime, Utc};
use gpui::{
    App, ClipboardItem, Context, Entity, Focusable, KeyBinding, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, ScrollHandle,
    SharedString, Window, actions, div, list, prelude::*, px,
};
use hirc_core::command::{self, Input, WindowAction};
use hirc_core::event::{Command, ConnStatus, Event, Target};
use hirc_core::layout::{self, Layout};
use hirc_core::services;
use hirc_core::store::{Applied, Buffer, HistoryState, LineKind, Network, Store, counts};
use hirc_core::theme::Theme;
use hirc_core::{display, multiline, split};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

actions!(hirc, [Quit, Window1, Window2, Window3, Window4, Window5, Window6, Window7, Window8, Window9, Window10]);

/// alt-1 .. alt-9 go to windows 1-9, alt-0 to window 10 (irssi). No key
/// context, so they work while the text input has focus.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("alt-1", Window1, None),
        KeyBinding::new("alt-2", Window2, None),
        KeyBinding::new("alt-3", Window3, None),
        KeyBinding::new("alt-4", Window4, None),
        KeyBinding::new("alt-5", Window5, None),
        KeyBinding::new("alt-6", Window6, None),
        KeyBinding::new("alt-7", Window7, None),
        KeyBinding::new("alt-8", Window8, None),
        KeyBinding::new("alt-9", Window9, None),
        KeyBinding::new("alt-0", Window10, None),
    ]);
}

pub struct Init {
    pub networks: Vec<String>,
    /// Each configured server with its `channels`, in config order (seeds the window list).
    pub servers: Vec<(String, Vec<String>)>,
    pub theme: Theme,
    pub font_family: Option<String>,
    pub font_size: f32,
    pub banner: Option<String>,
    /// A quiet note for the first server buffer (no config file yet).
    pub hint: Option<String>,
    pub commands: UnboundedSender<Command>,
    pub events: UnboundedReceiver<Event>,
    /// Snapshot runs neither read nor write the window layout.
    pub no_layout_file: bool,
    /// A fixed clock for the lines hirc writes (snapshot fixtures).
    pub clock: Option<chrono::DateTime<chrono::Utc>>,
    /// Snapshot runs never open a link: they print what a click would
    /// open (`snapshot: open <url>`).
    pub dry_open: bool,
}

/// A text drawn with links: its layout, the links' ranges and URLs.
#[cfg(feature = "snapshot")]
pub type LinkSpot = (gpui::TextLayout, Vec<std::ops::Range<usize>>, Vec<String>);

pub struct HircApp {
    pub store: Store,
    pub selected: Option<(usize, usize)>,
    pub layout: Layout,
    layout_path: PathBuf,
    no_layout_file: bool,
    pub palette: Palette,
    font_family: Option<SharedString>,
    /// The configured font, for measuring text that must fit (the header).
    pub font: gpui::Font,
    /// Every spacing value, from the configured font.
    pub metrics: Metrics,
    /// The window list's scroll position; selection scrolls it into view.
    pub sidebar_scroll: ScrollHandle,
    /// The window list's width at the last render (it follows the window).
    sidebar_width: f32,
    /// The window's height at the last render (the list's full height).
    sidebar_height: f32,
    banner: Option<String>,
    /// Shown in the header when there is no buffer to put it in.
    hint: Option<String>,
    input: Entity<TextInput>,
    /// Each window's unsent line, kept while another window is shown.
    drafts: HashMap<(usize, usize), String>,
    lines: ListState,
    /// The message view's time and nick columns, in cells (sized to the
    /// view, the same in every buffer).
    cols: Columns,
    /// The message view's width at the last render: text is broken onto
    /// lines against it (see `wrap`).
    pane: f32,
    /// Wheel movement not yet a whole line.
    wheel: Rc<Cell<f32>>,
    /// What is left of a line beside the message view's whole lines,
    /// which the header and the input bar each take (see `viewport`), as
    /// the view last measured it.
    extra: Rc<Cell<f32>>,
    /// That, split above and below a bar's text on whole device px, at
    /// the last render: the window list starts the part above further
    /// down, so its first row stays on the header's line.
    pub bar_extra: (f32, f32),
    /// Each buffer's nick colour slots, and its line count when handed out.
    nick_slots: HashMap<(usize, usize), (usize, HashMap<String, usize>)>,
    /// Lines arrived below while the view was scrolled up.
    fresh_below: bool,
    /// The list was reset or took a page since it was last laid out: its
    /// heights are stale for a frame (lines not measured yet read as
    /// none), so whether the top shows or all of it fits waits a frame.
    settling: bool,
    /// All of the window fits in the view, as of the last settled frame.
    fits: bool,
    /// The scroll note took the bottom row last frame (the list was a
    /// row shorter).
    noted: bool,
    /// The window whose last page of older lines came back empty without
    /// reaching the start (the server gave up on it for this connection):
    /// not asked again until the connection changes or the window is
    /// left. Only an answer sets it, so a request lost with the
    /// connection never stops the next one.
    dry: Option<(usize, usize)>,
    /// The channel a /join asked for, by network name: its window is
    /// focused when the server confirms the join.
    pending_join: Option<(String, String)>,
    /// Where the unread lines began when this window was entered: the
    /// window and the first unread line's seq (a "new" rule is drawn
    /// above it until the window is left).
    marker: Option<(usize, usize, u64)>,
    /// The read marker hirc last sent for each window: when the server
    /// sends it back, the divider stays where it is.
    sent_read: HashMap<(usize, usize), DateTime<Utc>>,
    /// Where the primary button last went down in the message view
    /// (logical px): a link opens only on a click, not at the end of a
    /// drag.
    pub press: Rc<Cell<Option<(f32, f32)>>>,
    /// How many clicks that press made (a double click is 2): only a
    /// single one opens a link, the others select.
    pub clicks: Rc<Cell<usize>>,
    /// Where each line's text was drawn this frame, top to bottom: what a
    /// press or drag hit-tests (see `buffer::hit`).
    pub spots: Rc<RefCell<Vec<buffer::Spot>>>,
    /// The selected message text, if any (see `selection`).
    selection: Option<Selection>,
    /// Where a drag that may select began, while the button is down.
    drag: Option<Mark>,
    /// The selection resolved against the shown buffer's lines at the
    /// last render: the first and last point, in order.
    selected_span: Option<(selection::At, selection::At)>,
    /// Print what a link click would open instead of opening it.
    pub dry_open: bool,
    /// The links drawn this frame (their text's layout, ranges and
    /// URLs), for the `link:` snapshot step.
    #[cfg(feature = "snapshot")]
    pub link_spots: std::cell::RefCell<Vec<LinkSpot>>,
    commands: UnboundedSender<Command>,
}

impl HircApp {
    pub fn new(init: Init, window: &mut Window, cx: &mut Context<Self>) -> HircApp {
        let mut store = Store::new(init.networks.clone());
        store.clock = init.clock;
        let layout_path = layout::path();
        let saved = if init.no_layout_file { None } else { Layout::load(&layout_path) };
        let missing = saved.is_none();
        let mut layout = saved.unwrap_or_else(|| Layout::seed(&init.servers));
        let mut changed = layout.prune(&init.networks);
        // Materialise saved windows so their numbers are stable before the server sends JOIN.
        for key in layout.windows.clone() {
            let Some(n) = store.networks.iter().position(|net| net.name == key.network) else { continue };
            if let Some(name) = &key.name {
                let target = if name.starts_with(['#', '&']) { Target::Channel(name.clone()) } else { Target::Query(name.clone()) };
                store.buffer_ix(n, &target);
            }
        }
        changed |= layout.sync(&store);
        for cmd in reopen(&store) {
            let _ = init.commands.send(cmd);
        }
        if (missing || changed)
            && !init.no_layout_file
            && let Err(e) = layout.save(&layout_path)
        {
            tracing::warn!("could not save window layout: {e}");
        }
        let font_family: Option<SharedString> = init.font_family.map(Into::into);
        let font = font_for(window, font_family.clone());
        let metrics = Metrics::new(init.font_size, measure(window, &font, init.font_size));
        // No config yet: say so once, quietly, where server notices go.
        let mut hint = init.hint;
        if let Some(note) = hint.take_if(|_| store.networks.first().is_some_and(|net| matches!(net.buffers.first(), Some(b) if b.target == Target::Server))) {
            store.push_local(0, 0, LineKind::Server, note);
        }
        let selected = layout.resolve(&store, 0).or_else(|| (!store.networks.is_empty()).then_some((0, 0)));
        let palette = palette(&init.theme);
        let input = cx.new(|cx| {
            let mut input = TextInput::new("", cx);
            input.palette = Some(palette.clone());
            input.metrics = Some(metrics);
            input
        });
        cx.subscribe(&input, |this, _, ev: &Submit, cx| this.submit_input(ev.text.clone(), ev.confirmed, cx)).detach();
        cx.subscribe(&input, |this, input, _: &WantTab, cx| {
            let nicks = this.current().map(|(net, buf)| hirc_core::complete::nicks(buf, net.nick.as_deref())).unwrap_or_default();
            input.update(cx, |input, cx| input.tab(&nicks, cx));
        })
        .detach();
        cx.subscribe(&input, |this, _, ev: &VimEvent, cx| this.vim_effect(&ev.0, cx)).detach();
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        // The banner can be dismissed; keep what it said where errors go.
        // It is about the whole app, so every network's server window has it.
        if let Some(banner) = &init.banner {
            for n in 0..store.networks.len() {
                store.push_local(n, 0, LineKind::Error, banner.clone());
            }
        }
        // A dropped network's header counts down to its retry: redraw each
        // second while one waits (a fixed clock has nothing to count).
        if init.clock.is_none() {
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                    let alive = this.update(cx, |app, cx| {
                        if app.store.networks.iter().any(|net| matches!(net.status, ConnStatus::Disconnected { .. })) {
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        // Any key but the copy chord puts a selection away (it never
        // reaches the input's own handling of the key).
        let this = cx.weak_entity();
        cx.intercept_keystrokes(move |ev, _, cx| {
            if !selection::keeps(&ev.keystroke.key, ev.keystroke.modifiers.platform) {
                _ = this.update(cx, |app, cx| app.clear_selection(cx));
            }
        })
        .detach();
        // Coming back to hirc may mark the window on screen read (see
        // `marks_read`): draw again so that is looked at.
        cx.observe_window_activation(window, |_, _, cx| cx.notify()).detach();
        let mut events = init.events;
        cx.spawn(async move |this, cx| {
            while let Some(ev) = events.recv().await {
                if this.update(cx, |app, cx| app.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        HircApp {
            store,
            selected,
            layout,
            layout_path,
            no_layout_file: init.no_layout_file,
            palette,
            font_family,
            font,
            metrics,
            sidebar_scroll: ScrollHandle::new(),
            sidebar_width: metrics.sidebar_width(f32::from(window.viewport_size().width)),
            sidebar_height: f32::from(window.viewport_size().height),
            banner: init.banner,
            hint,
            input,
            drafts: HashMap::new(),
            // Measure every line, so scroll offsets and the count of lines
            // below the view are exact (lines are cached once measured).
            lines: ListState::new(0, ListAlignment::Bottom, px(400.)).measure_all(),
            cols: Columns::default(),
            pane: 0.,
            wheel: Rc::default(),
            extra: Rc::default(),
            bar_extra: (0., 0.),
            nick_slots: HashMap::new(),
            fresh_below: false,
            settling: true,
            fits: false,
            noted: false,
            dry: None,
            pending_join: None,
            marker: None,
            sent_read: HashMap::new(),
            press: Rc::default(),
            clicks: Rc::default(),
            spots: Rc::default(),
            selection: None,
            drag: None,
            selected_span: None,
            dry_open: init.dry_open,
            #[cfg(feature = "snapshot")]
            link_spots: Default::default(),
            commands: init.commands,
        }
    }

    pub fn current(&self) -> Option<(&Network, &Buffer)> {
        let (n, b) = self.selected?;
        let net = self.store.networks.get(n)?;
        Some((net, net.buffers.get(b)?))
    }

    pub fn select(&mut self, n: usize, b: usize, cx: &mut Context<Self>) {
        // The input line belongs to its window: keep this one's draft and
        // bring back the new one's, so a line is never sent somewhere else.
        if self.selected != Some((n, b)) {
            let next = self.drafts.remove(&(n, b)).unwrap_or_default();
            let draft = self.input.update(cx, |input, cx| input.swap_draft(next, cx));
            if let Some(old) = self.selected.filter(|_| !draft.is_empty()) {
                self.drafts.insert(old, draft);
            }
        }
        if self.selected != Some((n, b)) {
            self.selection = None;
            self.drag = None;
            let buf = &self.store.networks[n].buffers[b];
            self.marker = buf.first_unread().map(|ix| (n, b, buf.lines[ix].seq));
        }
        self.selected = Some((n, b));
        let buf = &mut self.store.networks[n].buffers[b];
        buf.unread = 0;
        buf.mentions = 0;
        self.lines.reset(buf.lines.len());
        self.fresh_below = false;
        self.settling = true;
        self.fits = false;
        self.dry = None;
        self.wheel.set(0.);
        self.reveal_selected();
        cx.notify();
    }

    /// Index of window `(n, b)` among the sidebar's rows.
    fn sidebar_row(&self, n: usize, b: usize) -> Option<usize> {
        (0..self.layout.windows.len()).filter_map(|pos| self.layout.resolve(&self.store, pos)).position(|w| w == (n, b))
    }

    /// Scroll the window list so the selected row is in view.
    fn reveal_selected(&self) {
        if let Some(row) = self.selected.and_then(|(n, b)| self.sidebar_row(n, b)) {
            self.sidebar_scroll.scroll_to_item(row);
        }
    }

    /// Give any new buffer a window number and persist the list.
    fn sync_layout(&mut self) {
        if self.layout.sync(&self.store) {
            self.save_layout();
        }
    }

    fn save_layout(&self) {
        if self.no_layout_file {
            return;
        }
        if let Err(e) = self.layout.save(&self.layout_path) {
            tracing::warn!("could not save window layout: {e}");
        }
    }

    /// `number` is 1-based, as typed; out of range does nothing (alt-N
    /// past the last window).
    pub fn goto_window(&mut self, number: usize, cx: &mut Context<Self>) {
        if let Some((n, b)) = number.checked_sub(1).and_then(|pos| self.layout.resolve(&self.store, pos)) {
            self.select(n, b, cx);
        }
    }

    /// A typed `/window N` or `:N`: out of range says so.
    fn goto_typed(&mut self, number: usize, cx: &mut Context<Self>) {
        let count = self.window_count();
        match self.selected {
            Some((n, b)) if number > count => self.local_error(n, b, no_window(number, count)),
            _ => self.goto_window(number, cx),
        }
    }

    /// The selected window's name, as the list shows it.
    fn current_label(&self) -> String {
        self.current().map(|(net, buf)| buf.target.name().unwrap_or(&net.name).to_owned()).unwrap_or_default()
    }

    /// Snapshot scripts: type into the input as a person would.
    #[cfg(feature = "snapshot")]
    pub fn type_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.type_text(text, window, cx));
    }

    /// Logical centre of the first sidebar row whose label contains `label`
    /// (case-insensitive), as the sidebar lays rows out at its current
    /// scroll position. `Err` names a match that is scrolled out of view.
    #[cfg(feature = "snapshot")]
    pub fn sidebar_point(&self, label: &str) -> Option<Result<(f32, f32), String>> {
        let want = label.to_lowercase();
        let rows = (0..self.layout.windows.len()).filter_map(|pos| self.layout.resolve(&self.store, pos));
        let row = rows.enumerate().find_map(|(row, (n, b))| {
            let net = &self.store.networks[n];
            let name = net.buffers[b].target.name().unwrap_or(&net.name);
            name.to_lowercase().contains(&want).then_some(row)
        })?;
        let count = self.window_count();
        let (x, y) = sidebar::row_centre(
            &self.metrics,
            self.bar_extra.0,
            self.sidebar_width,
            self.sidebar_height,
            count,
            f32::from(self.sidebar_scroll.offset().y),
            row,
        );
        let view = self.sidebar_scroll.bounds();
        let visible = view.size.height <= px(0.) || (px(y - self.metrics.row / 2.) >= view.top() && px(y + self.metrics.row / 2.) <= view.bottom());
        Some(if visible { Ok((x, y)) } else { Err(format!("row {} is scrolled out of view", row + 1)) })
    }

    /// Logical centre of the first character of the first link drawn
    /// whose URL contains `text` (case-insensitive).
    #[cfg(feature = "snapshot")]
    pub fn link_point(&self, text: &str) -> Option<(f32, f32)> {
        let want = text.to_lowercase();
        self.link_spots.borrow().iter().find_map(|(layout, ranges, urls)| {
            let ix = urls.iter().position(|url| url.to_lowercase().contains(&want))?;
            let at = layout.position_for_index(ranges[ix].start)?;
            Some((f32::from(at.x) + self.metrics.cell / 2., f32::from(at.y) + self.metrics.line / 2.))
        })
    }

    /// The window list's width at the last render.
    #[cfg(feature = "snapshot")]
    pub fn sidebar_width(&self) -> f32 {
        self.sidebar_width
    }

    /// Number of numbered windows.
    fn window_count(&self) -> usize {
        (0..self.layout.windows.len()).filter_map(|pos| self.layout.resolve(&self.store, pos)).count()
    }

    /// Next or previous window after the selected one, wrapping around.
    fn nav(&mut self, dir: Nav, cx: &mut Context<Self>) {
        let count = self.window_count();
        let Some((n, b)) = self.selected else { return };
        let Some(pos) = self.layout.position_of(&self.store, n, b) else { return };
        if count == 0 {
            return;
        }
        let to = match dir {
            Nav::Next => (pos + 1) % count,
            Nav::Prev => (pos + count - 1) % count,
        };
        self.goto_window(to + 1, cx);
    }

    /// Go to the first window whose label starts with `name`, ignoring case.
    fn goto_buffer(&mut self, name: &str, cx: &mut Context<Self>) {
        let want = name.to_lowercase();
        let found = (0..self.layout.windows.len()).filter_map(|pos| self.layout.resolve(&self.store, pos)).find(|&(n, b)| {
            let net = &self.store.networks[n];
            let label = net.buffers[b].target.name().unwrap_or(&net.name);
            let label = label.to_lowercase();
            label.starts_with(&want) || label.trim_start_matches(['#', '&']).starts_with(&want)
        });
        match (found, self.selected) {
            (Some((n, b)), _) => self.select(n, b, cx),
            (None, Some((n, b))) => {
                self.local_error(n, b, format!("No buffer matches \"{name}\""));
                cx.notify();
            }
            (None, None) => {}
        }
    }

    /// Scroll on the line grid (see `scroll`): `k` moves one line, `ctrl-u`
    /// half the view's lines.
    fn scroll(&mut self, s: Scroll) {
        let line = self.metrics.line;
        // The list reports its viewport once laid out; before that, guess.
        let page = match f32::from(self.lines.viewport_bounds().size.height) {
            h if h > 0. => scroll::whole_lines(h, line).0,
            _ => 20,
        };
        match s {
            Scroll::Lines(n) => scroll::by_lines(&self.lines, n, line),
            Scroll::HalfPage(n) => scroll::by_lines(&self.lines, (page / 2).max(1) as i32 * n, line),
            Scroll::Top => scroll::set(&self.lines, if self.lines.item_count() == 0 { Pos::Tail } else { Pos::Offset(0.) }),
            Scroll::Bottom => scroll::set(&self.lines, Pos::Tail),
        }
    }

    /// Lines of the grid below the view: what `j` steps through.
    fn lines_below(&self) -> usize {
        let max = f32::from(self.lines.max_offset_for_scrollbar().y);
        let cur = -f32::from(self.lines.scroll_px_offset_for_scrollbar().y);
        scroll::lines_below(max, cur, self.metrics.line)
    }

    /// While scrolled, the message whose wrapped tail is the top row (its
    /// head, with the time and nick, is above the view).
    fn cut_head(&self) -> Option<usize> {
        let top = self.lines.logical_scroll_top();
        (f32::from(top.offset_in_item) >= self.metrics.line / 2.).then_some(top.item_ix)
    }

    fn vim_effect(&mut self, effect: &Effect, cx: &mut Context<Self>) {
        match effect {
            Effect::Scroll(s) => self.scroll(*s),
            Effect::Nav(dir) => self.nav(*dir, cx),
            Effect::Ex(line) => match vim::ex(line) {
                Ex::None => {}
                // Through the Quit action, which writes the history cache first.
                Ex::Quit => cx.dispatch_action(&Quit),
                Ex::Window(n) => self.goto_typed(n, cx),
                Ex::Buffer(name) => self.goto_buffer(&name, cx),
                Ex::Slash(s) => self.submit(s, cx),
            },
            Effect::Dismiss => self.banner = None,
            Effect::Pass | Effect::Edit { .. } | Effect::Submit => {}
        }
        cx.notify();
    }

    /// Move the selected window to `number`, and say so where the user is
    /// looking (the list changes far from the input line).
    fn move_window(&mut self, number: usize, cx: &mut Context<Self>) {
        let Some((n, b)) = self.selected else { return };
        let count = self.window_count();
        if number > count {
            self.local_error(n, b, no_window(number, count));
        } else if let Some(from) = self.layout.position_of(&self.store, n, b) {
            self.layout.move_to(from, number - 1);
            self.save_layout();
            self.reveal_selected();
            self.local_note(n, b, format!("{} is now window {number}", self.current_label()));
        }
        cx.notify();
    }

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        // Our JOIN answering a /join: the user wants that window.
        let focus = match &ev {
            Event::Joined { network, channel } => {
                self.pending_join.take_if(|(net, chan)| net == network && chan.eq_ignore_ascii_case(channel)).map(|_| (network.clone(), channel.clone()))
            }
            _ => None,
        };
        let got = absorb(&mut self.store, self.selected, &mut self.sent_read, &mut self.dry, ev);
        for cmd in [got.resend, got.open].into_iter().flatten() {
            let _ = self.commands.send(cmd);
        }
        match got.applied {
            Applied::Line { network, buffer, index } => self.arrived(network, buffer, index),
            Applied::Lines { network, at } => at.into_iter().for_each(|(buffer, index)| self.arrived(network, buffer, index)),
            Applied::Replaced { network, buffer, removed, index } => {
                if self.selected == Some((network, buffer)) {
                    self.lines.splice(removed..removed + 1, 0);
                }
                if let Some(index) = index {
                    self.arrived(network, buffer, index);
                }
            }
            Applied::History { network, buffer, at } => self.history_arrived(network, buffer, at),
            Applied::Read { network, buffer } => {
                if let Some(seq) = got.divider {
                    self.set_marker(network, buffer, seq);
                }
            }
            Applied::Changed | Applied::Nothing => {}
        }
        self.sync_layout();
        if let Some((network, channel)) = focus {
            let n = self.store.network_ix(&network);
            let b = self.store.buffer_ix(n, &Target::Channel(channel));
            self.select(n, b, cx);
        }
        cx.notify();
    }

    /// A line landed at `index` in buffer `(n, b)`: show it if that is the
    /// window on screen.
    fn arrived(&mut self, n: usize, b: usize, index: usize) {
        if self.selected == Some((n, b)) {
            self.fresh_below |= !scroll::following(&self.lines);
            self.lines.splice(index..index, 1);
            let buf = &mut self.store.networks[n].buffers[b];
            buf.unread = 0;
            buf.mentions = 0;
        }
    }

    /// A page of history landed in `(n, b)`, each line at its index in
    /// `at` (in the order they went in). On screen each is spliced in
    /// where it went. Lines above the view keep it where it was: the list
    /// moves its top item down with them, so nothing jumps.
    fn history_arrived(&mut self, n: usize, b: usize, at: Vec<usize>) {
        if self.selected != Some((n, b)) {
            return;
        }
        self.settling |= !at.is_empty();
        for ix in at {
            self.lines.splice(ix..ix, 1);
        }
        // Lines past the read marker came with it, and there is no divider
        // yet: it goes above the first of them.
        let buf = &self.store.networks[n].buffers[b];
        if self.marker.is_none()
            && let Some(ix) = buf.first_unread()
        {
            let seq = buf.lines[ix].seq;
            self.set_marker(n, b, Some(seq));
        }
        let buf = &mut self.store.networks[n].buffers[b];
        buf.unread = 0;
        buf.mentions = 0;
    }

    /// The top of the window on screen is showing: ask for the page
    /// before it, unless one is on its way or there is none.
    fn want_older(&mut self, up: bool) {
        let Some((n, b)) = self.selected else { return };
        let net = &mut self.store.networks[n];
        let status = &net.status;
        let buf = &mut net.buffers[b];
        if !older_ready(buf, up, status) {
            return;
        }
        let Some(target) = buf.target.name().map(str::to_owned) else { return };
        buf.history = HistoryState::Loading;
        let _ = self.commands.send(Command::Older { network: net.name.clone(), target });
    }

    /// Put the "new" divider of the window on screen above the line with
    /// `seq` (or take it away), redrawing the rows it leaves and joins.
    fn set_marker(&mut self, n: usize, b: usize, seq: Option<u64>) {
        let old = self.marker.take().filter(|m| (m.0, m.1) == (n, b)).map(|m| m.2);
        self.marker = seq.map(|s| (n, b, s));
        if self.selected != Some((n, b)) {
            return;
        }
        let lines = &self.store.networks[n].buffers[b].lines;
        for s in [old, seq].into_iter().flatten() {
            if let Some(ix) = lines.iter().position(|l| l.seq == s) {
                self.lines.splice(ix..ix + 1, 1);
            }
        }
    }

    /// Show a local error line in the current buffer.
    fn local_error(&mut self, n: usize, b: usize, msg: String) {
        let index = self.store.push_local(n, b, LineKind::Error, msg);
        self.lines.splice(index..index, 1);
    }

    /// The selected window has been answered: whatever the user sends
    /// there says they have read it, so its "new" rule goes.
    fn caught_up(&mut self) {
        let Some((n, b, _)) = self.marker else { return };
        if self.selected != Some((n, b)) {
            return;
        }
        self.set_marker(n, b, None);
    }

    /// Show hirc's own answer to a command (`-!-`, the client's voice, not
    /// the server's `--`) in the current buffer.
    fn local_note(&mut self, n: usize, b: usize, msg: String) {
        let index = self.store.push_local(n, b, LineKind::Status, msg);
        self.lines.splice(index..index, 1);
    }

    /// Enter in the input. A text of several lines that starts as a
    /// command stays, with why (commands are one line); one that would go
    /// out as many messages waits for a second Enter (see `confirm`). Else
    /// the input starts afresh and the text goes as `submit` sends it.
    fn submit_input(&mut self, text: String, confirmed: bool, cx: &mut Context<Self>) {
        if let Some(note) = self.hold_for(&text, confirmed) {
            self.input.update(cx, |input, cx| input.hold(note, cx));
            return;
        }
        self.input.update(cx, |input, cx| input.sent(cx));
        self.submit(text, cx);
    }

    /// Why `text` should not go on this Enter, if it should not.
    fn hold_for(&self, text: &str, confirmed: bool) -> Option<Note> {
        let lines = multiline::lines(text).len();
        if lines < 2 {
            return None;
        }
        if text.starts_with('/') && !text.starts_with("//") {
            return Some(Note::Error("Commands are one line: remove the line breaks to run it".into()));
        }
        if confirmed {
            return None;
        }
        let (net, buf) = self.current()?;
        let target = buf.target.name()?;
        if !matches!(net.status, ConnStatus::Connected { .. }) {
            return None;
        }
        let messages =
            multiline::messages(text, split::body_budget("PRIVMSG", target, net.nick.as_deref().unwrap_or_default().len() + 75).max(1), net.multiline.as_ref());
        confirm(lines, messages, net.multiline.is_some()).map(Note::Confirm)
    }

    fn submit(&mut self, text: String, cx: &mut Context<Self>) {
        if text.trim().is_empty() {
            return;
        }
        let text = outgoing(&text);
        let Some((n, b)) = self.selected else { return };
        let net = &self.store.networks[n];
        let target = net.buffers[b].target.name().map(str::to_owned);
        let name = net.name.clone();
        let status = net.status.clone();
        let connected = matches!(status, ConnStatus::Connected { .. });
        // A bare /topic shows the whole topic (the header may cut it short).
        if text.trim().eq_ignore_ascii_case("/topic")
            && let Some(chan) = target.as_deref().filter(|t| t.starts_with(['#', '&']))
        {
            let note = match &net.buffers[b].topic {
                Some(topic) if !topic.is_empty() => format!("Topic for {chan}: {topic}"),
                _ => format!("No topic is set for {chan}"),
            };
            self.local_note(n, b, note);
            cx.notify();
            return;
        }
        match command::parse(&text, &name, target.as_deref()) {
            Ok(Input::Send(cmd @ Command::Reconnect { .. })) => {
                let _ = self.commands.send(cmd);
            }
            // Chat sent while offline would vanish without a trace; keep
            // the line so it can be sent once connected.
            Ok(Input::Send(Command::Say { .. } | Command::Action { .. })) if !connected => {
                self.local_error(n, b, not_connected(&name, &status));
                self.input.update(cx, |input, cx| input.swap_draft(text.clone(), cx));
            }
            // A down network drops everything but /reconnect.
            Ok(Input::Send(_)) if status.is_down() => self.local_error(n, b, not_connected(&name, &status)),
            // Nothing to leave: the command failed, so it reads as one.
            Ok(Input::Send(Command::Part { channel, .. })) if !self.in_channel(n, &channel) => self.local_error(n, b, format!("Not in {channel}")),
            Ok(Input::Send(Command::Join { channel, .. })) if self.in_channel(n, &channel) => {
                let j = self.store.buffer_ix(n, &Target::Channel(channel));
                self.select(n, j, cx);
            }
            Ok(Input::Send(cmd)) => {
                if matches!(cmd, Command::Say { .. } | Command::Action { .. }) {
                    self.caught_up();
                }
                if let Command::Join { channel, .. } = &cmd
                    && let Some(first) = channel.split(',').next()
                {
                    self.pending_join = Some((name, first.to_owned()));
                }
                let _ = self.commands.send(cmd);
            }
            Ok(Input::OpenQuery { nick, text }) => {
                let (q, open) = open_query(&mut self.store, n, &nick);
                if let Some(cmd) = open {
                    let _ = self.commands.send(cmd);
                }
                self.sync_layout();
                self.select(n, q, cx);
                if let Some(text) = text {
                    let _ = self.commands.send(Command::Say { network: name, target: nick, text });
                }
            }
            Ok(Input::Window(WindowAction::Goto(number))) => self.goto_typed(number, cx),
            Ok(Input::Window(WindowAction::Move(number))) => self.move_window(number, cx),
            Err(msg) => self.local_error(n, b, msg),
        }
        cx.notify();
    }

    /// Are we in `channel` on network `n`? A list (`#a,#b`) is left to
    /// the server.
    fn in_channel(&self, n: usize, channel: &str) -> bool {
        channel.contains(',') || self.store.networks[n].buffers.iter().any(|b| b.joined && b.target.same(&Target::Channel(channel.to_owned())))
    }

    /// The empty input's hint: where a line typed now will go, or, when it
    /// can't go anywhere, the way out (the header says why).
    fn placeholder(&self) -> String {
        let Some((net, buf)) = self.current() else { return String::new() };
        let connected = matches!(net.status, ConnStatus::Connected { .. });
        match &buf.target {
            _ if net.status.is_down() => way_out(&net.status).to_owned(),
            Target::Channel(_) if connected && !buf.joined => "/join to rejoin".to_owned(),
            Target::Server => format!("Command for {}", net.name),
            target => format!("Message {}", target.name().unwrap_or_default()),
        }
    }

    /// Hand out nick colours in the selected buffer (`theme::slots`), in
    /// the order people first appear and apart from whoever they are seen
    /// next to, again whenever its lines change.
    fn refresh_nick_slots(&mut self) {
        let Some((n, b)) = self.selected else { return };
        let Some(buf) = self.store.networks.get(n).and_then(|net| net.buffers.get(b)) else { return };
        if self.nick_slots.get(&(n, b)).is_some_and(|(len, _)| *len == buf.lines.len()) {
            return;
        }
        let me = self.store.networks[n].nick.as_deref();
        // The other people each line names, in order.
        let lines: Vec<Vec<String>> = buf
            .lines
            .iter()
            .map(|line| {
                let named: Vec<String> = match display::event_parts(line.kind, line.nick.as_deref(), &line.text, None) {
                    Some(parts) => parts.into_iter().filter(|(part, _)| *part == display::Part::Nick).map(|(_, s)| s).collect(),
                    None => line.nick.iter().cloned().collect(),
                };
                named.into_iter().filter(|nick| !nick.is_empty() && !me.is_some_and(|me| me.eq_ignore_ascii_case(nick))).collect()
            })
            .collect();
        let slots = theme::slots(&lines, &self.palette.nick_hue);
        self.nick_slots.insert((n, b), (buf.lines.len(), slots));
    }

    /// The message view's columns, in cells.
    pub fn cols(&self) -> Columns {
        self.cols
    }

    /// The message view's width, in px.
    pub fn pane(&self) -> f32 {
        self.pane
    }

    /// The note for the header when no server is configured.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// Put the selection away.
    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.drag = None;
        if self.selection.take().is_some() {
            cx.notify();
        }
    }

    /// The text under `at` (see `buffer::hit`).
    fn hit(&self, at: gpui::Point<gpui::Pixels>) -> Option<Mark> {
        buffer::hit(&self.spots.borrow(), at)
    }

    /// The primary button went down over the message view: one click puts
    /// any selection away and may start a drag; two select the word under
    /// it, three its whole line. Focus stays in the input.
    fn press_text(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        self.press.set(Some((f32::from(e.position.x), f32::from(e.position.y))));
        self.clicks.set(e.click_count);
        self.selection = None;
        self.drag = None;
        let (Some(buffer), Some(at)) = (self.selected, self.hit(e.position)) else {
            cx.notify();
            return;
        };
        if e.click_count <= 1 {
            self.drag = Some(at);
            cx.notify();
            return;
        }
        // The line's text as drawn, to find the word in.
        let text = self
            .current()
            .and_then(|(net, buf)| Some((net, buf.lines.iter().find(|l| l.seq == at.seq)?)))
            .map(|(net, line)| buffer::log(self, line, &self.copy_context(net.nick.as_deref())).text)
            .unwrap_or_default();
        let r = if e.click_count == 2 { selection::word(&text, at.byte) } else { 0..text.len() };
        self.selection = Some(Selection { buffer, anchor: Mark { seq: at.seq, byte: r.start }, head: Mark { seq: at.seq, byte: r.end } });
        cx.notify();
    }

    /// The mouse moved with the primary button down: once past a click's
    /// slop, the selection runs from where it went down to here.
    fn drag_text(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if e.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let (Some(anchor), Some(buffer), Some(down)) = (self.drag, self.selected, self.press.get()) else { return };
        if self.selection.is_none() && links::is_click(down, (f32::from(e.position.x), f32::from(e.position.y)), self.metrics.cell / 2.) {
            return;
        }
        if let Some(head) = self.hit(e.position) {
            self.selection = Some(Selection { buffer, anchor, head });
            cx.notify();
        }
    }

    /// A line context for copying: our nick, no colours.
    fn copy_context<'a>(&self, me: Option<&'a str>) -> buffer::Context<'a> {
        static EMPTY: std::sync::LazyLock<HashMap<String, usize>> = std::sync::LazyLock::new(HashMap::new);
        buffer::Context { me, cols: self.cols, slots: &EMPTY, selected: None }
    }

    /// The selection, resolved against the shown buffer: its first and
    /// last point in order, or `None` when there is none here.
    fn resolve_selection(&self) -> Option<(selection::At, selection::At)> {
        let sel = self.selection.filter(|s| Some(s.buffer) == self.selected)?;
        let (_, buf) = self.current()?;
        let ix = |m: Mark| buf.lines.iter().position(|l| l.seq == m.seq).map(|i| (i, m.byte));
        Some(selection::order(ix(sel.anchor)?, ix(sel.head)?))
    }

    /// What a copy takes from the selection (see `selection::copy`), when
    /// it has any text.
    fn selection_text(&self) -> Option<String> {
        let (start, end) = self.resolve_selection()?;
        let (net, buf) = self.current()?;
        let cx = self.copy_context(net.nick.as_deref());
        let lines: Vec<_> = buf.lines[start.0..=end.0].iter().map(|line| buffer::log(self, line, &cx)).collect();
        Some(selection::copy(&lines, start, end)).filter(|s| !s.is_empty())
    }

    /// `⌘C` with message text selected copies it (before the input's own
    /// copy, which runs only when nothing here is selected).
    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            cx.stop_propagation();
        }
    }

    fn line_element(&mut self, ix: usize, window: &Window) -> gpui::AnyElement {
        self.refresh_nick_slots();
        let slots = self.selected.and_then(|key| self.nick_slots.get(&key)).map(|(_, s)| s);
        let empty = HashMap::new();
        let slots = slots.unwrap_or(&empty);
        match self.current().and_then(|(net, buf)| Some((net, buf.lines.get(ix)?))) {
            Some((net, line)) => {
                let selected = self.selected_span.and_then(|(start, end)| selection::span(ix, usize::MAX, start, end));
                let line_el = buffer::line(self, window, line, &buffer::Context { me: net.nick.as_deref(), cols: self.cols, slots, selected });
                let server = self.current().is_some_and(|(_, buf)| buf.target == Target::Server);
                match self.marker {
                    Some((n, b, seq)) if self.selected == Some((n, b)) && line.seq == seq => buffer::with_marker(self, line_el),
                    // In a server buffer, a quiet rule where one
                    // connection's lines end, so sessions never run together.
                    _ if server && line.boundary && ix > 0 => buffer::with_rule(self, None, line_el),
                    _ => line_el,
                }
            }
            None => div().into_any_element(),
        }
    }
}

impl Render for HircApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette.clone();
        let m = self.metrics;
        #[cfg(feature = "snapshot")]
        self.link_spots.borrow_mut().clear();
        self.spots.borrow_mut().clear();
        self.selected_span = self.resolve_selection();
        let width = f32::from(window.viewport_size().width);
        self.sidebar_width = m.sidebar_width(width);
        self.sidebar_height = f32::from(window.viewport_size().height);
        self.bar_extra = viewport::halves(self.extra.get(), window.scale_factor());
        // The time and nick columns follow the view's width alone, so the
        // text column and the input line never move when you switch
        // windows; rows wrap against them and the width, so a change
        // means measuring them again.
        let pane = width - self.sidebar_width;
        let cols = m.columns(pane);
        if cols != self.cols || pane != self.pane {
            self.cols = cols;
            self.pane = pane;
            self.lines.remeasure();
        }
        let following = scroll::following(&self.lines);
        if following {
            self.fresh_below = false;
        }
        if let Some(cmd) = read_command(&mut self.store, self.selected, &mut self.sent_read, window.is_window_active(), following) {
            let _ = self.commands.send(cmd);
        }
        // The input types on the message text column, and names its target.
        let column = m.text_column(cols);
        let placeholder = self.placeholder();
        let text_width = pane - 2. * m.gutter - column;
        self.input.update(cx, |input, _| {
            input.column = column;
            input.width = text_width;
            input.placeholder = placeholder.into();
        });
        let at_top = !following && f32::from(self.lines.scroll_px_offset_for_scrollbar().y) >= 0.;
        // While scrolled, the `-- more --` line takes the bottom row of its
        // own (it never covers a line), and a wrapped tail at the top shows
        // whose message it is.
        // Heights read before the list is laid out again are stale: wait a
        // frame (and ask for one) before acting on them.
        let settled = !std::mem::take(&mut self.settling);
        if !settled {
            window.request_animation_frame();
        }
        // All of it fits: its top shows too, and older lines are wanted.
        if settled {
            self.fits = fits(following, f32::from(self.lines.max_offset_for_scrollbar().y), self.noted, m.line);
        }
        let fits = self.fits;
        let service = self.current().is_some_and(|(_, buf)| matches!(&buf.target, Target::Query(nick) if services::is_service(nick)));
        let dry = self.selected.is_some() && self.dry == self.selected;
        self.want_older(ask_older(settled, at_top, fits, service, dry));
        let state = self.current().map_or(HistoryState::Idle, |(_, buf)| buf.history);
        let more = scroll_note(following, at_top, fits, state).map(|top| buffer::more(self, self.lines_below(), top, self.fresh_below));
        self.noted = more.is_some();
        let sticky = (!following)
            .then(|| self.cut_head())
            .flatten()
            .and_then(|ix| self.current().and_then(|(_, buf)| buf.lines.get(ix)).map(|line| buffer::sticky(self, line, cols)));
        let lines = list(self.lines.clone(), cx.processor(|this, ix: usize, window, _cx| this.line_element(ix, window))).size_full();
        let mut root = div()
            .size_full()
            .capture_action(cx.listener(|this, _: &input::Copy, _, cx| this.copy_selection(cx)))
            // A drag goes on over the whole window, and ends anywhere.
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| this.drag_text(e, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.drag = None));
        macro_rules! window_keys {
            ($($action:ident => $n:expr),*) => {
                $(root = root.on_action(cx.listener(|this, _: &$action, _, cx| this.goto_window($n, cx)));)*
            };
        }
        window_keys!(Window1 => 1, Window2 => 2, Window3 => 3, Window4 => 4, Window5 => 5, Window6 => 6, Window7 => 7, Window8 => 8, Window9 => 9, Window10 => 10);
        root.flex()
            .bg(p.background)
            .text_color(p.text)
            .text_size(px(m.size))
            .line_height(px(m.line))
            .when_some(self.font_family.clone(), |d, f| d.font_family(f))
            .child(sidebar::render(self, self.sidebar_width, self.sidebar_height, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .bg(p.buffer_bg)
                    .when_some(self.banner.clone(), |d, b| d.child(buffer::banner(self, b)))
                    .child(buffer::title(self, window, width - self.sidebar_width))
                    .child(
                        div()
                            // A link opens on a click, a drag selects; this
                            // is where the press began (see `buffer::line`).
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| this.press_text(e, cx)))
                            .relative()
                            .flex_1()
                            .min_h(px(0.))
                            .flex()
                            .flex_col()
                            // The pane keeps the bars' own padding clear on
                            // both sides of their rules.
                            .child(viewport::viewport(
                                self.lines.clone(),
                                lines.into_any_element(),
                                viewport::Frame { line: m.line, pad: m.bar_pad(), scale: window.scale_factor(), extra: self.extra.clone() },
                                self.wheel.clone(),
                                sticky,
                                more,
                            )),
                    )
                    // The input bar takes what the header does of what is
                    // left of a line (see `viewport`), split evenly about
                    // its text, so the two bars are always one height.
                    .child({
                        let (above, below) = self.bar_extra;
                        div()
                            .px(px(m.gutter))
                            .pt(px(m.bar_pad() + above))
                            .pb(px(m.bar_pad() + below))
                            .bg(p.input_bg)
                            .border_t_1()
                            .border_color(p.border)
                            .child(self.input.clone())
                    }),
            )
    }
}

/// The configured font; the family falls back as rendering does (the
/// window's default text style).
fn font_for(window: &Window, family: Option<SharedString>) -> gpui::Font {
    let mut style = window.text_style();
    if let Some(family) = family {
        style.font_family = family;
    }
    style.font()
}

/// Why a line typed for network `name` was not sent, in the words the
/// header uses (offline for a quit, disconnected for a fault). The way out
/// is the header's and the input's to say.
fn not_connected(name: &str, status: &ConnStatus) -> String {
    let state = if matches!(status, ConnStatus::Offline) { "offline" } else { "disconnected" };
    format!("Not sent: {name} is {state}")
}

/// The way back on a down network: the input's placeholder (the header
/// owns the state, the log the reason).
fn way_out(status: &ConnStatus) -> &'static str {
    match status {
        ConnStatus::Offline => "/reconnect to connect again",
        ConnStatus::Disconnected { .. } => "/reconnect to retry now",
        _ => "/reconnect to try again",
    }
}

/// The most lines a text may go out as one message each without asking
/// first, on a server that takes no multiline batches.
const PLAIN_LINES: usize = 5;

/// The hint that asks before a big send of `lines` lines as `messages`
/// messages: without multiline batches (`batches` false) once there are
/// more than `PLAIN_LINES`, since each line is a message of its own; with
/// them, once it takes more than one batch. `None` when it goes at once.
fn confirm(lines: usize, messages: usize, batches: bool) -> Option<String> {
    let ask = if batches { messages > 1 } else { lines > PLAIN_LINES };
    ask.then(|| format!("{lines} lines will be sent as {messages} messages — Enter to send, Esc to cancel"))
}

/// What goes out for `text`: shortcodes as their emoji, except in a raw
/// line, which goes as written.
fn outgoing(text: &str) -> String {
    let word = command::after_slash(text).map(|r| r.split(' ').next().unwrap_or("").to_ascii_lowercase());
    if matches!(word.as_deref(), Some("raw" | "quote")) { text.to_owned() } else { hirc_core::emoji::expand(text) }
}

/// Whether to ask for older lines of `buf` now: `up` (its top is on
/// screen, or all of it fits), nothing asked for yet and something left to
/// get, on a live connection, in a channel we are in or a query.
pub fn older_ready(buf: &Buffer, up: bool, status: &ConnStatus) -> bool {
    let place = match &buf.target {
        Target::Channel(_) => buf.joined,
        Target::Query(_) => true,
        Target::Server => false,
    };
    up && place && buf.history == HistoryState::Idle && matches!(status, ConnStatus::Connected { .. })
}

/// How far to tell the server a window on screen at its end is read: its
/// newest line from the server that someone else said (`counts`), never
/// our own JOIN or other lines of ours. Only once the server has said
/// where its marker is on this connection and the window's history for
/// it is in (`filled`), so hirc never marks lines read before their
/// divider has shown, and only when that moves the marker on. Never in a
/// services query, which hirc leaves alone unasked.
pub fn read_up_to(buf: &Buffer) -> Option<DateTime<Utc>> {
    if !buf.read_synced || !buf.filled || !markable(&buf.target) {
        return None;
    }
    let newest = buf.lines.iter().rev().find(|l| l.msgid.is_some() && counts(l))?.time;
    buf.read.is_none_or(|r| newest > r).then_some(newest)
}

/// Whether hirc ever tells the server how far `target` is read: a
/// channel or a query, but never a services query, which hirc leaves
/// alone unasked.
fn markable(target: &Target) -> bool {
    match target {
        Target::Channel(_) => true,
        Target::Query(nick) => !services::is_service(nick),
        Target::Server => false,
    }
}

/// The marker to send again when the server, on a new connection, says
/// `server` for `buf` but hirc had read further (`buf.read`): a MARKREAD
/// lost with the last socket. Never one behind the server's.
fn read_behind(buf: &Buffer, server: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    if !buf.read_synced || !markable(&buf.target) {
        return None;
    }
    buf.read.filter(|r| server.is_none_or(|s| *r > s))
}

/// The MARKREAD for the window on screen, if it may go now (`marks_read`)
/// and moves the server's marker on (`read_up_to`). The store takes it at
/// once and `sent` remembers it, so its echo leaves the divider alone.
fn read_command(
    store: &mut Store,
    selected: Option<(usize, usize)>,
    sent: &mut HashMap<(usize, usize), DateTime<Utc>>,
    active: bool,
    following: bool,
) -> Option<Command> {
    if !marks_read(active, following) {
        return None;
    }
    let (n, b) = selected?;
    let net = &store.networks[n];
    let buf = &net.buffers[b];
    let time = read_up_to(buf)?;
    let target = buf.target.name()?.to_owned();
    let network = net.name.clone();
    store.mark_read(n, b, time);
    sent.insert((n, b), time);
    Some(Command::MarkRead { network, target, time })
}

/// What an event did, for the view to follow.
struct Absorbed {
    applied: Applied,
    /// For `Applied::Read` on the window on screen: where its "new"
    /// divider goes (see `divider_after_read`).
    divider: Option<Option<u64>>,
    /// A MARKREAD to send again (see `read_behind`).
    resend: Option<Command>,
    /// The `Open` for a query window a line just opened (see `opened`).
    open: Option<Command>,
}

/// `on_event` up to the view: apply `ev` to the store and keep what hangs
/// on it up to date. That covers whether the server gave up on older
/// lines of the window on screen (`dry`), the window on screen having
/// nothing unread once the server moves its marker, and a marker lost
/// with the last connection going out again.
fn absorb(
    store: &mut Store,
    selected: Option<(usize, usize)>,
    sent: &mut HashMap<(usize, usize), DateTime<Utc>>,
    dry: &mut Option<(usize, usize)>,
    ev: Event,
) -> Absorbed {
    // A new connection (or none): what the server gave up on is asked again.
    if matches!(ev, Event::Status { .. }) {
        *dry = None;
    }
    let incoming = page_lines(&ev);
    let server = match &ev {
        Event::ReadMarker { time, .. } => Some(*time),
        _ => None,
    };
    let loading = selected.is_some_and(|(n, b)| store.networks[n].buffers[b].history == HistoryState::Loading);
    let opened = opened(store, &ev);
    let applied = store.apply(ev);
    let open = opened.map(|(network, target)| Command::Open { network, target });
    let mut got = Absorbed { applied, divider: None, resend: None, open };
    match got.applied {
        Applied::History { network, buffer, .. } if selected == Some((network, buffer)) => {
            match page_dry(loading, store.networks[network].buffers[buffer].history, incoming) {
                Some(true) => *dry = Some((network, buffer)),
                Some(false) => *dry = None,
                None => {}
            }
        }
        Applied::Read { network, buffer } => {
            let net = &mut store.networks[network];
            let buf = &mut net.buffers[buffer];
            if let Some(server) = server
                && let Some(time) = read_behind(buf, server)
                && let Some(target) = buf.target.name()
            {
                sent.insert((network, buffer), time);
                got.resend = Some(Command::MarkRead { network: net.name.clone(), target: target.to_owned(), time });
            }
            if selected == Some((network, buffer)) {
                got.divider = divider_after_read(buf, sent.get(&(network, buffer)));
                buf.unread = 0;
                buf.mentions = 0;
            }
        }
        _ => {}
    }
    got
}

/// The query `ev` would open: a line in a query window not there yet (a
/// PM to us, or our own `/msg`). Its window needs an `Open`, as one
/// `/query` opens does, so its history and read marker sync on this
/// connection. Never a services query, which hirc leaves alone unasked.
fn opened(store: &Store, ev: &Event) -> Option<(String, String)> {
    let Event::Line { network, target: Target::Query(nick), .. } = ev else { return None };
    if services::is_service(nick) {
        return None;
    }
    let held = store.networks.iter().find(|n| n.name == *network).is_some_and(|n| n.buffers.iter().any(|b| b.target.same(&Target::Query(nick.clone()))));
    (!held).then(|| (network.clone(), nick.clone()))
}

/// May the window on screen be marked read now? Only while hirc is the
/// active window (`active`) and the view is at its newest line
/// (`following`): a marker sent otherwise would tell the user's other
/// clients they read lines they never saw.
fn marks_read(active: bool, following: bool) -> bool {
    active && following
}

/// Where the "new" divider goes once the server has moved `buf`'s read
/// marker: `Some(seq)` of the first line past it (`Some(None)`, none),
/// or `None` to leave it, when the marker is the one hirc last sent
/// (`sent`) coming back.
fn divider_after_read(buf: &Buffer, sent: Option<&DateTime<Utc>>) -> Option<Option<u64>> {
    if buf.read.is_some() && sent == buf.read.as_ref() {
        return None;
    }
    Some(buf.first_unread().map(|ix| buf.lines[ix].seq))
}

/// How many lines a history page brought, before any already here were
/// left out: what says whether the server still has some to give.
fn page_lines(ev: &Event) -> usize {
    match ev {
        Event::History { lines, .. } => lines.len(),
        _ => 0,
    }
}

/// Does all of the window fit in the view? Only while following (scrolled
/// up, it overflows). `max` is the list's scroll range as last laid out.
/// With the note's row taken last frame (`noted`) the list was a row
/// shorter, so a row over still fits, else the note would come and go.
fn fits(following: bool, max: f32, noted: bool, line: f32) -> bool {
    following && max <= if noted { line } else { 0. }
}

/// Ask for older lines this frame? Not while the list's heights are stale
/// (`settled` false: reset, or a page went in, since it was laid out),
/// nor in a window the server gave up on (`dry`). At its top, the user
/// scrolled there and asks; a window that all fits asks on its own,
/// except a services query, which is never brought up to date unasked.
fn ask_older(settled: bool, at_top: bool, fits: bool, service: bool, dry: bool) -> bool {
    settled && !dry && (at_top || (fits && !service))
}

/// The scroll row, if it shows, and what it says of the top (`None` while
/// the top isn't on screen). Scrolled away from the newest line it always
/// shows. A window that all fits (so following) shows it only while an
/// older page is loading: never "start of history", which under the
/// newest line would read as if the talk ended there.
fn scroll_note(following: bool, at_top: bool, fits: bool, state: HistoryState) -> Option<Option<HistoryState>> {
    if fits {
        return (state == HistoryState::Loading).then_some(Some(HistoryState::Loading));
    }
    (!following).then(|| at_top.then_some(state))
}

/// What a page of history for the window on screen says of asking again:
/// `Some(true)`, an older page asked for (`loading` before it) came back
/// with nothing and not at the start, so the server has given up on it
/// for now; `Some(false)`, lines came, ask freely; `None`, no change.
/// `sent` counts the page's lines as the server sent them: lines already
/// here (a latest page repeating what came live) still say it has more.
fn page_dry(loading: bool, now: HistoryState, sent: usize) -> Option<bool> {
    if sent > 0 {
        Some(false)
    } else if loading && now == HistoryState::Idle {
        Some(true)
    } else {
        None
    }
}

/// The `Open` for each query window from last time, for the network side
/// to bring up to date once connected (channels catch up as they are
/// joined). Services queries are left out: never brought up to date
/// unasked.
fn reopen(store: &Store) -> Vec<Command> {
    store
        .networks
        .iter()
        .flat_map(|net| {
            net.buffers.iter().filter_map(|buf| match &buf.target {
                Target::Query(nick) if !services::is_service(nick) => Some(Command::Open { network: net.name.clone(), target: nick.clone() }),
                _ => None,
            })
        })
        .collect()
}

/// `/query nick` on network `n`: its window, and the `Open` that brings it
/// up to date when the window is new (the user opened it, so a services
/// query too).
fn open_query(store: &mut Store, n: usize, nick: &str) -> (usize, Option<Command>) {
    let before = store.networks[n].buffers.len();
    let q = store.buffer_ix(n, &Target::Query(nick.to_owned()));
    let open = (store.networks[n].buffers.len() > before).then(|| Command::Open { network: store.networks[n].name.clone(), target: nick.to_owned() });
    (q, open)
}

/// "No window 30 (there are 6)."
fn no_window(number: usize, count: usize) -> String {
    format!("No window {number} (there are {count})")
}

/// The configured font's shape, for `Metrics`.
fn measure(window: &Window, font: &gpui::Font, size: f32) -> Shape {
    let ts = window.text_system();
    let id = ts.resolve_font(font);
    let size = px(size);
    let per_px = |v: gpui::Pixels| f32::from(v) / f32::from(size);
    match ts.ch_advance(id, size) {
        Ok(cell) => Shape { cell: per_px(cell), ascent: per_px(ts.ascent(id, size)).abs(), descent: per_px(ts.descent(id, size)).abs() },
        Err(_) => Shape::FALLBACK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_lines_are_asked_for_only_when_they_can_come() {
        use hirc_core::store::HistoryState;
        let up = ConnStatus::Connected { nick: "me".into() };
        let mut chan = Buffer::new(Target::Channel("#a".into()));
        assert!(!older_ready(&chan, true, &up), "not joined");
        chan.joined = true;
        assert!(older_ready(&chan, true, &up));
        assert!(!older_ready(&chan, false, &up), "not at the top");
        assert!(!older_ready(&chan, true, &ConnStatus::Connecting));
        chan.history = HistoryState::Loading;
        assert!(!older_ready(&chan, true, &up), "one at a time");
        chan.history = HistoryState::Start;
        assert!(!older_ready(&chan, true, &up), "nothing older");
        assert!(older_ready(&Buffer::new(Target::Query("bob".into())), true, &up));
        assert!(!older_ready(&Buffer::new(Target::Server), true, &up));
    }

    #[test]
    fn the_scroll_row_shows_where_it_should() {
        use HistoryState::*;
        // Following, overflowing: no row, whatever the state.
        for s in [Idle, Loading, Start] {
            assert_eq!(scroll_note(true, false, false, s), None);
        }
        // Scrolled up, top not on screen: the count alone.
        assert_eq!(scroll_note(false, false, false, Start), Some(None));
        // Scrolled up to the top of an overflowing window: every note.
        for s in [Idle, Loading, Start] {
            assert_eq!(scroll_note(false, true, false, s), Some(Some(s)));
        }
        // A window that fits: only while loading, never the start note.
        assert_eq!(scroll_note(true, false, true, Loading), Some(Some(Loading)));
        assert_eq!(scroll_note(true, false, true, Start), None);
        assert_eq!(scroll_note(true, false, true, Idle), None);
    }

    #[test]
    fn a_window_fits_only_while_following_and_the_note_row_is_slack() {
        let line = 21.;
        assert!(fits(true, 0., false, line));
        assert!(!fits(true, 1., false, line));
        // The note took a row: a row over still fits, more doesn't.
        assert!(fits(true, line, true, line));
        assert!(!fits(true, line + 1., true, line));
        // Scrolled up, it overflows.
        assert!(!fits(false, 0., false, line));
    }

    #[test]
    fn older_lines_wait_for_a_laid_out_frame_and_the_user_for_services() {
        // Stale heights after a reset or a page: no ask, even "at the top".
        assert!(!ask_older(false, true, false, false, false));
        assert!(!ask_older(false, false, true, false, false));
        assert!(ask_older(true, true, false, false, false));
        assert!(ask_older(true, false, true, false, false));
        assert!(!ask_older(true, false, false, false, false));
        // A services query that fits never asks; scrolled to its top it does.
        assert!(!ask_older(true, false, true, true, false));
        assert!(ask_older(true, true, false, true, false));
        // The server gave up: not asked again.
        assert!(!ask_older(true, true, false, false, true));
        assert!(!ask_older(true, false, true, false, true));
    }

    #[test]
    fn only_an_answered_empty_page_stops_asking() {
        use HistoryState::*;
        // Asked, answered with nothing, not the start: dry.
        assert_eq!(page_dry(true, Idle, 0), Some(true));
        // Lines came: ask freely (also clears an earlier dry).
        assert_eq!(page_dry(true, Idle, 3), Some(false));
        assert_eq!(page_dry(false, Idle, 2), Some(false));
        // The start: `Start` stops asking, not dry.
        assert_eq!(page_dry(true, Start, 0), None);
        // An empty page nobody asked older for (a backfill): no change.
        assert_eq!(page_dry(false, Idle, 0), None);
    }

    #[test]
    fn a_request_lost_with_the_connection_is_asked_again() {
        // The store puts a lost request back to Idle on the drop (no page),
        // and `dry` is only ever set by an answered empty page: nothing
        // remembers the lost request, so the next connection asks.
        let up = ConnStatus::Connected { nick: "me".into() };
        let mut store = Store::new(["net".to_owned()]);
        let b = store.buffer_ix(0, &Target::Query("bob".into()));
        store.networks[0].status = up.clone();
        store.networks[0].buffers[b].history = HistoryState::Loading;
        store.apply(Event::Status { network: "net".into(), status: ConnStatus::Connecting });
        assert_eq!(store.networks[0].buffers[b].history, HistoryState::Idle);
        store.apply(Event::Status { network: "net".into(), status: up.clone() });
        let buf = &store.networks[0].buffers[b];
        assert!(ask_older(true, false, true, false, false) && older_ready(buf, true, &up));
    }

    #[test]
    fn new_lines_do_not_rearm_a_window_the_server_gave_up_on() {
        // `dry` is per window, not per line count: lines arriving after the
        // empty answer leave it set (only a page with lines, a new
        // connection or leaving the window clear it).
        assert_eq!(page_dry(false, HistoryState::Idle, 0), None);
        assert!(!ask_older(true, false, true, false, true));
    }

    #[test]
    fn query_windows_from_last_time_reopen_but_services() {
        let mut store = Store::new(["net".to_owned()]);
        for t in [Target::Query("bob".into()), Target::Query("NickServ".into()), Target::Channel("#a".into()), Target::Query("chanserv".into())] {
            store.buffer_ix(0, &t);
        }
        assert_eq!(reopen(&store), [Command::Open { network: "net".into(), target: "bob".into() }]);
    }

    #[test]
    fn query_opens_a_new_window_once() {
        let mut store = Store::new(["net".to_owned()]);
        let (q, open) = open_query(&mut store, 0, "bob");
        assert_eq!(open, Some(Command::Open { network: "net".into(), target: "bob".into() }));
        // Already open: no second Open (case-insensitively the same window).
        assert_eq!(open_query(&mut store, 0, "Bob"), (q, None));
        // The user opened it: a services query gets one too.
        assert_eq!(open_query(&mut store, 0, "NickServ").1, Some(Command::Open { network: "net".into(), target: "NickServ".into() }));
    }

    fn at(s: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000 + s, 0).unwrap()
    }

    fn said(s: i64, id: Option<&str>) -> hirc_core::store::Line {
        use hirc_core::store::{Delivery, Line};
        Line {
            seq: 0,
            time: at(s),
            msgid: id.map(Into::into),
            kind: LineKind::Privmsg,
            nick: Some("bob".into()),
            text: "x".into(),
            own: false,
            delivery: Delivery::Sent,
            boundary: false,
            replayed: false,
        }
    }

    #[test]
    fn the_server_hears_how_far_a_window_is_read_once_it_has_said() {
        let mut s = Store::new(["n".to_string()]);
        let chan = || Target::Channel("#a".into());
        s.apply(Event::Line { network: "n".into(), target: chan(), line: said(1, Some("m1")) });
        // Ours, still waiting for its echo: not the server's yet.
        s.apply(Event::Line { network: "n".into(), target: chan(), line: said(2, None) });
        let b = s.buffer_ix(0, &chan());
        assert_eq!(read_up_to(&s.networks[0].buffers[b]), None, "the server hasn't said where its marker is");
        s.apply(Event::ReadMarker { network: "n".into(), target: chan(), time: None });
        assert_eq!(read_up_to(&s.networks[0].buffers[b]), None, "its history isn't in yet");
        s.apply(Event::Filled { network: "n".into(), target: chan() });
        assert_eq!(read_up_to(&s.networks[0].buffers[b]), Some(at(1)));
        s.mark_read(0, b, at(1));
        assert_eq!(read_up_to(&s.networks[0].buffers[b]), None, "already there");
        assert_eq!(read_up_to(&s.networks[0].buffers[0]), None, "the server buffer has no marker");
    }

    #[test]
    fn services_queries_are_never_marked_read() {
        let mut s = Store::new(["n".to_string()]);
        let q = || Target::Query("NickServ".into());
        s.apply(Event::Line { network: "n".into(), target: q(), line: said(1, Some("m1")) });
        s.apply(Event::ReadMarker { network: "n".into(), target: q(), time: None });
        let b = s.buffer_ix(0, &q());
        assert!(s.networks[0].buffers[b].read_synced);
        assert_eq!(read_up_to(&s.networks[0].buffers[b]), None);
    }

    #[test]
    fn a_window_is_marked_read_only_while_hirc_is_in_front_and_at_the_bottom() {
        assert!(marks_read(true, true));
        assert!(!marks_read(false, true), "hirc isn't focused: the user may not have seen it");
        assert!(!marks_read(true, false), "scrolled up: the newest lines aren't on screen");
        assert!(!marks_read(false, false));
    }

    #[test]
    fn the_divider_follows_the_servers_marker_but_not_our_own() {
        let mut s = Store::new(["n".to_string()]);
        let chan = || Target::Channel("#a".into());
        for (t, id) in [(1, "m1"), (2, "m2"), (3, "m3")] {
            s.apply(Event::Line { network: "n".into(), target: chan(), line: said(t, Some(id)) });
        }
        let b = s.buffer_ix(0, &chan());
        // Another client read up to m1: the divider goes above m2.
        s.apply(Event::ReadMarker { network: "n".into(), target: chan(), time: Some(at(1)) });
        let buf = &s.networks[0].buffers[b];
        assert_eq!(divider_after_read(buf, None), Some(Some(buf.lines[1].seq)));
        // Our own marker coming back leaves it where it is.
        s.mark_read(0, b, at(3));
        s.apply(Event::ReadMarker { network: "n".into(), target: chan(), time: Some(at(3)) });
        let buf = &s.networks[0].buffers[b];
        assert_eq!(divider_after_read(buf, Some(&at(3))), None);
        // Read to the end elsewhere: no divider.
        assert_eq!(divider_after_read(buf, Some(&at(2))), Some(None));
    }

    /// A store with `lines` said in `target` on network "n", and the
    /// server's word on where its marker is.
    fn with_lines(target: &Target, lines: &[(i64, &str)], marker: Option<i64>) -> (Store, usize) {
        let mut s = Store::new(["n".to_string()]);
        for &(t, id) in lines {
            s.apply(Event::Line { network: "n".into(), target: target.clone(), line: said(t, Some(id)) });
        }
        s.apply(Event::ReadMarker { network: "n".into(), target: target.clone(), time: marker.map(at) });
        s.apply(Event::Filled { network: "n".into(), target: target.clone() });
        let b = s.buffer_ix(0, target);
        (s, b)
    }

    fn marker(target: &Target, time: Option<i64>) -> Event {
        Event::ReadMarker { network: "n".into(), target: target.clone(), time: time.map(at) }
    }

    fn mark(target: &str, time: i64) -> Command {
        Command::MarkRead { network: "n".into(), target: target.into(), time: at(time) }
    }

    #[test]
    fn the_window_on_screen_is_marked_read_only_while_in_front_and_at_the_bottom() {
        let chan = Target::Channel("#a".into());
        let (mut s, b) = with_lines(&chan, &[(1, "m1"), (2, "m2")], None);
        let mut sent = HashMap::new();
        let on = Some((0, b));
        assert_eq!(read_command(&mut s, on, &mut sent, false, true), None, "hirc isn't focused");
        assert_eq!(read_command(&mut s, on, &mut sent, true, false), None, "scrolled up");
        assert_eq!((s.networks[0].buffers[b].read, sent.len()), (None, 0), "nothing marked while held back");
        // Back in front at the bottom: it goes now, once.
        assert_eq!(read_command(&mut s, on, &mut sent, true, true), Some(mark("#a", 2)));
        assert_eq!((s.networks[0].buffers[b].read, sent.get(&(0, b))), (Some(at(2)), Some(&at(2))));
        assert_eq!(read_command(&mut s, on, &mut sent, true, true), None, "already there");
        // Its echo is ours: the divider stays, nothing is sent again.
        let got = absorb(&mut s, on, &mut sent, &mut None, marker(&chan, Some(2)));
        assert_eq!((got.divider, got.resend), (None, None));
    }

    #[test]
    fn a_marker_on_the_window_on_screen_leaves_nothing_unread() {
        let q = Target::Query("bob".into());
        let (mut s, b) = with_lines(&q, &[(1, "m1"), (2, "m2"), (3, "m3")], None);
        let first_new = s.networks[0].buffers[b].lines[1].seq;
        // Elsewhere: the counts follow the marker.
        absorb(&mut s, None, &mut HashMap::new(), &mut None, marker(&q, Some(1)));
        assert_eq!((s.networks[0].buffers[b].unread, s.networks[0].buffers[b].mentions), (2, 2));
        // On screen: the divider moves above m2, and nothing is unread.
        let got = absorb(&mut s, Some((0, b)), &mut HashMap::new(), &mut None, marker(&q, Some(1)));
        assert_eq!(got.divider, Some(Some(first_new)));
        assert_eq!((s.networks[0].buffers[b].unread, s.networks[0].buffers[b].mentions), (0, 0));
    }

    #[test]
    fn whether_the_server_gave_up_follows_the_lines_it_sent() {
        let chan = Target::Channel("#a".into());
        let (mut s, b) = with_lines(&chan, &[(1, "m1")], None);
        let on = Some((0, b));
        let page = |lines| Event::History { network: "n".into(), target: chan.clone(), lines, start: false, older: false };
        // Only lines already here: the server still has some.
        s.networks[0].buffers[b].history = HistoryState::Loading;
        let mut dry = None;
        absorb(&mut s, on, &mut HashMap::new(), &mut dry, page(vec![said(1, Some("m1"))]));
        assert_eq!(dry, None);
        // Nothing at all: it gave up.
        s.networks[0].buffers[b].history = HistoryState::Loading;
        absorb(&mut s, on, &mut HashMap::new(), &mut dry, page(vec![]));
        assert_eq!(dry, on);
        // A new connection asks again.
        absorb(&mut s, on, &mut HashMap::new(), &mut dry, Event::Status { network: "n".into(), status: ConnStatus::Connecting });
        assert_eq!(dry, None);
    }

    #[test]
    fn a_marker_lost_with_the_connection_is_sent_again() {
        let chan = Target::Channel("#a".into());
        let (mut s, b) = with_lines(&chan, &[(1, "m1"), (2, "m2")], Some(1));
        let mut sent = HashMap::new();
        assert_eq!(read_command(&mut s, Some((0, b)), &mut sent, true, true), Some(mark("#a", 2)));
        // The socket went before the server had it: the next one says m1.
        absorb(&mut s, None, &mut sent, &mut None, Event::Status { network: "n".into(), status: ConnStatus::Connecting });
        let got = absorb(&mut s, None, &mut sent, &mut None, marker(&chan, Some(1)));
        assert_eq!(got.resend, Some(mark("#a", 2)));
        assert_eq!(sent.get(&(0, b)), Some(&at(2)));
        // A server with none at all hears it too.
        assert_eq!(absorb(&mut s, None, &mut sent, &mut None, marker(&chan, None)).resend, Some(mark("#a", 2)));
        // Never backwards, and not when the server has ours.
        assert_eq!(absorb(&mut s, None, &mut sent, &mut None, marker(&chan, Some(2))).resend, None);
        assert_eq!(absorb(&mut s, None, &mut sent, &mut None, marker(&chan, Some(3))).resend, None);
        assert_eq!(s.networks[0].buffers[b].read, Some(at(3)));
        // Never for services.
        let ns = Target::Query("NickServ".into());
        let (mut s, b) = with_lines(&ns, &[(1, "m1"), (2, "m2")], Some(1));
        s.mark_read(0, b, at(2));
        assert_eq!(absorb(&mut s, None, &mut HashMap::new(), &mut None, marker(&ns, Some(1))).resend, None);
    }

    /// Back after a drop with #a on screen: the server says our JOIN (with
    /// a msgid, stamped now) and its marker before the gap fill comes. No
    /// marker goes out until the fill is in, and then only up to the
    /// newest line someone said: the divider sits above what was missed.
    #[test]
    fn a_reconnect_marks_nothing_read_until_the_fill_is_in() {
        use hirc_core::store::Line;
        let chan = Target::Channel("#a".into());
        let (mut s, b) = with_lines(&chan, &[(1, "m1")], Some(1));
        let on = Some((0, b));
        let mut sent = HashMap::new();
        let status = |status| Event::Status { network: "n".into(), status };
        absorb(&mut s, on, &mut sent, &mut None, status(ConnStatus::Connecting));
        absorb(&mut s, on, &mut sent, &mut None, status(ConnStatus::Connected { nick: "me".into() }));
        absorb(&mut s, on, &mut sent, &mut None, Event::Joined { network: "n".into(), channel: "#a".into() });
        // Another device read up to m2.
        absorb(&mut s, on, &mut sent, &mut None, marker(&chan, Some(2)));
        let join = Line { kind: LineKind::Join, own: true, nick: Some("me".into()), ..said(10, Some("j1")) };
        absorb(&mut s, on, &mut sent, &mut None, Event::Line { network: "n".into(), target: chan.clone(), line: join });
        assert_eq!(read_command(&mut s, on, &mut sent, true, true), None, "the fill isn't in");
        let missed = |t, id| Line { replayed: true, ..said(t, Some(id)) };
        let page = Event::History { network: "n".into(), target: chan.clone(), lines: vec![missed(2, "m2"), missed(3, "m3")], start: false, older: false };
        absorb(&mut s, on, &mut sent, &mut None, page);
        let buf = &s.networks[0].buffers[b];
        let ix = buf.first_unread().expect("m3 is new");
        assert_eq!(buf.lines[ix].msgid.as_deref(), Some("m3"));
        absorb(&mut s, on, &mut sent, &mut None, Event::Filled { network: "n".into(), target: chan.clone() });
        assert_eq!(read_command(&mut s, on, &mut sent, true, true), Some(mark("#a", 3)), "up to m3, not our JOIN");
    }

    /// A query a line opens (bob's PM, or our `/msg carol`) is brought up
    /// to date like one `/query` opens, so its marker syncs; never a
    /// services query.
    #[test]
    fn a_query_a_line_opens_is_brought_up_to_date() {
        use hirc_core::store::{Delivery, Line};
        let mut s = Store::new(["n".to_string()]);
        let mut line = |target: &str, line| {
            absorb(&mut s, None, &mut HashMap::new(), &mut None, Event::Line { network: "n".into(), target: Target::Query(target.into()), line }).open
        };
        let open = |t: &str| Some(Command::Open { network: "n".into(), target: t.into() });
        assert_eq!(line("bob", said(1, Some("p1"))), open("bob"));
        assert_eq!(line("bob", said(2, Some("p2"))), None, "open already");
        let ours = Line { own: true, delivery: Delivery::Pending, ..said(3, None) };
        assert_eq!(line("carol", ours), open("carol"));
        assert_eq!(line("NickServ", said(4, Some("n1"))), None);
    }

    #[test]
    fn a_page_that_only_repeats_lines_already_here_is_not_dry() {
        let mut s = Store::new(["n".to_string()]);
        let chan = || Target::Channel("#a".into());
        s.apply(Event::Line { network: "n".into(), target: chan(), line: said(1, Some("m1")) });
        let b = s.buffer_ix(0, &chan());
        s.networks[0].buffers[b].history = HistoryState::Loading;
        let page = Event::History { network: "n".into(), target: chan(), lines: vec![said(1, Some("m1"))], start: false, older: false };
        let incoming = page_lines(&page);
        let Applied::History { at, .. } = s.apply(page) else { panic!("not a page") };
        assert!(at.is_empty(), "the line was already here");
        assert_eq!(page_dry(true, s.networks[0].buffers[b].history, incoming), Some(false), "the server sent lines: it hasn't given up");
        let empty = Event::History { network: "n".into(), target: chan(), lines: vec![], start: false, older: false };
        assert_eq!(page_lines(&empty), 0);
        assert_eq!(page_lines(&Event::OwnNick { network: "n".into(), nick: "me".into() }), 0);
    }

    #[test]
    fn big_sends_ask_first() {
        // Without batches, more than five lines asks; five goes at once.
        assert_eq!(confirm(5, 5, false), None);
        assert_eq!(confirm(12, 12, false).as_deref(), Some("12 lines will be sent as 12 messages — Enter to send, Esc to cancel"));
        // With them, only more than one batch asks.
        assert_eq!(confirm(24, 1, true), None);
        assert_eq!(confirm(30, 2, true).as_deref(), Some("30 lines will be sent as 2 messages — Enter to send, Esc to cancel"));
    }

    #[test]
    fn shortcodes_go_out_as_emoji_but_raw_lines_as_written() {
        assert_eq!(outgoing("ship it :tada:"), "ship it 🎉");
        assert_eq!(outgoing("/me waves :wave:"), "/me waves 👋");
        assert_eq!(outgoing("/RAW PRIVMSG #a ::tada:"), "/RAW PRIVMSG #a ::tada:");
        assert_eq!(outgoing("/quote X :tada:"), "/quote X :tada:");
        // Raw exactly where the parser reads a raw command.
        assert_eq!(outgoing("  /raw X :tada:"), "  /raw X 🎉");
        assert_eq!(outgoing("//raw :tada:"), "//raw 🎉");
        assert_eq!(outgoing("/raw\tX :tada:"), "/raw\tX 🎉");
    }
}
