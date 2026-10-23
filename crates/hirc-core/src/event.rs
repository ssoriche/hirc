//! What flows between the network thread and the UI: `Event`s up,
//! `Command`s down.

use crate::store::Line;

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
    /// Will retry by itself.
    Disconnected {
        reason: String,
    },
    /// Won't retry until /reconnect (bad password, /disconnect).
    Failed {
        reason: String,
    },
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
