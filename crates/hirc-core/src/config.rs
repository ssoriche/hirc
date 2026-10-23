//! The config file, in Halloy's format so an existing Halloy config works
//! unchanged:
//!
//! ```toml
//! theme = "Catppuccin Mocha"
//! [servers.home]
//! nickname = "me"
//! server = "irc.example.org"
//! [servers.home.sasl.plain]
//! username = "me"
//! password_command = "op read op://vault/irc/password"
//! ```
//!
//! Keys hirc doesn't use yet are ignored. It lives in `~/.config/hirc/`
//! (or `$XDG_CONFIG_HOME/hirc/`) next to a `themes/` folder.

use indexmap::IndexMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// A file name in `themes/`, without `.toml`.
    pub theme: Option<String>,
    pub font: Font,
    /// In file order, which is the order the sidebar shows them.
    pub servers: IndexMap<String, Server>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Font {
    pub family: Option<String>,
    pub size: Option<f32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Server {
    pub nickname: String,
    /// Tried in order when the nickname is taken.
    pub alt_nicks: Vec<String>,
    pub username: Option<String>,
    pub realname: Option<String>,
    pub server: String,
    pub port: Option<u16>,
    /// The server password (PASS), e.g. for a bouncer.
    pub password: Option<String>,
    pub password_file: Option<PathBuf>,
    /// Run with `sh -c`; stdout is the password.
    pub password_command: Option<String>,
    pub use_tls: bool,
    /// Extra CA to trust (PEM), for self-signed servers.
    pub root_cert_path: Option<PathBuf>,
    pub channels: Vec<String>,
    pub chathistory: bool,
    pub sasl: Option<Sasl>,
}

impl Default for Server {
    fn default() -> Server {
        Server {
            nickname: String::new(),
            alt_nicks: Vec::new(),
            username: None,
            realname: None,
            server: String::new(),
            port: None,
            password: None,
            password_file: None,
            password_command: None,
            use_tls: true,
            root_cert_path: None,
            channels: Vec::new(),
            chathistory: true,
            sasl: None,
        }
    }
}

impl Server {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(if self.use_tls { 6697 } else { 6667 })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sasl {
    Plain { username: String, password: Option<String>, password_file: Option<PathBuf>, password_command: Option<String> },
    External { cert: PathBuf, key: Option<PathBuf> },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {}: {source}", path.display())]
    Read { path: PathBuf, source: std::io::Error },
    /// No source snippet: toml would print the offending line, which may
    /// be a password.
    #[error("parsing {}: {message} (line {line}, column {col})", path.display())]
    Parse { path: PathBuf, message: String, line: usize, col: usize },
    #[error("password command `{}` failed: {}", program(.command), clip(.message))]
    Command { command: String, message: String },
}

/// Only the program name: the rest of the command line may hold a secret.
fn program(command: &str) -> &str {
    command.split_whitespace().find(|w| !w.contains('=')).unwrap_or("command")
}

/// A parse error with a line and column but never the source text.
pub(crate) fn parse_error(path: &Path, text: &str, e: &toml::de::Error) -> ConfigError {
    let start = e.span().map_or(0, |s| s.start).min(text.len());
    let before = &text.as_bytes()[..start];
    let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
    let col = start - before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) + 1;
    ConfigError::Parse { path: path.to_owned(), message: e.message().to_owned(), line, col }
}

/// The command's stderr could echo a secret, so cap what we show.
fn clip(message: &str) -> String {
    message.chars().take(200).collect()
}

/// `$XDG_CONFIG_HOME/hirc`, else `~/.config/hirc`. Not `~/Library/...`:
/// the config is managed with dotfiles like everything else in ~/.config.
pub fn dir() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("hirc"),
        None => dirs::home_dir().unwrap_or_default().join(".config").join("hirc"),
    }
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read { path: path.to_owned(), source })?;
    toml::from_str(&text).map_err(|e| parse_error(path, &text, &e))
}

/// A password from, in order: the value itself, a file, or a command's
/// stdout. Trailing newlines are dropped. Blocks while the command runs.
pub fn secret(value: Option<&str>, file: Option<&Path>, command: Option<&str>) -> Result<Option<String>, ConfigError> {
    if let Some(v) = value {
        return Ok(Some(v.to_owned()));
    }
    if let Some(f) = file {
        let s = std::fs::read_to_string(f).map_err(|source| ConfigError::Read { path: f.to_owned(), source })?;
        return Ok(Some(s.trim_end_matches(['\r', '\n']).to_owned()));
    }
    let Some(cmd) = command else {
        return Ok(None);
    };
    let fail = |message: String| ConfigError::Command { command: cmd.to_owned(), message };
    let out = std::process::Command::new("sh").arg("-c").arg(cmd).stdin(std::process::Stdio::null()).output().map_err(|e| fail(e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(fail(if stderr.is_empty() { out.status.to_string() } else { stderr }));
    }
    let stdout = String::from_utf8(out.stdout).map_err(|_| fail("output is not valid UTF-8".to_owned()))?;
    Ok(Some(stdout.trim_end_matches(['\r', '\n']).to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HALLOY: &str = r##"
theme = "Night"

[font]
family = "Iosevka"
size = 13
bold = true

[buffer.server_messages.join]
enabled = false

[buffer.mark_as_read]
on_scroll_to_bottom = true

[servers.libera]
nickname = "tester"
server = "irc.example.net"
password_command = "printf 's3cret\n'"
chathistory = true

[servers.home]
nickname = "tester"
server = "irc.example.org"
port = 6697
channels = ["#one", "#two"]

[servers.home.sasl.plain]
username = "tester"
password_command = "printf hunter22"
"##;

    #[test]
    fn parses_halloy_shape_and_ignores_unknown_keys() {
        let c: Config = toml::from_str(HALLOY).unwrap();
        assert_eq!(c.theme.as_deref(), Some("Night"));
        assert_eq!(c.font.family.as_deref(), Some("Iosevka"));
        assert_eq!(c.font.size, Some(13.0));
        // File order is kept so the sidebar matches the config.
        assert_eq!(c.servers.keys().collect::<Vec<_>>(), ["libera", "home"]);
        let libera = &c.servers["libera"];
        assert!(libera.use_tls);
        assert_eq!(libera.port(), 6697);
        assert!(libera.chathistory);
        let home = &c.servers["home"];
        assert_eq!(home.channels, ["#one", "#two"]);
        match &home.sasl {
            Some(Sasl::Plain { username, password_command, .. }) => {
                assert_eq!(username, "tester");
                assert_eq!(password_command.as_deref(), Some("printf hunter22"));
            }
            other => panic!("expected sasl.plain, got {other:?}"),
        }
    }

    #[test]
    fn secret_prefers_value_then_file_then_command() {
        assert_eq!(secret(Some("v"), None, Some("false")).unwrap().as_deref(), Some("v"));
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("pw");
        std::fs::write(&f, "fromfile\n").unwrap();
        assert_eq!(secret(None, Some(&f), None).unwrap().as_deref(), Some("fromfile"));
        assert_eq!(secret(None, None, Some("printf 's3cret\\n'")).unwrap().as_deref(), Some("s3cret"));
        assert_eq!(secret(None, None, None).unwrap(), None);
    }

    #[test]
    fn failing_command_is_an_error() {
        let err = secret(None, None, Some("echo nope >&2; exit 3")).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn error_display_hides_command_arguments() {
        let err = secret(None, None, Some("printf hunter22 >&2; exit 1")).unwrap_err();
        let shown = err.to_string();
        assert!(!shown.contains("hunter22 >&2"), "{shown}");
        assert!(shown.contains("printf"), "{shown}");
        let err = secret(None, None, Some("printf hunter22; exit 1")).unwrap_err();
        assert!(!err.to_string().contains("hunter22"), "{err}");
    }

    #[test]
    fn non_utf8_output_is_an_error() {
        let err = secret(None, None, Some("printf '\\377'")).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn plaintext_default_port() {
        let c: Config = toml::from_str("[servers.x]\nnickname='n'\nserver='localhost'\nuse_tls=false\n").unwrap();
        assert_eq!(c.servers["x"].port(), 6667);
    }

    #[test]
    fn load_reports_path_on_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "servers = 3").unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("config.toml"), "{err}");
    }

    #[test]
    fn parse_error_hides_secret_and_shows_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "[servers.x]\npassword = \"hunter2").unwrap();
        let err = load(&p).unwrap_err().to_string();
        assert!(!err.contains("hunter2"), "{err}");
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn error_display_skips_env_assignments() {
        let err = secret(None, None, Some("OP_SESSION=abc op read x; exit 1")).unwrap_err();
        let shown = err.to_string();
        assert!(!shown.contains("abc"), "{shown}");
        assert!(shown.contains("`op`"), "{shown}");
    }
}
