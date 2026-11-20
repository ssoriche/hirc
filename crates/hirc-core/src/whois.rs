//! WHOIS replies as sentences: `hide is ~u@host (Hide)` rather than the
//! numerics' bare words.

use chrono::{DateTime, Local};

/// The numerics a WHOIS answers with, about the nick in their second
/// parameter. 318 ends the answer; 401 and 402 are "no such" errors.
pub const REPLIES: &[&str] = &["276", "301", "307", "311", "312", "313", "317", "318", "319", "320", "330", "335", "338", "378", "379", "401", "402", "671"];

/// The end of a WHOIS answer.
pub const END: &str = "318";

/// `code`'s line, from its parameters (ours first, then the nick): `None`
/// for one that shows nothing (the end).
pub fn text(code: &str, params: &[String]) -> Option<String> {
    let p = |i: usize| params.get(i).map_or("", String::as_str);
    let nick = p(1);
    let last = params.last().map_or("", String::as_str);
    Some(match code {
        END => return None,
        "311" => format!("{nick} is {}@{} ({last})", p(2), p(3)),
        "319" => format!("{nick} is in {last}"),
        "312" => format!("{nick} is on {} ({last})", p(2)),
        "330" => format!("{nick} {last} {}", p(2)),
        "301" => format!("{nick} is away: {last}"),
        // The real host, and with Ergo the IP too: `~u@host (ip)`.
        "338" => match params.get(2..params.len().saturating_sub(1)).unwrap_or_default() {
            [host] => format!("{nick}'s actual host is {host}"),
            [host, ip] => format!("{nick}'s actual host is {host} ({ip})"),
            _ => words(params),
        },
        "317" => {
            let idle = p(2).parse::<u64>().map(duration);
            let on = p(3).parse::<i64>().ok().and_then(|t| DateTime::from_timestamp(t, 0));
            match (idle, on) {
                (Ok(idle), Some(on)) => format!("{nick} has been idle {idle}, signed on {}", on.with_timezone(&Local).format("%Y-%m-%d %H:%M")),
                (Ok(idle), None) => format!("{nick} has been idle {idle}"),
                _ => words(params),
            }
        }
        "401" | "402" => format!("{nick}: {last}"),
        // "is using a secure connection", "has client certificate
        // fingerprint …": the server's own sentence, after the nick.
        _ if params.len() == 3 && (last.starts_with("is ") || last.starts_with("has ")) => format!("{nick} {last}"),
        _ => words(params),
    })
}

/// A reply as the server words it, without our nick.
fn words(params: &[String]) -> String {
    params.get(1..).unwrap_or_default().join(" ")
}

/// `secs` as `2d 3h`, `6m 30s` or `12s`: the two largest units.
fn duration(secs: u64) -> String {
    let units = [(secs / 86_400, "d"), (secs / 3_600 % 24, "h"), (secs / 60 % 60, "m"), (secs % 60, "s")];
    let first = units.iter().position(|(n, _)| *n > 0).unwrap_or(3);
    units[first..].iter().take(2).map(|(n, u)| format!("{n}{u}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_keep_two_units() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(12), "12s");
        assert_eq!(duration(390), "6m 30s");
        assert_eq!(duration(3_600), "1h 0m");
        assert_eq!(duration(2 * 86_400 + 3 * 3_600 + 59), "2d 3h");
    }

    #[test]
    fn odd_replies_read_as_the_server_words_them() {
        let params = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(text("317", &params(&["me", "bob", "x", "y", "seconds idle"])).unwrap(), "bob x y seconds idle");
        assert_eq!(text("338", &params(&["me", "bob", "Actual host"])).unwrap(), "bob Actual host");
        assert_eq!(text("338", &params(&["me", "bob", "~b@h", "1.2.3.4", "Actual user@host, Actual IP"])).unwrap(), "bob's actual host is ~b@h (1.2.3.4)");
        assert_eq!(text("318", &params(&["me", "bob", "End"])), None);
    }
}
