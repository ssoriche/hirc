//! Halloy theme files (`themes/<name>.toml`). Colours are `#RRGGBB` or
//! `#RRGGBBAA`. Missing keys fall back to hirc's built-in dark theme.

use crate::config::ConfigError;
use serde::{Deserialize, Deserializer};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(hex: u32) -> Color {
        Color { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8, a: 0xff }
    }

    pub fn parse(s: &str) -> Option<Color> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        if !hex.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        match hex.len() {
            6 => Some(Color { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: 0xff }),
            8 => Some(Color { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: byte(6)? }),
            _ => None,
        }
    }

    pub fn to_rgba_u32(self) -> u32 {
        u32::from_be_bytes([self.r, self.g, self.b, self.a])
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Color, D::Error> {
        let s = String::deserialize(d)?;
        Color::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("not a colour: {s:?}")))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub general: General,
    pub text: Text,
    pub buffer: BufferColors,
    pub buttons: Buttons,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct General {
    pub background: Color,
    pub border: Color,
    pub horizontal_rule: Color,
    pub unread_indicator: Color,
}

impl Default for General {
    fn default() -> General {
        General {
            background: Color::rgb(0x121218),
            border: Color::rgb(0x5a5a6e),
            horizontal_rule: Color::rgb(0x1f1f29),
            unread_indicator: Color::rgb(0xc792ea),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Text {
    pub primary: Color,
    pub secondary: Color,
    pub tertiary: Color,
    pub success: Color,
    pub error: Color,
}

impl Default for Text {
    fn default() -> Text {
        Text {
            primary: Color::rgb(0xd8dae6),
            secondary: Color::rgb(0x9a9db0),
            tertiary: Color::rgb(0xc792ea),
            success: Color::rgb(0x9ece6a),
            error: Color::rgb(0xf7768e),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct BufferColors {
    pub action: Color,
    pub background: Color,
    pub background_text_input: Color,
    pub background_title_bar: Color,
    pub border: Color,
    pub border_selected: Color,
    pub code: Color,
    pub highlight: Color,
    pub nickname: Color,
    pub selection: Color,
    pub timestamp: Color,
    pub topic: Color,
    pub url: Color,
    pub server_messages: ServerMessages,
}

impl Default for BufferColors {
    fn default() -> BufferColors {
        BufferColors {
            action: Color::rgb(0xff9e64),
            background: Color::rgb(0x16161e),
            background_text_input: Color::rgb(0x1f1f29),
            background_title_bar: Color::rgb(0x1f1f29),
            border: Color::rgb(0x121218),
            border_selected: Color::rgb(0x4a3f66),
            code: Color::rgb(0xb4bdf8),
            highlight: Color::rgb(0x55497a),
            nickname: Color::rgb(0x7dcfff),
            selection: Color::rgb(0x2a3049),
            timestamp: Color::rgb(0x565a70),
            topic: Color::rgb(0x7c8099),
            url: Color::rgb(0x7aa2f7),
            server_messages: ServerMessages::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ServerMessages {
    pub default: Color,
}

impl Default for ServerMessages {
    fn default() -> ServerMessages {
        ServerMessages { default: Color::rgb(0xe0af68) }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Buttons {
    pub primary: Button,
    pub secondary: Button,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Button {
    pub background: Color,
    pub background_hover: Color,
    pub background_selected: Color,
    pub background_selected_hover: Color,
}

impl Default for Button {
    fn default() -> Button {
        Button {
            background: Color { r: 0, g: 0, b: 0, a: 0 },
            background_hover: Color::rgb(0x1b1b26),
            background_selected: Color::rgb(0x23233a),
            background_selected_hover: Color::rgb(0x2a2a44),
        }
    }
}

/// `<config dir>/themes/<name>.toml`.
pub fn load(config_dir: &Path, name: &str) -> Result<Theme, ConfigError> {
    let path = config_dir.join("themes").join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read { path: path.clone(), source })?;
    toml::from_str(&text).map_err(|e| crate::config::parse_error(&path, &text, &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_with_optional_alpha() {
        assert_eq!(Color::parse("#11111B"), Some(Color { r: 0x11, g: 0x11, b: 0x1b, a: 0xff }));
        assert_eq!(Color::parse("#00000000"), Some(Color { r: 0, g: 0, b: 0, a: 0 }));
        assert_eq!(Color::parse("11111b"), Some(Color { r: 0x11, g: 0x11, b: 0x1b, a: 0xff }));
        assert_eq!(Color::parse("#12345"), None);
        assert_eq!(Color::parse("#zzzzzz"), None);
        assert_eq!(Color::parse("#A1B2C3").unwrap().to_rgba_u32(), 0xa1b2c3ff);
    }

    #[test]
    fn halloy_theme_file_parses_and_missing_keys_default() {
        let t: Theme = toml::from_str(
            r##"
[general]
background = "#101010"
unread_indicator = "#AA00AA"

[text]
primary = "#EEEEEE"

[buffer]
nickname = "#00AAAA"
background_text_input = "#202020"

  [buffer.server_messages]
  default = "#AAAA00"

[buttons.primary]
background = "#00000000"
"##,
        )
        .unwrap();
        assert_eq!(t.general.background, Color::parse("#101010").unwrap());
        assert_eq!(t.buffer.server_messages.default, Color::parse("#AAAA00").unwrap());
        assert_eq!(t.buttons.primary.background.a, 0);
        assert_eq!(t.text.error, Theme::default().text.error);
    }

    #[test]
    fn load_finds_themes_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("themes")).unwrap();
        std::fs::write(dir.path().join("themes/Night Sky.toml"), "[text]\nprimary = \"#FFFFFF\"\n").unwrap();
        assert_eq!(load(dir.path(), "Night Sky").unwrap().text.primary, Color::parse("#FFFFFF").unwrap());
        assert!(load(dir.path(), "Missing").is_err());
    }
}
