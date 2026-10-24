//! What flows between the network thread and the UI: `Event`s up,
//! `Command`s down.

use crate::store::Line;
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
    Status { network: String, status: ConnStatus },
    Line { network: String, target: Target, line: Line },
    Joined { network: String, channel: String },
    Parted { network: String, channel: String },
    Topic { network: String, channel: String, topic: Option<String> },
    OwnNick { network: String, nick: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Say { network: String, target: String, text: String },
    Action { network: String, target: String, text: String },
    Join { network: String, channel: String, key: Option<String> },
    Part { network: String, channel: String, reason: Option<String> },
    Nick { network: String, nick: String },
    Raw { network: String, line: String },
    Disconnect { network: String, reason: Option<String> },
    Reconnect { network: String },
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
            | Command::Reconnect { network } => network,
        }
    }
}
