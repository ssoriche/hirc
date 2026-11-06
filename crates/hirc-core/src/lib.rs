//! Everything hirc does that isn't drawing: the IRC protocol, per-server
//! state machines, the message store and config loading. No UI types here,
//! so it builds and tests without gpui.

pub mod batch;
pub mod bridge;
pub mod cache;
pub mod caps;
pub mod command;
pub mod config;
pub mod conn;
pub mod display;
pub mod emoji;
pub mod event;
pub mod history;
pub mod irccloud;
pub mod isupport;
pub mod layout;
pub mod msgid;
pub mod multiline;
pub mod playback;
pub mod proto;
pub mod sasl;
pub mod services;
pub mod session;
pub mod split;
pub mod store;
pub mod theme;
pub mod wire;
