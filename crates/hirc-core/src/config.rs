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
    /// Lines per history page (default 100), never more than the server's
    /// CHATHISTORY limit. hirc's key; Halloy ignores it.
    pub history_page_size: Option<usize>,
    pub sasl: Option<Sasl>,
    /// Relay bots (a Discord bridge, say) whose `<name> text` lines read
    /// as `name`'s own. hirc's key; Halloy ignores it.
    pub bridges: Vec<String>,
    /// This client's name to IRCCloud's bouncer, put into its `bnc:`
    /// password so it replays what this client missed (`irccloud`).
    /// hirc's key; Halloy ignores it.
    #[serde(deserialize_with = "client_id")]
    pub bouncer_client_id: Option<String>,
    /// Seconds without a word from the server before hirc pings it
    /// (Halloy's key; hirc's default is 120).
    pub ping_time: Option<u64>,
    /// Seconds to wait after that ping before the connection counts as
    /// lost and is retried (Halloy's key; hirc's default is 180).
    pub ping_timeout: Option<u64>,
}

/// A `bouncer_client_id` IRCCloud can read back out of the password.
fn client_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let id = String::deserialize(d)?;
    if crate::irccloud::valid_id(&id) { Ok(Some(id)) } else { Err(serde::de::Error::custom("bouncer_client_id must not be empty, or hold spaces, `:` or `@`")) }
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
            history_page_size: None,
            sasl: None,
            bridges: Vec::new(),
            bouncer_client_id: None,
            ping_time: None,
            ping_timeout: None,
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
    #[error("Can't read {}: {source}", path.display())]
    Read { path: PathBuf, source: std::io::Error },
    /// No source snippet: toml would print the offending line, which may
    /// be a password.
    #[error("Error in {}: {message} (line {line}, column {col})", path.display())]
    Parse { path: PathBuf, message: String, line: usize, col: usize },
    #[error("password command `{}` failed: {}", program(.command), clip(.message))]
    Command { command: String, message: String },
}

impl ConfigError {
    /// The file is not there at all: on a first run that is normal, and
    /// the defaults stand in.
    pub fn is_missing(&self) -> bool {
        matches!(self, ConfigError::Read { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

/// `text` with every path under `home` written from `~`, so messages stay
/// short and do not spell out the user's home directory.
pub fn tilde(text: &str, home: Option<&Path>) -> String {
    match home.and_then(Path::to_str).filter(|h| h.len() > 1) {
        Some(h) => text.replace(&format!("{}/", h.trim_end_matches('/')), "~/"),
        None => text.to_owned(),
    }
}

/// `tilde` with the user's home directory.
pub fn tilde_home(text: &str) -> String {
    tilde(text, dirs::home_dir().as_deref())
}

/// Only the program name: the rest of the command line may hold a secret.
fn program(command: &str) -> &str {
    command.split_whitespace().find(|w| !w.contains(['=', '"', '\''])).unwrap_or("command")
}

/// toml quotes the rejected value in type errors (``invalid type: integer
/// `123`, expected a string``), and that value may be a password, so blank
/// every quoted span before `expected`. What follows `expected` is toml's
/// grammar (`` `]` ``, `a string`), never the file's text, and it is what
/// the user needs to fix the file, so it stays.
fn redact(message: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    for (i, c) in message.char_indices() {
        if matches!(c, '`' | '"' | '\'') {
            if quoted {
                out.push('…');
            }
            out.push('`');
            quoted = !quoted;
        } else if !quoted {
            out.push(c);
            // Only an unquoted `expected`: a quoted value may contain the word.
            if out.ends_with("expected") {
                out.push_str(&message[i + c.len_utf8()..]);
                return out;
            }
        }
    }
    if quoted {
        out.push('…');
    }
    out
}

/// A parse error with a line and column but never the source text.
pub(crate) fn parse_error(path: &Path, text: &str, e: &toml::de::Error) -> ConfigError {
    let start = e.span().map_or(0, |s| s.start).min(text.len());
    let before = &text.as_bytes()[..start];
    let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
    let col = start - before.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) + 1;
    ConfigError::Parse { path: path.to_owned(), message: redact(e.message()), line, col }
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
    fn a_bouncer_client_id_is_checked_as_it_loads() {
        let c: Config = toml::from_str("[servers.x]\nnickname='n'\nserver='h'\nbouncer_client_id='laptop'\n[servers.y]\nnickname='n'\nserver='h'\n").unwrap();
        assert_eq!(c.servers["x"].bouncer_client_id.as_deref(), Some("laptop"));
        assert_eq!(c.servers["y"].bouncer_client_id, None);
        let e = toml::from_str::<Config>("[servers.x]\nnickname='n'\nserver='h'\nbouncer_client_id='my laptop'\n").unwrap_err();
        assert!(e.to_string().contains("bouncer_client_id must not"), "{e}");
    }

    #[test]
    fn bridges_are_listed_per_server_and_default_to_none() {
        let c: Config = toml::from_str("[servers.x]\nnickname='n'\nserver='h'\nbridges=['IrcBot']\n[servers.y]\nnickname='n'\nserver='h'\n").unwrap();
        assert_eq!(c.servers["x"].bridges, ["IrcBot"]);
        assert!(c.servers["y"].bridges.is_empty());
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

    #[test]
    fn type_errors_hide_the_rejected_value() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        for body in ["password = 918273", "port = \"hunter2\"", "port = 'hunter2'"] {
            std::fs::write(&p, format!("[servers.x]\nserver = \"h\"\n{body}")).unwrap();
            let err = load(&p).unwrap_err().to_string();
            assert!(!err.contains("918273") && !err.contains("hunter2"), "{err}");
        }
    }

    #[test]
    fn program_skips_quoted_env_values() {
        assert_eq!(program("FOO=\"a b\" op read x"), "op");
    }

    #[test]
    fn redaction_keeps_what_toml_expected() {
        assert_eq!(redact("invalid type: string \"hunter2\", expected a boolean"), "invalid type: string `…`, expected a boolean");
        assert_eq!(redact("unclosed table, expected `]`"), "unclosed table, expected `]`");
        assert_eq!(redact("invalid type: integer `123`"), "invalid type: integer `…`");
        // The word inside a quoted value does not end the redaction.
        assert_eq!(redact("invalid type: string \"unexpected!\", expected a map"), "invalid type: string `…`, expected a map");
        assert_eq!(redact("bad `secret"), "bad `…");
    }

    #[test]
    fn home_paths_shorten_to_tilde() {
        let home = Path::new("/Users/me");
        assert_eq!(tilde("reading /Users/me/.config/hirc/config.toml: x", Some(home)), "reading ~/.config/hirc/config.toml: x");
        assert_eq!(tilde("/Users/meg/x", Some(home)), "/Users/meg/x");
        assert_eq!(tilde("/tmp/x", None), "/tmp/x");
        assert_eq!(tilde("/x", Some(Path::new("/"))), "/x");
    }

    #[test]
    fn a_missing_file_is_told_apart() {
        let missing = load(Path::new("/nonexistent/hirc/config.toml")).unwrap_err();
        assert!(missing.is_missing());
        let parse = ConfigError::Parse { path: "x".into(), message: String::new(), line: 1, col: 1 };
        assert!(!parse.is_missing());
    }

    #[test]
    fn history_page_size_is_optional_per_server() {
        let c: Config =
            toml::from_str("[servers.a]\nnickname='n'\nserver='s'\nhistory_page_size = 250\n[servers.b]\nnickname='n'\nserver='s'\nchathistory = false\n")
                .unwrap();
        assert_eq!(c.servers["a"].history_page_size, Some(250));
        assert!(c.servers["a"].chathistory);
        assert_eq!(c.servers["b"].history_page_size, None);
        assert!(!c.servers["b"].chathistory);
    }
}
