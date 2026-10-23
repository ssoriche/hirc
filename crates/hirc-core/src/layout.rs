//! The window list: one numbered, user-ordered list of buffers across every
//! network, irssi-style, so the sidebar isn't forced into a server tree.
//! Saved under the state dir (not the config dir, which is chezmoi-managed).

use crate::store::Store;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One window. `name: None` is the network's server buffer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub network: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Key {
    fn matches(&self, network: &str, name: Option<&str>) -> bool {
        self.network == network
            && match (&self.name, name) {
                (None, None) => true,
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => false,
            }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default, rename = "window")]
    pub windows: Vec<Key>,
}

/// `$XDG_STATE_HOME/hirc/layout.toml`, else `~/.local/state/hirc/layout.toml`.
pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"));
    base.join("hirc/layout.toml")
}

impl Layout {
    /// First run: each server buffer followed by its configured channels, in config order.
    pub fn seed(servers: &[(String, Vec<String>)]) -> Layout {
        let mut windows = Vec::new();
        for (net, channels) in servers {
            windows.push(Key { network: net.clone(), name: None });
            for c in channels {
                let chan = c.split_whitespace().next().unwrap_or_default();
                if !chan.is_empty() {
                    windows.push(Key { network: net.clone(), name: Some(chan.to_owned()) });
                }
            }
        }
        Layout { windows }
    }

    /// Forget windows of networks that are no longer configured. True if anything changed.
    pub fn prune(&mut self, networks: &[String]) -> bool {
        let before = self.windows.len();
        self.windows.retain(|k| networks.contains(&k.network));
        self.windows.len() != before
    }

    /// Append any store buffer that has no window yet. True if anything changed.
    pub fn sync(&mut self, store: &Store) -> bool {
        let mut changed = false;
        for net in &store.networks {
            for buf in &net.buffers {
                let name = buf.target.name();
                if !self.windows.iter().any(|k| k.matches(&net.name, name)) {
                    self.windows.push(Key { network: net.name.clone(), name: name.map(str::to_owned) });
                    changed = true;
                }
            }
        }
        changed
    }

    /// Window position (0-based) → store indexes, if that buffer exists.
    pub fn resolve(&self, store: &Store, pos: usize) -> Option<(usize, usize)> {
        let k = self.windows.get(pos)?;
        let n = store.networks.iter().position(|net| net.name == k.network)?;
        let b = store.networks[n].buffers.iter().position(|buf| k.matches(&k.network, buf.target.name()))?;
        Some((n, b))
    }

    /// Store indexes → window position (0-based).
    pub fn position_of(&self, store: &Store, n: usize, b: usize) -> Option<usize> {
        let net = store.networks.get(n)?;
        let name = net.buffers.get(b)?.target.name();
        self.windows.iter().position(|k| k.matches(&net.name, name))
    }

    /// Move the window at `from` so it sits at `to` (clamped); others shift. 0-based.
    pub fn move_to(&mut self, from: usize, to: usize) {
        if from >= self.windows.len() {
            return;
        }
        let k = self.windows.remove(from);
        let to = to.min(self.windows.len());
        self.windows.insert(to, k);
    }

    /// `None` when the file is missing or unreadable; the caller seeds a fresh one.
    pub fn load(path: &Path) -> Option<Layout> {
        let text = std::fs::read_to_string(path).ok()?;
        toml::from_str(&text).ok()
    }

    /// Write via a temp file and rename, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Target;
    use crate::store::Store;

    fn key(n: &str, name: Option<&str>) -> Key {
        Key { network: n.into(), name: name.map(Into::into) }
    }

    #[test]
    fn seed_lists_each_server_then_its_channels() {
        let l = Layout::seed(&[("a".into(), vec!["#x".into(), "#y key".into()]), ("b".into(), vec![])]);
        assert_eq!(l.windows, [key("a", None), key("a", Some("#x")), key("a", Some("#y")), key("b", None)]);
    }

    #[test]
    fn sync_appends_new_buffers_once() {
        let mut store = Store::new(["a".to_owned()]);
        let mut l = Layout::seed(&[("a".into(), vec![])]);
        let q = store.buffer_ix(0, &Target::Query("Bob".into()));
        assert!(l.sync(&store));
        assert!(!l.sync(&store), "second sync changes nothing");
        assert_eq!(l.windows.last(), Some(&key("a", Some("Bob"))));
        assert_eq!(l.position_of(&store, 0, q), Some(1));
        assert_eq!(l.resolve(&store, 1), Some((0, q)));
    }

    #[test]
    fn names_match_case_insensitively() {
        let mut store = Store::new(["a".to_owned()]);
        let mut l = Layout { windows: vec![key("a", None), key("a", Some("#Rust"))] };
        let b = store.buffer_ix(0, &Target::Channel("#rust".into()));
        assert!(!l.sync(&store));
        assert_eq!(l.position_of(&store, 0, b), Some(1));
    }

    #[test]
    fn move_to_inserts_at_the_new_position() {
        let mut l = Layout { windows: vec![key("a", None), key("a", Some("#1")), key("a", Some("#2")), key("a", Some("#3"))] };
        l.move_to(3, 1);
        assert_eq!(l.windows[1], key("a", Some("#3")));
        assert_eq!(l.windows[3], key("a", Some("#2")));
        l.move_to(1, 99); // clamps to the end
        assert_eq!(l.windows.last(), Some(&key("a", Some("#3"))));
        l.move_to(42, 0); // out of range: no-op
        assert_eq!(l.windows.len(), 4);
    }

    #[test]
    fn prune_drops_networks_no_longer_configured() {
        let mut l = Layout { windows: vec![key("a", None), key("gone", None), key("a", Some("#x"))] };
        assert!(l.prune(&["a".to_owned()]));
        assert_eq!(l.windows, [key("a", None), key("a", Some("#x"))]);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/layout.toml");
        let l = Layout { windows: vec![key("a", None), key("a", Some("#x"))] };
        l.save(&path).unwrap();
        assert_eq!(Layout::load(&path), Some(l));
        assert_eq!(Layout::load(&dir.path().join("missing.toml")), None);
        std::fs::write(dir.path().join("bad.toml"), "not = [valid").unwrap();
        assert_eq!(Layout::load(&dir.path().join("bad.toml")), None);
    }
}
