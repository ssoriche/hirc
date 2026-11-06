//! What flows between the network thread and the UI: `Event`s up,
//! `Command`s down.

use crate::multiline::Limits;
use crate::store::Line;
use chrono::{DateTime, Utc};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The network's own buffer: MOTD, numerics, errors.
    Server,
    Channel(String),
    Query(String),
}

impl Target {
    pub fn name(&self) -> Option<&str> {
        match self {
            Target::Server => None,
            Target::Channel(n) | Target::Query(n) => Some(n),
        }
    }

    /// Same buffer? IRC names ignore ASCII case.
    pub fn same(&self, other: &Target) -> bool {
        match (self, other) {
            (Target::Server, Target::Server) => true,
            (Target::Channel(a), Target::Channel(b)) | (Target::Query(a), Target::Query(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ConnStatus {
    #[default]
    Idle,
    Connecting,
    Connected {
        nick: String,
    },
    /// The connection dropped; hirc tries again after `retry`.
    Disconnected {
        reason: String,
        retry: Duration,
    },
    /// Something went wrong that retrying won't fix (bad password, TLS
    /// setup); waits for /reconnect.
    Failed {
        reason: String,
    },
    /// The user left (/quit, /disconnect); waits for /reconnect.
    Offline,
}

impl ConnStatus {
    /// Not connected and not on the way: commands have nowhere to go.
    pub fn is_down(&self) -> bool {
        matches!(self, ConnStatus::Disconnected { .. } | ConnStatus::Failed { .. } | ConnStatus::Offline)
    }

    /// Down because something went wrong, not because the user chose it.
    pub fn is_fault(&self) -> bool {
        matches!(self, ConnStatus::Disconnected { .. } | ConnStatus::Failed { .. })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Status {
        network: String,
        status: ConnStatus,
    },
    Line {
        network: String,
        target: Target,
        line: Line,
    },
    /// One page of a buffer's history, oldest first, every line
    /// `replayed`: from the server (a `chathistory` batch) or the cache.
    /// `start`: the server has nothing older. `older`: every line is
    /// older than any shown (a page before them, from the server or the
    /// cache), so on a tie in time it goes before them; else after.
    History {
        network: String,
        target: Target,
        lines: Vec<Line>,
        start: bool,
        older: bool,
    },
    /// Where the user has read up to in `target` (draft/read-marker), as
    /// the server says; `None` is its `*`, no marker yet.
    ReadMarker {
        network: String,
        target: Target,
        time: Option<DateTime<Utc>>,
    },
    /// `target`'s history is up to date on this connection: its latest
    /// page or gap fill is in, the server gave up on it, or history can't
    /// come. Until then hirc never moves its read marker.
    Filled {
        network: String,
        target: Target,
    },
    Joined {
        network: String,
        channel: String,
    },
    Parted {
        network: String,
        channel: String,
    },
    Topic {
        network: String,
        channel: String,
        topic: Option<String>,
    },
    /// Who is in `channel` now, every nick as the server spelled it,
    /// sorted (ignoring case): sent whenever that changes. Empty once we
    /// have left.
    Members {
        network: String,
        channel: String,
        nicks: Vec<String>,
    },
    OwnNick {
        network: String,
        nick: String,
    },
    /// Whether the server takes `draft/multiline` batches now, and how
    /// big: `None` when it doesn't (each line goes as its own message).
    Multiline {
        network: String,
        limits: Option<Limits>,
    },
    /// What we learned about someone else on the network (for a query's
    /// header: are they there, away, gone).
    Peer {
        network: String,
        nick: String,
        change: PeerChange,
    },
}

/// A change in what we know of another user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerChange {
    /// Seen on the network: in a channel we share, or talking. `host` is
    /// their `user@host` when the message carried it.
    Seen { host: Option<String> },
    /// Marked away (with their message), or back (`None`): away-notify or
    /// an RPL_AWAY reply.
    Away(Option<String>),
    /// Left the network, with their reason (maybe empty).
    Quit(String),
    /// Now goes by another nick.
    Renamed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Say {
        network: String,
        target: String,
        text: String,
    },
    Action {
        network: String,
        target: String,
        text: String,
    },
    Join {
        network: String,
        channel: String,
        key: Option<String>,
    },
    Part {
        network: String,
        channel: String,
        reason: Option<String>,
    },
    Nick {
        network: String,
        nick: String,
    },
    Raw {
        network: String,
        line: String,
    },
    Disconnect {
        network: String,
        reason: Option<String>,
    },
    Reconnect {
        network: String,
    },
    /// The user is at the top of `target`'s window: show older lines.
    Older {
        network: String,
        target: String,
    },
    /// A window for `target` is open (a query from last time, or one the
    /// user just opened): keep its history up to date.
    Open {
        network: String,
        target: String,
    },
    /// The user has read `target` up to `time`: move the server's read
    /// marker there (draft/read-marker), so other clients know.
    MarkRead {
        network: String,
        target: String,
        time: DateTime<Utc>,
    },
}

impl Command {
    pub fn network(&self) -> &str {
        match self {
            Command::Say { network, .. }
            | Command::Action { network, .. }
            | Command::Join { network, .. }
            | Command::Part { network, .. }
            | Command::Nick { network, .. }
            | Command::Raw { network, .. }
            | Command::Disconnect { network, .. }
            | Command::Reconnect { network }
            | Command::Older { network, .. }
            | Command::Open { network, .. }
            | Command::MarkRead { network, .. } => network,
        }
    }
}
