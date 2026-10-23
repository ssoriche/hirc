//! The Halloy theme as gpui colours.

use gpui::{Hsla, rgba};
use hirc_core::theme::{Color, Theme};

#[derive(Clone)]
pub struct Palette {
    pub background: Hsla,
    pub border: Hsla,
    pub unread: Hsla,
    pub text: Hsla,
    pub text_dim: Hsla,
    pub success: Hsla,
    pub error: Hsla,
    pub buffer_bg: Hsla,
    pub input_bg: Hsla,
    pub title_bg: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub nickname: Hsla,
    pub timestamp: Hsla,
    pub action: Hsla,
    pub server: Hsla,
    pub topic: Hsla,
}

fn c(c: Color) -> Hsla {
    rgba(c.to_rgba_u32()).into()
}

pub fn palette(t: &Theme) -> Palette {
    Palette {
        background: c(t.general.background),
        border: c(t.general.border),
        unread: c(t.general.unread_indicator),
        text: c(t.text.primary),
        text_dim: c(t.text.secondary),
        success: c(t.text.success),
        error: c(t.text.error),
        buffer_bg: c(t.buffer.background),
        input_bg: c(t.buffer.background_text_input),
        title_bg: c(t.buffer.background_title_bar),
        hover: c(t.buttons.primary.background_hover),
        selected: c(t.buttons.primary.background_selected),
        nickname: c(t.buffer.nickname),
        timestamp: c(t.buffer.timestamp),
        action: c(t.buffer.action),
        server: c(t.buffer.server_messages.default),
        topic: c(t.buffer.topic),
    }
}
