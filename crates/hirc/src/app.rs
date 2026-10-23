//! App state and the event loop. The network thread sends `Event`s; we
//! apply them to the `Store` and redraw. Input goes back as `Command`s.

use crate::input::{Submit, TextInput};
use crate::theme::{Palette, palette};
use crate::views::{buffer, sidebar};
use gpui::{Context, Entity, Focusable, ListAlignment, ListState, SharedString, Window, actions, div, list, prelude::*, px};
use hirc_core::command::{self, Input};
use hirc_core::event::{Command, Event, Target};
use hirc_core::store::{Applied, Buffer, LineKind, Network, Store};
use hirc_core::theme::Theme;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

actions!(hirc, [Quit]);

pub struct Init {
    pub networks: Vec<String>,
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
        let store = Store::new(init.networks);
        let selected = (!store.networks.is_empty()).then_some((0, 0));
        let input = cx.new(|cx| TextInput::new("Message", cx));
        cx.subscribe(&input, |this, _, ev: &Submit, cx| this.submit(ev.0.clone(), cx)).detach();
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
            palette: palette(&init.theme),
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

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        if let Applied::Line { network, buffer, index } = self.store.apply(ev)
            && self.selected == Some((network, buffer))
        {
            self.lines.splice(index..index, 1);
            self.store.networks[network].buffers[buffer].unread = 0;
        }
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
        // `/me` with no text would send an empty action; refuse it here.
        let t = text.trim();
        if t == "/me" {
            self.local_error(n, b, "Usage: /me <text>".to_owned());
            cx.notify();
            return;
        }
        match command::parse(&text, &name, target.as_deref()) {
            Ok(Input::Send(cmd)) => {
                let _ = self.commands.send(cmd);
            }
            Ok(Input::OpenQuery { nick, text }) => {
                let q = self.store.buffer_ix(n, &Target::Query(nick.clone()));
                self.select(n, q, cx);
                if let Some(text) = text {
                    let _ = self.commands.send(Command::Say { network: name, target: nick, text });
                }
            }
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
        div()
            .size_full()
            .flex()
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
