//! IRCCloud's bouncer (`bnc.irccloud.com`). Its server password is
//! `bnc:<key>`; written `bnc@<id>:<key>`, it replays what client `<id>`
//! hasn't seen each time that client attaches, as plain lines timed with
//! server-time: no batch, no msgids, nothing asked for
//! (<https://blog.irccloud.com/bouncer/>). hirc puts the id in from the
//! config's `bouncer_client_id`, so the stored password stays as IRCCloud
//! gave it. Pure: `conn` builds the password with these, and `Session`
//! reads the replay by its time alone.

/// The bouncer's host.
pub const HOST: &str = "bnc.irccloud.com";

/// Whether `host` (the config's `server`) is IRCCloud's bouncer.
pub fn is_host(host: &str) -> bool {
    host.trim_end_matches('.').eq_ignore_ascii_case(HOST)
}

/// A client id IRCCloud can read back out of the password: not empty, and
/// nothing that ends or splits it (`:`, `@`, spaces).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && !id.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, ':' | '@'))
}

/// `password` with client `id` put in: `bnc:<key>` becomes `bnc@<id>:<key>`.
/// Any other password (one that names a client already, or isn't
/// IRCCloud's) goes as it is.
pub fn password(password: &str, id: &str) -> String {
    match password.strip_prefix("bnc:") {
        Some(key) => format!("bnc@{id}:{key}"),
        None => password.to_owned(),
    }
}

/// Whether `password` names a client, so IRCCloud replays what it missed.
pub fn replays(password: &str) -> bool {
    password.strip_prefix("bnc@").and_then(|rest| rest.split_once(':')).is_some_and(|(id, _)| valid_id(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_is_irccloud_s_bouncer_alone() {
        assert!(is_host("bnc.irccloud.com"));
        assert!(is_host("BNC.IRCCloud.com."));
        assert!(!is_host("irc.irccloud.com"));
        assert!(!is_host("bnc.irccloud.com.example"));
    }

    #[test]
    fn the_id_goes_in_after_bnc() {
        assert_eq!(password("bnc:abcxyz", "laptop"), "bnc@laptop:abcxyz");
        // A key with a colon in it stays whole.
        assert_eq!(password("bnc:ab:cd", "laptop"), "bnc@laptop:ab:cd");
        // One that names a client already, or isn't IRCCloud's: as it is.
        assert_eq!(password("bnc@desk:abcxyz", "laptop"), "bnc@desk:abcxyz");
        assert_eq!(password("hunter2", "laptop"), "hunter2");
    }

    #[test]
    fn ids_that_would_break_the_password_are_refused() {
        assert!(valid_id("laptop"));
        assert!(valid_id("hirc-work.2"));
        for bad in ["", "my laptop", "a:b", "a@b", "tab\there"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_a_password_naming_a_client_replays() {
        assert!(replays("bnc@laptop:abcxyz"));
        assert!(!replays("bnc:abcxyz"));
        assert!(!replays("bnc@:abcxyz"));
        assert!(!replays("hunter2"));
    }
}
