//! Network services (NickServ and friends). What is said to them can hold
//! a password (`IDENTIFY <password>`), so hirc never opens, backfills or
//! keeps their queries on its own; only the user opens one.

/// Services nicks, matched exactly (ASCII case-insensitive). No suffix
/// rule: plenty of real nicks end in "serv".
pub const SERVICES: &[&str] = &["NickServ", "ChanServ", "SaslServ", "HostServ", "MemoServ", "OperServ", "BotServ", "HistServ"];

/// A `NickServ@services.libera.chat` style name counts by its nick part.
pub fn is_service(nick: &str) -> bool {
    let nick = nick.split(['@', '!']).next().unwrap_or(nick);
    SERVICES.iter().any(|s| s.eq_ignore_ascii_case(nick))
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
    }
}
