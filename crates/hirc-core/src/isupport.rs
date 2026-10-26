//! RPL_ISUPPORT (005): what kind of names are channels, which nick prefixes
//! exist, and the server's CHATHISTORY limit.

#[derive(Clone, Debug)]
pub struct Isupport {
    pub network: Option<String>,
    pub chantypes: String,
    /// (mode, symbol), highest first: `[('o', '@'), ('v', '+')]`.
    pub prefix: Vec<(char, char)>,
    pub chathistory: Option<usize>,
}

impl Default for Isupport {
    fn default() -> Isupport {
        Isupport { network: None, chantypes: "#&".into(), prefix: vec![('o', '@'), ('v', '+')], chathistory: None }
    }
}

impl Isupport {
    /// The params of a 005 line: our nick, the tokens, then trailing text.
    pub fn apply(&mut self, params: &[String]) {
        let n = params.len();
        if n < 3 {
            return;
        }
        for tok in &params[1..n - 1] {
            let (neg, tok) = match tok.strip_prefix('-') {
                Some(t) => (true, t),
                None => (false, tok.as_str()),
            };
            let (key, val) = tok.split_once('=').unwrap_or((tok, ""));
            match key {
                "NETWORK" => self.network = (!neg).then(|| val.to_owned()),
                "CHANTYPES" => self.chantypes = if neg { Isupport::default().chantypes } else { val.to_owned() },
                "PREFIX" => self.prefix = if neg { Isupport::default().prefix } else { parse_prefix(val) },
                // 0 is the spec's "no limit".
                "CHATHISTORY" | "draft/CHATHISTORY" => self.chathistory = if neg { None } else { val.parse().ok().filter(|&n| n > 0) },
                _ => {}
            }
        }
    }

    pub fn is_channel(&self, target: &str) -> bool {
        target.chars().next().is_some_and(|c| self.chantypes.contains(c))
    }

    /// `"@+nick!user@host"` (multi-prefix, userhost-in-names) -> `"nick"`.
    pub fn bare_nick<'a>(&self, name: &'a str) -> &'a str {
        let n = name.trim_start_matches(|c| self.prefix.iter().any(|&(_, s)| s == c));
        n.split_once('!').map_or(n, |(n, _)| n)
    }
}

/// `"(ov)@+"` -> `[('o', '@'), ('v', '+')]`. Anything else -> empty.
fn parse_prefix(v: &str) -> Vec<(char, char)> {
    let Some((modes, syms)) = v.strip_prefix('(').and_then(|r| r.split_once(')')) else { return Vec::new() };
    modes.chars().zip(syms.chars()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(s: &str) -> Vec<String> {
        crate::proto::Message::parse(s).unwrap().params
    }

    #[test]
    fn applies_tokens_and_negation() {
        let mut i = Isupport::default();
        i.apply(&params(":srv 005 alice NETWORK=HircTest CHANTYPES=# PREFIX=(qaohv)~&@%+ CHATHISTORY=1000 :are supported by this server"));
        assert_eq!(i.network.as_deref(), Some("HircTest"));
        assert!(i.is_channel("#a") && !i.is_channel("&a") && !i.is_channel("bob"));
        assert_eq!(i.prefix[0], ('q', '~'));
        assert_eq!(i.chathistory, Some(1000));
        i.apply(&params(":srv 005 alice -CHATHISTORY :are supported"));
        assert_eq!(i.chathistory, None);
    }

    #[test]
    fn bare_nick_strips_prefixes_and_userhost() {
        let i = Isupport::default();
        assert_eq!(i.bare_nick("@+bob!b@host"), "bob");
        assert_eq!(i.bare_nick("carol"), "carol");
    }

    #[test]
    fn ignores_malformed() {
        let mut i = Isupport::default();
        i.apply(&params(":srv 005 alice PREFIX=garbage =x :y"));
        i.apply(&[]);
        assert!(i.prefix.is_empty());
    }

    #[test]
    fn chathistory_zero_is_no_limit() {
        let mut i = Isupport::default();
        i.apply(&params(":srv 005 alice CHATHISTORY=0 :are supported"));
        assert_eq!(i.chathistory, None);
        i.apply(&params(":srv 005 alice draft/CHATHISTORY=50 :are supported"));
        assert_eq!(i.chathistory, Some(50));
    }
}
