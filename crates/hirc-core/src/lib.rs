//! Everything hirc does that isn't drawing: the IRC protocol, per-server
//! state machines, the message store and config loading. No UI types here,
//! so it builds and tests without gpui.

pub mod caps;
pub mod isupport;
pub mod proto;
pub mod sasl;
pub mod split;
