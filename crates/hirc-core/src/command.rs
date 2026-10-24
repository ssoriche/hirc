//! What the user typed into the input box: plain text or a /command.

use crate::event::Command;

/// irssi-style window commands; numbers are 1-based, as typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowAction {
    Goto(usize),
    Move(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Send(Command),
    /// /query: open (or switch to) a DM buffer, maybe sending a first line.
    OpenQuery {
        nick: String,
        text: Option<String>,
    },
    Window(WindowAction),
}

/// Every command `parse` knows (case-insensitive), for the input line to
/// show a recognised command as one.
pub const COMMANDS: &[&str] =
    &["window", "win", "me", "join", "j", "part", "leave", "msg", "privmsg", "query", "q", "nick", "raw", "quote", "topic", "quit", "disconnect", "reconnect"];

/// The command a line starts with (`/me waves` is `me`), if it is one.
pub fn leading(input: &str) -> Option<&str> {
    let rest = input.strip_prefix('/').filter(|r| !r.starts_with('/'))?;
    let cmd = rest.split(' ').next().unwrap_or(rest);
    COMMANDS.iter().any(|c| c.eq_ignore_ascii_case(cmd)).then_some(cmd)
}

fn is_channel(s: &str) -> bool {
    s.starts_with(['#', '&'])
}

/// `target` is the current buffer's channel or nick (`None` in the server
/// buffer). Errors are short sentences to show the user.
pub fn parse(input: &str, network: &str, target: Option<&str>) -> Result<Input, String> {
    let net = network.to_owned();
    let here = || target.map(str::to_owned).ok_or_else(|| "This buffer has no channel or nick. Use /msg or /join.".to_owned());
    let input = input.trim_end();
    let Some(rest) = input.strip_prefix('/').filter(|r| !r.starts_with('/')) else {
        let text = input.strip_prefix('/').unwrap_or(input).to_owned();
        return Ok(Input::Send(Command::Say { network: net, target: here()?, text }));
    };
    let (cmd, args) = rest.split_once(' ').unwrap_or((rest, ""));
    let args = args.trim();
    let (first, tail) = args.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((args, ""));
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    if matches!(cmd.to_ascii_lowercase().as_str(), "window" | "win") {
        let number = |s: &str| s.parse::<usize>().ok().filter(|&n| n >= 1);
        let action = match (first.to_ascii_lowercase().as_str(), tail) {
            ("move", n) => number(n).map(WindowAction::Move),
            ("goto", n) => number(n).map(WindowAction::Goto),
            (n, "") => number(n).map(WindowAction::Goto),
            _ => None,
        };
        return action.map(Input::Window).ok_or_else(|| "Usage: /window <number> | /window move <number>".to_owned());
    }
    let cmd = match cmd.to_ascii_lowercase().as_str() {
        "me" if args.is_empty() => return Err("Usage: /me <text>".into()),
        "me" => Command::Action { network: net, target: here()?, text: args.to_owned() },
        "join" | "j" if !first.is_empty() => Command::Join { network: net, channel: first.to_owned(), key: opt(tail) },
        "part" | "leave" if is_channel(first) => Command::Part { network: net, channel: first.to_owned(), reason: opt(tail) },
        "part" | "leave" => Command::Part { network: net, channel: here()?, reason: opt(args) },
        "msg" | "privmsg" if !first.is_empty() && !tail.is_empty() => Command::Say { network: net, target: first.to_owned(), text: tail.to_owned() },
        "query" | "q" if !first.is_empty() => return Ok(Input::OpenQuery { nick: first.to_owned(), text: opt(tail) }),
        "nick" if !first.is_empty() => Command::Nick { network: net, nick: first.to_owned() },
        "raw" | "quote" if !args.is_empty() => Command::Raw { network: net, line: args.to_owned() },
        "topic" => {
            let chan = here()?;
            let line = if args.is_empty() { format!("TOPIC {chan}") } else { format!("TOPIC {chan} :{args}") };
            Command::Raw { network: net, line }
        }
        "quit" | "disconnect" => Command::Disconnect { network: net, reason: opt(args) },
        "reconnect" => Command::Reconnect { network: net },
        "join" | "j" | "msg" | "privmsg" | "query" | "q" | "nick" | "raw" | "quote" => return Err(format!("/{cmd} needs more arguments")),
        _ => return Err(format!("Unknown command /{cmd}")),
    };
    Ok(Input::Send(cmd))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Input, String> {
        parse(s, "net", Some("#chan"))
    }

    #[test]
    fn every_listed_command_is_parsed() {
        for c in COMMANDS {
            assert!(!matches!(p(&format!("/{c} x y")), Err(e) if e.starts_with("Unknown")), "{c}");
        }
        assert_eq!(leading("/me waves"), Some("me"));
        assert_eq!(leading("/JOIN"), Some("JOIN"));
        assert_eq!(leading("/mw waves"), None);
        assert_eq!(leading("//me"), None);
        assert_eq!(leading("me"), None);
    }

    #[test]
    fn bare_me_is_a_usage_error() {
        for s in ["/me", "/ME", "/me   "] {
            assert_eq!(p(s), Err("Usage: /me <text>".to_owned()), "{s}");
        }
    }

    fn send(s: &str) -> Command {
        match p(s) {
            Ok(Input::Send(c)) => c,
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn plain_text_says_to_current_target() {
        assert_eq!(send("hello"), Command::Say { network: "net".into(), target: "#chan".into(), text: "hello".into() });
        assert_eq!(send("//not a command"), Command::Say { network: "net".into(), target: "#chan".into(), text: "/not a command".into() });
    }

    #[test]
    fn server_buffer_rejects_plain_text() {
        assert!(parse("hello", "net", None).is_err());
        assert!(matches!(parse("/join #x", "net", None), Ok(Input::Send(Command::Join { .. }))));
    }

    #[test]
    fn commands() {
        assert_eq!(send("/me waves"), Command::Action { network: "net".into(), target: "#chan".into(), text: "waves".into() });
        assert_eq!(send("/join #x key"), Command::Join { network: "net".into(), channel: "#x".into(), key: Some("key".into()) });
        assert_eq!(send("/J #x"), Command::Join { network: "net".into(), channel: "#x".into(), key: None });
        assert_eq!(send("/part"), Command::Part { network: "net".into(), channel: "#chan".into(), reason: None });
        assert_eq!(send("/part bye all"), Command::Part { network: "net".into(), channel: "#chan".into(), reason: Some("bye all".into()) });
        assert_eq!(send("/part #other bye"), Command::Part { network: "net".into(), channel: "#other".into(), reason: Some("bye".into()) });
        assert_eq!(send("/msg bob hi there"), Command::Say { network: "net".into(), target: "bob".into(), text: "hi there".into() });
        assert_eq!(send("/nick newme"), Command::Nick { network: "net".into(), nick: "newme".into() });
        assert_eq!(send("/raw PRIVMSG x :y"), Command::Raw { network: "net".into(), line: "PRIVMSG x :y".into() });
        assert_eq!(send("/quote WHO #x"), Command::Raw { network: "net".into(), line: "WHO #x".into() });
        assert_eq!(send("/topic new topic"), Command::Raw { network: "net".into(), line: "TOPIC #chan :new topic".into() });
        assert_eq!(send("/quit later"), Command::Disconnect { network: "net".into(), reason: Some("later".into()) });
        assert_eq!(send("/reconnect"), Command::Reconnect { network: "net".into() });
    }

    #[test]
    fn query_opens_a_buffer() {
        assert_eq!(p("/query bob"), Ok(Input::OpenQuery { nick: "bob".into(), text: None }));
        assert_eq!(p("/query bob hey"), Ok(Input::OpenQuery { nick: "bob".into(), text: Some("hey".into()) }));
    }

    #[test]
    fn window_commands() {
        let w = |s: &str| parse(s, "net", None);
        assert_eq!(w("/window 3"), Ok(Input::Window(WindowAction::Goto(3))));
        assert_eq!(w("/WIN goto 2"), Ok(Input::Window(WindowAction::Goto(2))));
        assert_eq!(w("/window move 1"), Ok(Input::Window(WindowAction::Move(1))));
        let usage = Err("Usage: /window <number> | /window move <number>".to_owned());
        for s in ["/window", "/window 0", "/window move x", "/window move", "/window 1 2"] {
            assert_eq!(w(s), usage, "{s}");
        }
    }

    #[test]
    fn errors() {
        assert!(p("/join").is_err());
        assert!(p("/msg bob").is_err());
        assert!(p("/frobnicate").unwrap_err().contains("/frobnicate"));
        assert!(parse("/part", "net", None).is_err());
    }
}
