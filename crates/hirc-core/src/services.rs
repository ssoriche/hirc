//! Network services (NickServ and friends). What is said to them can hold
//! a password (`IDENTIFY <password>`), so hirc never opens, backfills or
//! keeps their queries on its own; only the user opens one. A bouncer's
//! modules (ZNC's `*status`) and services (soju's `BouncerServ`) count
//! as services.

/// Services nicks, matched exactly (ASCII case-insensitive). No suffix
/// rule: plenty of real nicks end in "serv".
pub const SERVICES: &[&str] = &["NickServ", "ChanServ", "SaslServ", "HostServ", "MemoServ", "OperServ", "BotServ", "HistServ", "BouncerServ"];

/// A `NickServ@services.libera.chat` style name counts by its nick part.
/// A bouncer's own modules (ZNC's `*status`, `*playback`,
/// `*controlpanel`) count too: no nick starts with `*`, and what is said
/// to them can hold a password (`*controlpanel Set Password`).
pub fn is_service(nick: &str) -> bool {
    let nick = nick.split(['@', '!']).next().unwrap_or(nick);
    (nick.len() > 1 && nick.starts_with('*')) || SERVICES.iter().any(|s| s.eq_ignore_ascii_case(nick))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_are_exact_names_in_any_case() {
        assert!(is_service("NickServ") && is_service("nickserv") && is_service("CHANSERV") && is_service("HistServ"));
        assert!(is_service("NickServ@services.libera.chat") && is_service("chanserv!ChanServ@services.") && is_service("NICKSERV@x"));
        assert!(!is_service("bob@NickServ") && !is_service("@NickServ"));
        assert!(!is_service("FooServ") && !is_service("NickServ2") && !is_service("Serv") && !is_service(""));
        // A bouncer's own modules: no nick starts with `*`.
        assert!(is_service("*status") && is_service("*playback") && is_service("*controlpanel@znc.in"));
        assert!(!is_service("*"));
    }

    #[test]
    fn soju_s_own_service_is_a_service() {
        // `sasl set-plain`, `user update -password`: it takes passwords.
        assert!(is_service("BouncerServ") && is_service("bouncerserv") && is_service("BouncerServ@soju"));
    }
}
