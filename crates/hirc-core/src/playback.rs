//! ZNC's playback module (`znc.in/playback`). With its cap on, ZNC stops
//! replaying its buffers when we attach, and sends what was missed when
//! asked with `PLAY`. Pure: `Session` sends what this builds, and reads
//! ZNC's replay with these names. The values were checked against ZNC
//! 1.10.3 (`tests/data/znc-1.10.3-*.txt`).
//!
//! ZNC brackets a batch-less replay with `***` "Buffer Playback..." lines
//! only for a client without `server-time`, and hirc always has it from
//! ZNC: none were ever recorded, so nothing here looks for them. Without
//! batches, a replayed line is known by its time alone (`Session`).

use crate::proto::Message;
use chrono::{DateTime, Utc};

/// The cap that makes ZNC wait for `PLAY`.
pub const CAP: &str = "znc.in/playback";
/// Lines we send from another client come to us as our own.
pub const SELF_MESSAGE: &str = "znc.in/self-message";
/// The batch type one buffer's replay comes in. ZNC sends an empty one
/// per buffer on every attach, and reuses each buffer's ref every time.
pub const BATCH: &str = "znc.in/playback";
/// Who `PLAY` goes to.
pub const MODULE: &str = "*playback";
/// Lines kept from one replayed buffer at most. ZNC's own cap
/// (`MaxBufferSize`) is 500 by default; this only bounds a bouncer gone
/// wrong.
pub const MAX_LINES: usize = 5000;

/// `PLAY`'s time: the newest line held, in whole epoch seconds, or 0
/// (everything ZNC has) when nothing is held. Rounding down asks for a
/// little more than was missed, never less; the copies of lines we hold
/// collapse by msgid.
pub fn since(newest: Option<DateTime<Utc>>) -> i64 {
    newest.map_or(0, |t| t.timestamp()).max(0)
}

/// `PLAY * <since>`: every buffer, queries included.
pub fn play_text(since: i64) -> String {
    format!("PLAY * {since}")
}

/// `PRIVMSG *playback :PLAY * <since>`.
pub fn play(since: i64) -> Message {
    Message::new("PRIVMSG", [MODULE.to_owned(), play_text(since)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_asks_every_buffer_from_whole_seconds() {
        assert_eq!(play(since(None)).to_wire(), "PRIVMSG *playback :PLAY * 0");
        let t: DateTime<Utc> = "2026-10-22T10:00:00.999Z".parse().unwrap();
        assert_eq!(play(since(Some(t))).to_wire(), "PRIVMSG *playback :PLAY * 1792663200");
        // A clock before 1970 still asks for everything, not a negative time.
        let t: DateTime<Utc> = "1969-12-31T23:59:00Z".parse().unwrap();
        assert_eq!(since(Some(t)), 0);
    }

    #[test]
    fn the_recorded_play_is_what_we_send() {
        let sent = include_str!("../tests/data/znc-1.10.3-play-batch.txt").lines().next().and_then(|l| l.strip_prefix("> "));
        assert_eq!(sent, Some(play(0).to_wire().as_str()));
    }
}
