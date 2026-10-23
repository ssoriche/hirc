//! App state and the event loop. The network thread sends `Event`s; we
//! apply them to the `Store` and redraw. Input goes back as `Command`s.

use crate::input::{Submit, TextInput, VimEvent};
use crate::theme::{Palette, palette};
use crate::views::{buffer, sidebar};
use crate::vim::{self, Effect, Ex, Nav, Scroll};
use gpui::{App, Context, Entity, Focusable, KeyBinding, ListAlignment, ListOffset, ListState, SharedString, Window, actions, div, list, prelude::*, px};
use hirc_core::command::{self, Input, WindowAction};
use hirc_core::event::{Command, Event, Target};
use hirc_core::layout::{self, Layout};
use hirc_core::store::{Applied, Buffer, LineKind, Network, Store};
use hirc_core::theme::Theme;
use std::path::PathBuf;
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
    pub commands: UnboundedSender<Command>,
    pub events: UnboundedReceiver<Event>,
}

pub struct HircApp {
    pub store: Store,
    pub selected: Option<(usize, usize)>,
    pub layout: Layout,
    layout_path: PathBuf,
    pub palette: Palette,
    font_family: Option<SharedString>,
    font_size: f32,
    banner: Option<String>,
    input: Entity<TextInput>,
    lines: ListState,
    commands: UnboundedSender<Command>,
}

impl HircApp {
    pub fn new(init: Init, window: &mut Window, cx: &mut Context<Self>) -> HircApp {
        let mut store = Store::new(init.networks.clone());
        let layout_path = layout::path();
        let saved = Layout::load(&layout_path);
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
            && let Err(e) = layout.save(&layout_path)
        {
            tracing::warn!("could not save window layout: {e}");
        }
        let selected = layout.resolve(&store, 0).or_else(|| (!store.networks.is_empty()).then_some((0, 0)));
        let palette = palette(&init.theme);
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Message", cx);
            input.palette = Some(palette.clone());
            input
        });
        cx.subscribe(&input, |this, _, ev: &Submit, cx| this.submit(ev.0.clone(), cx)).detach();
        cx.subscribe(&input, |this, _, ev: &VimEvent, cx| this.vim_effect(&ev.0, cx)).detach();
        window.focus(&input.read(cx).focus_handle(cx));
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
            palette,
            font_family: init.font_family.map(Into::into),
            font_size: init.font_size,
            banner: init.banner,
            input,
            lines: ListState::new(0, ListAlignment::Bottom, px(400.)),
            commands: init.commands,
        }
    }

    pub fn current(&self) -> Option<(&Network, &Buffer)> {
        let (n, b) = self.selected?;
        let net = self.store.networks.get(n)?;
        Some((net, net.buffers.get(b)?))
    }

    pub fn select(&mut self, n: usize, b: usize, cx: &mut Context<Self>) {
        self.selected = Some((n, b));
        let buf = &mut self.store.networks[n].buffers[b];
        buf.unread = 0;
        self.lines.reset(buf.lines.len());
        cx.notify();
    }

    /// Give any new buffer a window number and persist the list.
    fn sync_layout(&mut self) {
        if self.layout.sync(&self.store) {
            self.save_layout();
        }
    }

    fn save_layout(&self) {
        if let Err(e) = self.layout.save(&self.layout_path) {
            tracing::warn!("could not save window layout: {e}");
        }
    }

    /// `number` is 1-based, as typed; out of range does nothing.
    pub fn goto_window(&mut self, number: usize, cx: &mut Context<Self>) {
        if let Some((n, b)) = number.checked_sub(1).and_then(|pos| self.layout.resolve(&self.store, pos)) {
            self.select(n, b, cx);
        }
    }

    /// Number of numbered windows.
    fn window_count(&self) -> usize {
        (0..).take_while(|&pos| self.layout.resolve(&self.store, pos).is_some()).count()
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
        let found = (0..).map_while(|pos| self.layout.resolve(&self.store, pos)).find(|&(n, b)| {
            let net = &self.store.networks[n];
            let label = net.buffers[b].target.name().unwrap_or(&net.name);
            label.to_lowercase().starts_with(&want)
        });
        match (found, self.selected) {
            (Some((n, b)), _) => self.select(n, b, cx),
            (None, Some((n, b))) => {
                self.local_error(n, b, format!("No buffer matching {name}"));
                cx.notify();
            }
            (None, None) => {}
        }
    }

    fn scroll(&mut self, s: Scroll) {
        let line = px(self.font_size * 1.4);
        // The list reports its viewport once laid out; before that, guess.
        let page = match self.lines.viewport_bounds().size.height {
            h if h > px(0.) => h,
            _ => px(400.),
        };
        match s {
            Scroll::Lines(n) => self.lines.scroll_by(line * n as f32),
            Scroll::HalfPage(n) => self.lines.scroll_by(page / 2. * n as f32),
            Scroll::Top => self.lines.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) }),
            Scroll::Bottom => self.lines.scroll_to(ListOffset { item_ix: self.lines.item_count(), offset_in_item: px(0.) }),
        }
    }

    fn vim_effect(&mut self, effect: &Effect, cx: &mut Context<Self>) {
        match effect {
            Effect::Scroll(s) => self.scroll(*s),
            Effect::Nav(dir) => self.nav(*dir, cx),
            Effect::Ex(line) => match vim::ex(line) {
                Ex::None => {}
                Ex::Quit => cx.quit(),
                Ex::Window(n) => self.goto_window(n, cx),
                Ex::Buffer(name) => self.goto_buffer(&name, cx),
                Ex::Slash(s) => self.submit(s, cx),
            },
            Effect::Pass | Effect::Edit { .. } | Effect::Submit => {}
        }
        cx.notify();
    }

    fn move_window(&mut self, number: usize, cx: &mut Context<Self>) {
        let Some((n, b)) = self.selected else { return };
        if let Some(from) = self.layout.position_of(&self.store, n, b) {
            self.layout.move_to(from, number - 1);
            self.save_layout();
            cx.notify();
        }
    }

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        if let Applied::Line { network, buffer, index } = self.store.apply(ev)
            && self.selected == Some((network, buffer))
        {
            self.lines.splice(index..index, 1);
            self.store.networks[network].buffers[buffer].unread = 0;
        }
        self.sync_layout();
        cx.notify();
    }

    /// Show a local error line in the current buffer.
    fn local_error(&mut self, n: usize, b: usize, msg: String) {
        let index = self.store.push_local(n, b, LineKind::Error, msg);
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
        match command::parse(&text, &name, target.as_deref()) {
            Ok(Input::Send(cmd)) => {
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
            Ok(Input::Window(WindowAction::Goto(number))) => self.goto_window(number, cx),
            Ok(Input::Window(WindowAction::Move(number))) => self.move_window(number, cx),
            Err(msg) => self.local_error(n, b, msg),
        }
        cx.notify();
    }

    fn line_element(&mut self, ix: usize) -> gpui::AnyElement {
        match self.current().and_then(|(_, buf)| buf.lines.get(ix)) {
            Some(line) => buffer::line(self, line),
            None => div().into_any_element(),
        }
    }
}

impl Render for HircApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette.clone();
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
            .text_size(px(self.font_size))
            .when_some(self.font_family.clone(), |d, f| d.font_family(f))
            .child(sidebar::render(self, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .bg(p.buffer_bg)
                    .when_some(self.banner.clone(), |d, b| d.child(div().px_3().py_1().bg(p.error).text_color(p.background).child(b)))
                    .child(buffer::title(self))
                    .child(list(self.lines.clone(), cx.processor(|this, ix: usize, _window, _cx| this.line_element(ix))).flex_1().w_full())
                    .child(div().px_3().py_2().bg(p.input_bg).border_t_1().border_color(p.border).child(self.input.clone())),
            )
    }
}
