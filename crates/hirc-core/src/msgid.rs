//! Made-up msgids, for lines whose live and replayed copies don't share
//! a server msgid: on a server without chathistory that times its lines
//! but gives them none, and on a playback network (ZNC) always, the
//! server's msgid replaced, since ZNC passes it on live lines but replays
//! without it (ruling Q1). With one, a line's live copy and its replayed
//! copy are one line in the store, and it can go in the history cache,
//! whose key, breaks and paging all need a msgid. The id is a hash of
//! what the line says, so both copies get the same one. Two identical
//! lines from one nick in the same millisecond become one (accepted).
//! Made-up ids never leave hirc: `PREFIX` tells them apart, and nothing
//! sends one.

use crate::event::Target;
use crate::store::Line;

/// What every made-up id starts with.
pub const PREFIX: &str = "hirc:";

/// `hirc:<16 hex digits>`: FNV-1a over the line's time in milliseconds,
/// its target (lowercased, as buffers match), kind, nick and text.
pub fn made_up(target: &Target, line: &Line) -> String {
    let ms = line.time.timestamp_millis().to_string();
    let name = target.name().unwrap_or_default().to_ascii_lowercase();
    let fields = [ms.as_str(), name.as_str(), crate::cache::kind_name(line.kind), line.nick.as_deref().unwrap_or_default(), line.text.as_str()];
    // A NUL after each field (IRC text never holds one), so fields can't
    // run into each other.
    format!("{PREFIX}{:016x}", fnv1a(fields.iter().flat_map(|f| f.bytes().chain(std::iter::once(0)))))
}

pub fn is_made_up(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// FNV-1a, 64 bits. Stable across builds and Rust versions (std's hasher
/// isn't), so an id in the cache matches the one made after an upgrade.
fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.into_iter().fold(OFFSET, |h, b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::local_line;
    use crate::store::LineKind;

    fn chan(name: &str) -> Target {
        Target::Channel(name.into())
    }

    fn said(text: &str) -> Line {
        Line { time: "2026-10-22T10:00:00.250Z".parse().unwrap(), text: text.into(), ..local_line() }
    }

    #[test]
    fn fnv1a_is_the_standard_64_bit_one() {
        assert_eq!(fnv1a(*b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(*b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn the_same_line_gets_the_same_id_and_any_change_another() {
        let id = made_up(&chan("#test"), &said("hi"));
        assert!(is_made_up(&id) && id.len() == PREFIX.len() + 16, "{id}");
        assert_eq!(made_up(&chan("#TEST"), &said("hi")), id, "names ignore case");
        let others = [
            made_up(&chan("#other"), &said("hi")),
            made_up(&chan("#test"), &said("hi!")),
            made_up(&chan("#test"), &Line { nick: Some("carol".into()), ..said("hi") }),
            made_up(&chan("#test"), &Line { kind: LineKind::Notice, ..said("hi") }),
            made_up(&chan("#test"), &Line { time: "2026-10-22T10:00:00.251Z".parse().unwrap(), ..said("hi") }),
        ];
        assert!(others.iter().all(|o| *o != id), "{others:?}");
        // Arrival order and the replayed flag are not what a line says.
        assert_eq!(made_up(&chan("#test"), &Line { seq: 9, replayed: true, ..said("hi") }), id);
    }

    #[test]
    fn fields_never_run_into_each_other() {
        let a = made_up(&chan("#t"), &Line { nick: Some("ab".into()), ..said("c") });
        let b = made_up(&chan("#t"), &Line { nick: Some("a".into()), ..said("bc") });
        assert_ne!(a, b);
    }
}
