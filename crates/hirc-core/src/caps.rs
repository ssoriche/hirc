//! CAP negotiation (CAP LS 302, cap-notify). We only request caps hirc
//! implements. Draft caps get renamed when they're ratified, so each wanted
//! cap is a group of spellings and either one counts.

use std::collections::{BTreeMap, BTreeSet};

pub type CapGroups = &'static [&'static [&'static str]];

/// What hirc implements so far, in request order. Later milestones add
/// groups here, e.g. `&["draft/chathistory", "chathistory"]`.
pub const WANTED: CapGroups = &[&["message-tags"], &["server-time"], &["echo-message"], &["cap-notify"], &["sasl"], &["batch"]];

#[derive(Debug)]
pub struct Caps {
    wanted: CapGroups,
    /// Advertised by the server: name -> value (`sasl=PLAIN,EXTERNAL`).
    available: BTreeMap<String, Option<String>>,
    enabled: BTreeSet<String>,
    requested: BTreeSet<String>,
}

impl Default for Caps {
    fn default() -> Caps {
        Caps::new(WANTED)
    }
}

/// `"a b=1 c"` -> `[("a", None), ("b", Some("1")), ("c", None)]`.
pub fn parse_list(list: &str) -> impl Iterator<Item = (&str, Option<&str>)> {
    list.split_whitespace().map(|c| match c.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (c, None),
    })
}

impl Caps {
    pub fn new(wanted: CapGroups) -> Caps {
        Caps { wanted, available: BTreeMap::new(), enabled: BTreeSet::new(), requested: BTreeSet::new() }
    }

    /// Forget everything, for a new connection.
    pub fn reset(&mut self) {
        *self = Caps::new(self.wanted);
    }

    /// The params of `CAP <nick> LS [*] :<caps>`. True on the last line.
    pub fn on_ls(&mut self, params: &[String]) -> bool {
        let more = params.len() >= 4 && params[2] == "*";
        if let Some(list) = params.last() {
            self.on_new(list);
        }
        !more
    }

    pub fn on_new(&mut self, list: &str) {
        for (k, v) in parse_list(list) {
            self.available.insert(k.to_owned(), v.map(str::to_owned));
        }
    }

    pub fn on_del(&mut self, list: &str) {
        for (k, _) in parse_list(list) {
            self.available.remove(k);
            self.enabled.remove(k);
        }
    }

    pub fn on_ack(&mut self, list: &str) {
        for (k, _) in parse_list(list) {
            match k.strip_prefix('-') {
                Some(off) => {
                    self.enabled.remove(off);
                    self.requested.remove(off);
                }
                None => {
                    self.enabled.insert(k.to_owned());
                    self.requested.remove(k);
                }
            }
        }
    }

    pub fn on_nak(&mut self, list: &str) {
        for (k, _) in parse_list(list) {
            self.requested.remove(k);
        }
    }

    /// Payloads for `CAP REQ`: the first available spelling of each wanted
    /// cap that isn't enabled or already asked for. Each payload fits on
    /// one line.
    pub fn to_request(&mut self) -> Vec<String> {
        let mut want = Vec::new();
        for group in self.wanted {
            if group.iter().any(|c| self.enabled.contains(*c) || self.requested.contains(*c)) {
                continue;
            }
            if let Some(c) = group.iter().find(|c| self.available.contains_key(**c)) {
                want.push(*c);
            }
        }
        let mut out: Vec<String> = Vec::new();
        for c in want {
            self.requested.insert(c.to_owned());
            match out.last_mut() {
                Some(line) if line.len() + 1 + c.len() <= 400 => {
                    line.push(' ');
                    line.push_str(c);
                }
                _ => out.push(c.to_owned()),
            }
        }
        out
    }

    fn group(&self, name: &str) -> Option<&'static [&'static str]> {
        self.wanted.iter().copied().find(|g| g.contains(&name))
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        match self.group(name) {
            Some(g) => g.iter().any(|c| self.enabled.contains(*c)),
            None => self.enabled.contains(name),
        }
    }

    /// The advertised value, e.g. `"PLAIN,EXTERNAL"` for `sasl`.
    pub fn value(&self, name: &str) -> Option<&str> {
        let names = self.group(name).unwrap_or(std::slice::from_ref(&name));
        names.iter().find_map(|c| self.available.get(*c)).and_then(|v| v.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Message;

    fn ls(caps: &mut Caps, line: &str) -> bool {
        caps.on_ls(&Message::parse(line).unwrap().params)
    }

    #[test]
    fn golden_ergo_ls_requests_every_wanted_cap() {
        let golden = include_str!("../tests/data/ergo-2.19.1-cap-ls.txt");
        let mut caps = Caps::default();
        let done: Vec<bool> = golden.lines().map(|l| ls(&mut caps, l)).collect();
        assert_eq!(done, [false, true]);
        assert_eq!(caps.value("sasl"), Some("PLAIN,EXTERNAL"));
        let req = caps.to_request();
        assert_eq!(req, ["message-tags server-time echo-message cap-notify sasl batch"]);
        // Out-of-scope caps Ergo advertises are never requested.
        assert!(!req[0].contains("ergo.chat/nope") && !req[0].contains("znc.in/playback"));
        // Already requested: a second call asks for nothing.
        assert!(caps.to_request().is_empty());
    }

    #[test]
    fn accepts_either_spelling_preferring_the_first_listed() {
        const GROUPS: CapGroups = &[&["draft/chathistory", "chathistory"]];
        let mut caps = Caps::new(GROUPS);
        ls(&mut caps, ":srv CAP * LS :chathistory");
        assert_eq!(caps.to_request(), ["chathistory"]);
        caps.on_ack("chathistory");
        assert!(caps.is_enabled("draft/chathistory"));
        assert!(caps.is_enabled("chathistory"));

        let mut both = Caps::new(GROUPS);
        ls(&mut both, ":srv CAP * LS :chathistory draft/chathistory");
        assert_eq!(both.to_request(), ["draft/chathistory"]);
    }

    #[test]
    fn ack_minus_and_del_disable() {
        let mut caps = Caps::default();
        ls(&mut caps, ":srv CAP * LS :server-time batch");
        caps.to_request();
        caps.on_ack("server-time batch");
        assert!(caps.is_enabled("batch"));
        caps.on_ack("-batch");
        assert!(!caps.is_enabled("batch"));
        caps.on_del("server-time");
        assert!(!caps.is_enabled("server-time"));
        assert_eq!(caps.value("server-time"), None);
    }

    #[test]
    fn new_caps_can_be_requested_later() {
        let mut caps = Caps::default();
        ls(&mut caps, ":srv CAP * LS :server-time");
        caps.to_request();
        caps.on_ack("server-time");
        caps.on_new("echo-message");
        assert_eq!(caps.to_request(), ["echo-message"]);
    }

    #[test]
    fn nak_allows_asking_again() {
        let mut caps = Caps::default();
        ls(&mut caps, ":srv CAP * LS :batch");
        caps.to_request();
        caps.on_nak("batch");
        assert!(!caps.is_enabled("batch"));
        assert_eq!(caps.to_request(), ["batch"]);
    }
}
