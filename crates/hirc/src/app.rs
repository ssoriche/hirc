//! App state and the event loop. The network thread sends `Event`s; we
//! apply them to the `Store` and redraw. Input goes back as `Command`s.

use crate::input::{Submit, TextInput, VimEvent};
use crate::metrics::{Columns, Metrics, Shape};
use crate::scroll::{self, Pos};
use crate::theme::{self, Palette, palette};
use crate::views::{buffer, sidebar, viewport};
use crate::vim::{self, Effect, Ex, Nav, Scroll};
use gpui::{App, Context, Entity, Focusable, KeyBinding, ListAlignment, ListState, ScrollHandle, SharedString, Window, actions, div, list, prelude::*, px};
use hirc_core::command::{self, Input, WindowAction};
use hirc_core::display;
use hirc_core::event::{Command, ConnStatus, Event, Target};
use hirc_core::layout::{self, Layout};
use hirc_core::store::{Applied, Buffer, LineKind, Network, Store};
use hirc_core::theme::Theme;
use std::cell::Cell;
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
}

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
    /// view).
    cols: Columns,
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
    /// The channel a /join asked for, by network name: its window is
    /// focused when the server confirms the join.
    pending_join: Option<(String, String)>,
    /// Where the unread lines began when this window was entered: the
    /// window and the first unread line's seq (a "new" rule is drawn
    /// above it until the window is left).
    marker: Option<(usize, usize, u64)>,
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
        cx.subscribe(&input, |this, _, ev: &Submit, cx| this.submit(ev.0.clone(), cx)).detach();
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
            wheel: Rc::default(),
            extra: Rc::default(),
            bar_extra: (0., 0.),
            nick_slots: HashMap::new(),
            fresh_below: false,
            pending_join: None,
            marker: None,
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
            let buf = &self.store.networks[n].buffers[b];
            self.marker = buf.first_unread().map(|ix| (n, b, buf.lines[ix].seq));
        }
        self.selected = Some((n, b));
        let buf = &mut self.store.networks[n].buffers[b];
        buf.unread = 0;
        buf.mentions = 0;
        self.lines.reset(buf.lines.len());
        self.fresh_below = false;
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
                Ex::Quit => cx.quit(),
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
        match self.store.apply(ev) {
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

    /// Show a local error line in the current buffer.
    fn local_error(&mut self, n: usize, b: usize, msg: String) {
        let index = self.store.push_local(n, b, LineKind::Error, msg);
        self.lines.splice(index..index, 1);
    }

    /// Show hirc's own answer to a command (`-!-`, the client's voice, not
    /// the server's `--`) in the current buffer.
    fn local_note(&mut self, n: usize, b: usize, msg: String) {
        let index = self.store.push_local(n, b, LineKind::Status, msg);
        self.lines.splice(index..index, 1);
    }

    fn submit(&mut self, text: String, cx: &mut Context<Self>) {
        if text.trim().is_empty() {
            return;
        }
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
            // Nothing to leave: hirc's own answer, not a fault.
            Ok(Input::Send(Command::Part { channel, .. })) if !self.in_channel(n, &channel) => self.local_note(n, b, format!("Not in {channel}")),
            Ok(Input::Send(Command::Join { channel, .. })) if self.in_channel(n, &channel) => {
                let j = self.store.buffer_ix(n, &Target::Channel(channel));
                self.select(n, j, cx);
            }
            Ok(Input::Send(cmd)) => {
                if let Command::Join { channel, .. } = &cmd
                    && let Some(first) = channel.split(',').next()
                {
                    self.pending_join = Some((name, first.to_owned()));
                }
                let _ = self.commands.send(cmd);
            }
            Ok(Input::OpenQuery { nick, text }) => {
                let q = self.store.buffer_ix(n, &Target::Query(nick.clone()));
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

    /// The note for the header when no server is configured.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    fn line_element(&mut self, ix: usize) -> gpui::AnyElement {
        self.refresh_nick_slots();
        let slots = self.selected.and_then(|key| self.nick_slots.get(&key)).map(|(_, s)| s);
        let empty = HashMap::new();
        let slots = slots.unwrap_or(&empty);
        match self.current().and_then(|(net, buf)| Some((net, buf.lines.get(ix)?))) {
            Some((net, line)) => {
                let line_el = buffer::line(self, line, &buffer::Context { me: net.nick.as_deref(), cols: self.cols, slots });
                match self.marker {
                    Some((n, b, seq)) if self.selected == Some((n, b)) && line.seq == seq => buffer::with_marker(self, line_el),
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
        let width = f32::from(window.viewport_size().width);
        self.sidebar_width = m.sidebar_width(width);
        self.sidebar_height = f32::from(window.viewport_size().height);
        self.bar_extra = viewport::halves(self.extra.get(), window.scale_factor());
        // The time and nick columns follow the view's width; rows wrap
        // against them, so a change means measuring them again.
        let cols = m.columns(width - self.sidebar_width);
        if cols != self.cols {
            self.cols = cols;
            self.lines.remeasure();
        }
        let following = scroll::following(&self.lines);
        if following {
            self.fresh_below = false;
        }
        // The input types on the message text column, and names its target.
        let column = m.text_column(cols);
        let placeholder = self.placeholder();
        self.input.update(cx, |input, _| {
            input.column = column;
            input.placeholder = placeholder.into();
        });
        let at_top = !following && f32::from(self.lines.scroll_px_offset_for_scrollbar().y) >= 0.;
        // While scrolled, the `-- more --` line takes the bottom row of its
        // own (it never covers a line), and a wrapped tail at the top shows
        // whose message it is.
        let more = (!following).then(|| buffer::more(self, self.lines_below(), at_top, self.fresh_below));
        let sticky = (!following)
            .then(|| self.cut_head())
            .flatten()
            .and_then(|ix| self.current().and_then(|(_, buf)| buf.lines.get(ix)).map(|line| buffer::sticky(self, line, cols)));
        let lines = list(self.lines.clone(), cx.processor(|this, ix: usize, _window, _cx| this.line_element(ix))).size_full();
        let mut root = div().size_full();
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
/// header uses (offline for a quit, disconnected for a fault), and the
/// way out: the line stays in the input, so the placeholder is hidden.
fn not_connected(name: &str, status: &ConnStatus) -> String {
    let state = if matches!(status, ConnStatus::Offline) { "offline" } else { "disconnected" };
    format!("Not sent: {name} is {state} — {}", way_out(status))
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
