//! The react picker: its state as the user types (`Picker`), and the
//! reactions they use most, for its quick row (`Recent`), kept in
//! `$XDG_STATE_HOME/hirc/reactions.toml`, else
//! `~/.local/state/hirc/reactions.toml`. Emoji and counts only.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The quick row before anything is used, and the order ties keep.
pub const SEED: [&str; 8] = ["👍", "❤️", "😂", "🎉", "👀", "😮", "🙏", "✅"];

/// How many rows a search shows.
const ROWS: usize = 8;

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"));
    base.join("hirc/reactions.toml")
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    #[serde(default)]
    pub counts: BTreeMap<String, u32>,
}

impl Recent {
    /// What `path` holds; nothing (a fresh start) when it is missing or
    /// can't be read.
    pub fn load(path: &Path) -> Recent {
        std::fs::read_to_string(path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string(self).map_err(std::io::Error::other)?)
    }

    pub fn used(&mut self, emoji: &str) {
        *self.counts.entry(emoji.to_owned()).or_default() += 1;
    }

    /// The `n` most used, the seed filling in; ties keep the seed's order,
    /// then the emoji's own.
    pub fn top(&self, n: usize) -> Vec<String> {
        let rank = |e: &str| SEED.iter().position(|s| *s == e).unwrap_or(SEED.len());
        let count = |e: &str| self.counts.get(e).copied().unwrap_or(0);
        let mut all: Vec<&str> = SEED.iter().copied().chain(self.counts.keys().map(String::as_str).filter(|e| !SEED.contains(e))).collect();
        all.sort_by(|a, b| count(b).cmp(&count(a)).then(rank(a).cmp(&rank(b))).then(a.cmp(b)));
        all.into_iter().take(n).map(str::to_owned).collect()
    }
}

/// A key the picker takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickKey {
    Char(char),
    Backspace,
    Next,
    Prev,
    Enter,
    Escape,
}

/// What a key did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Picked {
    Stay,
    Close,
    Emoji(String),
}

/// The open picker: the quick row, and a shortcode search under it.
/// With the search empty, the selection is on the quick row and digits
/// 1 to 8 pick from it; once typing starts, digits are typed (`:100:`)
/// and the selection is on the results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picker {
    pub quick: Vec<String>,
    pub query: String,
    pub rows: Vec<(&'static str, &'static str)>,
    pub selected: usize,
}

impl Picker {
    pub fn new(quick: Vec<String>) -> Picker {
        Picker { quick, query: String::new(), rows: Vec::new(), selected: 0 }
    }

    fn search(&mut self) {
        let name = self.query.trim_matches(':');
        self.rows = if name.is_empty() { Vec::new() } else { crate::emoji::search(name, ROWS) };
        self.selected = 0;
    }

    /// How many there are to choose from now.
    fn choices(&self) -> usize {
        if self.query.is_empty() { self.quick.len() } else { self.rows.len() }
    }

    pub fn key(&mut self, key: PickKey) -> Picked {
        match key {
            PickKey::Char(c @ '1'..='8') if self.query.is_empty() => {
                self.quick.get(c as usize - '1' as usize).map_or(Picked::Stay, |e| Picked::Emoji(e.clone()))
            }
            PickKey::Char(c) => {
                self.query.push(c);
                self.search();
                Picked::Stay
            }
            PickKey::Backspace => {
                self.query.pop();
                self.search();
                Picked::Stay
            }
            PickKey::Next | PickKey::Prev => {
                let n = self.choices();
                if n > 0 {
                    self.selected = if key == PickKey::Next { (self.selected + 1) % n } else { (self.selected + n - 1) % n };
                }
                Picked::Stay
            }
            PickKey::Enter if self.query.is_empty() => self.quick.get(self.selected).map_or(Picked::Stay, |e| Picked::Emoji(e.clone())),
            PickKey::Enter => self.rows.get(self.selected).map_or(Picked::Stay, |(e, _)| Picked::Emoji((*e).to_owned())),
            PickKey::Escape => Picked::Close,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quick_row_starts_as_the_seed_and_follows_use() {
        let mut r = Recent::default();
        assert_eq!(r.top(8), SEED.map(str::to_owned));
        r.used("🚀");
        r.used("🚀");
        r.used("👀");
        assert_eq!(r.top(3), ["🚀", "👀", "👍"]);
    }

    #[test]
    fn a_bad_file_is_a_fresh_start_and_a_saved_one_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reactions.toml");
        std::fs::write(&path, "not = [toml").unwrap();
        assert_eq!(Recent::load(&path), Recent::default());
        let mut r = Recent::default();
        r.used("🚀");
        r.save(&path).unwrap();
        assert_eq!(Recent::load(&path), r);
    }

    #[test]
    fn digits_pick_from_the_quick_row_until_a_search_starts() {
        let mut p = Picker::new(SEED.map(str::to_owned).to_vec());
        assert_eq!(p.key(PickKey::Char('4')), Picked::Emoji("🎉".into()));
        let mut p = Picker::new(SEED.map(str::to_owned).to_vec());
        for c in ":100".chars() {
            assert_eq!(p.key(PickKey::Char(c)), Picked::Stay);
        }
        assert_eq!(p.query, ":100");
        assert_eq!(p.key(PickKey::Enter), Picked::Emoji("💯".into()));
    }

    #[test]
    fn arrows_move_over_the_row_then_the_results() {
        let mut p = Picker::new(SEED.map(str::to_owned).to_vec());
        p.key(PickKey::Next);
        p.key(PickKey::Next);
        assert_eq!(p.key(PickKey::Enter), Picked::Emoji("😂".into()));
        let mut p = Picker::new(SEED.map(str::to_owned).to_vec());
        for c in "party".chars() {
            p.key(PickKey::Char(c));
        }
        assert!(p.rows.len() > 1);
        p.key(PickKey::Prev);
        assert_eq!(p.selected, p.rows.len() - 1, "wraps");
        p.key(PickKey::Backspace);
        assert_eq!((p.query.as_str(), p.selected), ("part", 0));
        assert_eq!(p.key(PickKey::Escape), Picked::Close);
    }
}
