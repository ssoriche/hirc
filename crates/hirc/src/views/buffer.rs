//! The selected buffer: a title bar (name and topic) and its lines.

use crate::app::HircApp;
use gpui::{AnyElement, IntoElement, div, prelude::*, px};
use hirc_core::store::{Line, LineKind};

pub fn title(app: &HircApp) -> impl IntoElement {
    let p = &app.palette;
    let (name, topic) = match app.current() {
        Some((net, buf)) => (buf.target.name().unwrap_or(&net.name).to_owned(), buf.topic.clone().unwrap_or_default()),
        None => ("No servers configured".to_owned(), String::new()),
    };
    div()
        .flex()
        .gap_3()
        .px_3()
        .py_2()
        .bg(p.title_bg)
        .border_b_1()
        .border_color(p.border)
        .child(div().flex_none().text_color(p.text).child(name))
        .child(div().flex_1().truncate().text_color(p.topic).child(topic))
}

pub fn line(app: &HircApp, line: &Line) -> AnyElement {
    let p = &app.palette;
    let time = line.time.with_timezone(&chrono::Local).format("%H:%M").to_string();
    let nick = line.nick.clone().unwrap_or_default();
    let body = match line.kind {
        LineKind::Privmsg => div()
            .flex()
            .gap_2()
            .flex_1()
            .min_w(px(0.))
            .child(div().flex_none().text_color(p.nickname).child(nick))
            .child(div().flex_1().min_w(px(0.)).text_color(p.text).child(line.text.clone())),
        LineKind::Action => div().flex_1().min_w(px(0.)).text_color(p.action).child(format!("* {nick} {}", line.text)),
        LineKind::Notice => div().flex_1().min_w(px(0.)).text_color(p.text_dim).child(format!("-{nick}- {}", line.text)),
        LineKind::Error => div().flex_1().min_w(px(0.)).text_color(p.error).child(line.text.clone()),
        _ => div().flex_1().min_w(px(0.)).text_color(p.server).child(line.text.clone()),
    };
    div().flex().gap_2().px_3().py(px(1.)).child(div().flex_none().text_color(p.timestamp).child(time)).child(body).into_any_element()
}
