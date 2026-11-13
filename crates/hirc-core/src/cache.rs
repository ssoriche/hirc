//! The history cache: lines hirc has shown, in SQLite, so a window opens
//! on its history at once and a reconnect asks the server only for what
//! is newer. Message lines only: no config, no secrets, and nothing said
//! in a services query (`/msg NickServ IDENTIFY ...` is such a line).
//! Reads run on the network thread; writes run on a thread of their own
//! (`Shared`). Neither ever runs on the UI thread.

use crate::event::Target;
use crate::services;
use crate::store::{Delivery, Line, LineKind};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

pub const FILE: &str = "history.db";

/// The schema's version, kept in `PRAGMA user_version`. A file with another
/// version is not ours to read, so it is moved aside. Versions 1 (no `brk`)
/// and 2 (no `via`) and 3 (no `reply_to`, no reactions) are brought up to
/// date in place.
const VERSION: i64 = 4;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lines (
    network TEXT NOT NULL,
    target  TEXT NOT NULL,
    name    TEXT NOT NULL,
    query   INTEGER NOT NULL,
    msgid   TEXT NOT NULL,
    time    INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    nick    TEXT,
    text    TEXT NOT NULL,
    own     INTEGER NOT NULL,
    -- Not known to follow on from the next older line held for the target:
    -- lines between them may be missing, so paging stops here.
    brk     INTEGER NOT NULL DEFAULT 0,
    -- The bridge that relayed the line, when one did.
    via     TEXT,
    -- The msgid of the line it answers, when it is a reply.
    reply_to TEXT,
    PRIMARY KEY (network, target, msgid)
);
CREATE INDEX IF NOT EXISTS lines_by_time ON lines (network, target, time);
CREATE TABLE IF NOT EXISTS meta (
    network    TEXT PRIMARY KEY,
    discovered INTEGER
);
-- Whether the network's last connection could serve history.
CREATE TABLE IF NOT EXISTS networks (
    network TEXT PRIMARY KEY,
    history INTEGER NOT NULL
);
-- Who reacted to which line with what, as it last stood: `shown` is 0
-- once taken back, kept so an older copy from history can't bring it
-- back. `who` is the nick lowercased; `nick` as shown.
CREATE TABLE IF NOT EXISTS reactions (
    network TEXT NOT NULL,
    target  TEXT NOT NULL,
    parent  TEXT NOT NULL,
    emoji   TEXT NOT NULL,
    who     TEXT NOT NULL,
    nick    TEXT NOT NULL,
    shown   INTEGER NOT NULL,
    time    INTEGER NOT NULL,
    PRIMARY KEY (network, target, parent, emoji, who)
);
";

const KINDS: [(LineKind, &str); 13] = [
    (LineKind::Privmsg, "privmsg"),
    (LineKind::Notice, "notice"),
    (LineKind::Action, "action"),
    (LineKind::Join, "join"),
    (LineKind::Part, "part"),
    (LineKind::Quit, "quit"),
    (LineKind::Nick, "nick"),
    (LineKind::Kick, "kick"),
    (LineKind::Topic, "topic"),
    (LineKind::Mode, "mode"),
    (LineKind::Server, "server"),
    (LineKind::Status, "status"),
    (LineKind::Error, "error"),
];

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("{0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the cache is version {0}, not {VERSION}")]
    Version(i64),
}

/// `$XDG_DATA_HOME/hirc/history.db`, else `~/.local/share/hirc/history.db`.
pub fn path() -> PathBuf {
    path_from(std::env::var_os("XDG_DATA_HOME"), dirs::home_dir())
}

/// `path` from the environment it is given. XDG says a relative
/// `XDG_DATA_HOME` is to be ignored.
pub fn path_from(xdg: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    let base = xdg.map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.unwrap_or_default().join(".local/share"));
    base.join("hirc").join(FILE)
}

/// Services: their queries carry passwords, so they are never kept.
pub fn is_service(nick: &str) -> bool {
    services::is_service(nick)
}

/// Whether `line` in `target` goes in the cache: a message the server
/// gave a msgid (so it can be matched with the server's copy), in a
/// channel or a query that isn't with services. Every write is a run
/// through `put`, which keeps only these, so this is the one gate. A
/// services name never counts as a place, whatever it was taken for (a
/// `*status` from a server with `*` in CHANTYPES).
pub fn cacheable(target: &Target, line: &Line) -> bool {
    let place = match target {
        Target::Server => false,
        Target::Channel(name) | Target::Query(name) => !is_service(name),
    };
    place && line.msgid.is_some() && line.delivery == Delivery::Sent && !line.boundary
}

/// Lines for one target, to be written: a run with nothing missing
/// between them, oldest first.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub network: String,
    pub target: Target,
    pub lines: Vec<Line>,
    /// How the run's oldest line meets the next older line held.
    pub below: Below,
    /// How its newest line meets the next newer line held.
    pub above: Above,
    /// A page from before what is held: on a tie in time, its lines are
    /// older than those held, though stored after them.
    pub older: bool,
}

/// The older side of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Below {
    /// It follows straight on from the line with this msgid. Taken only
    /// when that line is held and is the next older one.
    Joins(String),
    /// Not known: its oldest line is a break.
    Breaks,
}

/// The newer side of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Above {
    /// The line with this msgid follows straight on from it (a page
    /// before that line), so a break there is healed. Taken only when that
    /// line is held and is the next newer one; else as `Breaks`.
    Joins(String),
    /// Not known: if the run's newest line is new, the next newer line
    /// held is a break (the run may sit in a hole below it).
    Breaks,
    /// Nothing newer is meant (a live line): leave it.
    Leave,
}

/// How far a read goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Down to the first break: the server fills in what is past it.
    ToBreak,
    /// Past breaks: for a network that can't serve history, the cache is
    /// all the history there is. The breaks stay, for when it can.
    All,
}

/// Lines from `Cache::before`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Older {
    pub lines: Vec<Line>,
    /// They stop at a break (or the anchor isn't held, or is one): what is
    /// older must come from the server. Not the start of history.
    pub broke: bool,
}

pub struct Cache {
    db: Connection,
}

impl Cache {
    /// Open the cache at `path`, making it if need be. A file that is
    /// corrupt, not a database, or of another version is moved aside (never
    /// deleted) and a new one started. Any other failure (a lock held by
    /// another process, a full disk) is returned and the file left alone.
    pub fn open(path: &Path) -> Result<Cache, CacheError> {
        if let Some(dir) = path.parent() {
            make_dir(dir)?;
        }
        create_private(path)?;
        let db = Connection::open(path)?;
        match Cache::prepare(path, &db) {
            Ok(()) => Ok(Cache { db }),
            Err(e) if unusable(&e) => {
                tracing::warn!("history cache {} is unusable ({e}); starting a new one", path.display());
                // Moved while still open: closing a connection can delete a
                // WAL that is beside it, and that file is to be kept.
                move_aside(path)?;
                drop(db);
                create_private(path)?;
                let db = Connection::open(path)?;
                Cache::prepare(path, &db)?;
                Ok(Cache { db })
            }
            Err(e) => Err(e),
        }
    }

    fn prepare(path: &Path, db: &Connection) -> Result<(), CacheError> {
        tighten(path)?;
        // A file that isn't a database fails on its first statement.
        db.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !(0..=VERSION).contains(&version) {
            return Err(CacheError::Version(version));
        }
        if version == 1 {
            // Every line already held stays as it was: following on.
            // Version 1 kept no breaks, and could have holes: what is held
            // is taken as following on, but not what came before it.
            db.execute_batch(
                "BEGIN;
                 ALTER TABLE lines ADD COLUMN brk INTEGER NOT NULL DEFAULT 0;
                 UPDATE lines SET brk = 1 WHERE rowid IN (
                     SELECT (SELECT i.rowid FROM lines i WHERE i.network = o.network AND i.target = o.target ORDER BY i.time, i.rowid LIMIT 1)
                     FROM lines o GROUP BY o.network, o.target);
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        }
        if (1..=2).contains(&version) {
            // Lines held from before kept no bridge: they read as said on IRC.
            db.execute_batch("BEGIN; ALTER TABLE lines ADD COLUMN via TEXT; PRAGMA user_version = 3; COMMIT;")?;
        }
        if (1..=3).contains(&version) {
            // Lines held from before answered nothing that was said.
            db.execute_batch(&format!("BEGIN; ALTER TABLE lines ADD COLUMN reply_to TEXT; PRAGMA user_version = {VERSION}; COMMIT;"))?;
        }
        db.execute_batch(SCHEMA)?;
        db.query_row("SELECT count(brk), count(via), count(reply_to) FROM lines", [], |_| Ok(()))?;
        db.query_row("SELECT count(*) FROM reactions", [], |_| Ok(()))?;
        if version == 0 {
            db.execute_batch(&format!("PRAGMA user_version = {VERSION}"))?;
        }
        // The WAL and its index exist now; they hold lines too.
        tighten(path)?;
        Ok(())
    }

    /// Write every cacheable line, in one transaction. A line already
    /// there (same msgid) is kept, but a run proves that each of its lines
    /// after the first follows on, so their breaks are healed. A run's
    /// oldest new line is a break unless it `Joins` (see `Below`, `Above`).
    /// A line held under another msgid (the same time, kind, nick and
    /// text) is skipped, and the next line after it is a break.
    /// Returns how many went in.
    pub fn put(&mut self, entries: &[Entry]) -> Result<usize, CacheError> {
        let tx = self.db.transaction()?;
        let mut n = 0;
        for e in entries {
            n += put_run(&tx, e)?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// The newest `limit` lines of `target`, oldest first, down to the
    /// first break: they follow on from each other.
    pub fn latest(&self, network: &str, target: &str, limit: usize) -> Result<Vec<Line>, CacheError> {
        self.latest_in(network, target, limit, Reach::ToBreak)
    }

    /// `latest`, reading past breaks with `Reach::All`.
    pub fn latest_in(&self, network: &str, target: &str, limit: usize, reach: Reach) -> Result<Vec<Line>, CacheError> {
        Ok(self.query(network, &target.to_ascii_lowercase(), i64::MAX, i64::MIN, limit, reach)?.lines)
    }

    /// The `limit` lines of `target` just older than the line with
    /// `msgid`, oldest first, down to and including the first break, never
    /// past it. Lines sharing the anchor's time with a lower rowid are
    /// older (a run from before what is held takes rowids below it). An
    /// anchor that isn't held, or is a break itself, gives
    /// nothing: the cache can't say what comes before it. With
    /// `Reach::All`, breaks are read past (and never reported).
    pub fn before(&self, network: &str, target: &str, msgid: &str, limit: usize, reach: Reach) -> Result<Older, CacheError> {
        let target = target.to_ascii_lowercase();
        let anchor = self
            .db
            .query_row("SELECT time, rowid, brk FROM lines WHERE network = ?1 AND target = ?2 AND msgid = ?3", params![network, target, msgid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, bool>(2)?))
            })
            .optional()?;
        match (anchor, reach) {
            (Some((time, rowid, false)), _) | (Some((time, rowid, true)), Reach::All) => self.query(network, &target, time, rowid, limit, reach),
            _ => Ok(Older { lines: Vec::new(), broke: true }),
        }
    }

    /// The `limit` lines of `target` older than `time`, oldest first, past
    /// every break: for an anchor not held (yet), with `Reach::All`.
    pub fn before_time(&self, network: &str, target: &str, time: DateTime<Utc>, limit: usize) -> Result<Older, CacheError> {
        self.query(network, &target.to_ascii_lowercase(), time.timestamp_millis(), i64::MIN, limit, Reach::All)
    }

    /// Keep a reaction (or its taking back), unless what is held of it is
    /// newer. Never one in a services query, or to a made-up msgid.
    pub fn react(&self, network: &str, target: &Target, r: &Reacted) -> Result<(), CacheError> {
        let Some(name) = target.name().filter(|n| !is_service(n)) else { return Ok(()) };
        if crate::msgid::is_made_up(&r.parent) {
            return Ok(());
        }
        self.db
            .prepare_cached(
                "INSERT INTO reactions (network, target, parent, emoji, who, nick, shown, time) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (network, target, parent, emoji, who) DO UPDATE SET nick = excluded.nick, shown = excluded.shown, time = excluded.time
                 WHERE excluded.time >= reactions.time",
            )?
            .execute(params![network, name.to_ascii_lowercase(), r.parent, r.emoji, r.nick.to_ascii_lowercase(), r.nick, r.on, r.time.timestamp_millis()])?;
        Ok(())
    }

    /// What is held of the reactions to the lines with these msgids in
    /// `target`, taken-back ones too, oldest change first.
    pub fn reactions(&self, network: &str, target: &str, parents: &[&str]) -> Result<Vec<Reacted>, CacheError> {
        let mut st = self.db.prepare_cached("SELECT parent, emoji, nick, shown, time FROM reactions WHERE network = ?1 AND target = ?2 AND parent = ?3")?;
        let mut out = Vec::new();
        for p in parents {
            let rows = st.query_map(params![network, target.to_ascii_lowercase(), p], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, bool>(3)?, r.get::<_, i64>(4)?))
            })?;
            for row in rows {
                let (parent, emoji, nick, on, ms) = row?;
                if let Some(time) = DateTime::from_timestamp_millis(ms) {
                    out.push(Reacted { parent, emoji, nick, on, time });
                }
            }
        }
        out.sort_by_key(|r| r.time);
        Ok(out)
    }

    fn query(&self, network: &str, target: &str, time: i64, rowid: i64, limit: usize, reach: Reach) -> Result<Older, CacheError> {
        let mut st = self.db.prepare_cached(
            "SELECT msgid, time, kind, nick, text, own, brk, via, reply_to FROM lines WHERE network = ?1 AND target = ?2 AND (time < ?3 OR (time = ?3 AND rowid < ?4)) ORDER BY time DESC, rowid DESC LIMIT ?5",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut rows = st.query(params![network, target, time, rowid, limit])?;
        let mut out = Older::default();
        while let Some(r) = rows.next()? {
            let (msgid, ms, kind) = (r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?);
            let (nick, text, own, brk) = (r.get::<_, Option<String>>(3)?, r.get::<_, String>(4)?, r.get::<_, bool>(5)?, r.get::<_, bool>(6)?);
            let (via, reply_to) = (r.get::<_, Option<String>>(7)?, r.get::<_, Option<String>>(8)?);
            // A row this hirc can't read (a later version's kind) is skipped.
            if let (Some(time), Some(kind)) = (DateTime::from_timestamp_millis(ms), kind_of(&kind)) {
                out.lines.push(Line {
                    seq: 0,
                    time,
                    msgid: Some(msgid),
                    kind,
                    nick,
                    text,
                    own,
                    delivery: Delivery::Sent,
                    boundary: false,
                    replayed: true,
                    via,
                    reply_to,
                });
            }
            if brk && reach == Reach::ToBreak {
                out.broke = true;
                break;
            }
        }
        out.lines.reverse();
        Ok(out)
    }

    /// Every target held for `network`: its name as its newest line has
    /// it, and whether it is a query. Sorted by lowercased name.
    pub fn targets(&self, network: &str) -> Result<Vec<(String, bool)>, CacheError> {
        // One scan: with MAX(), SQLite takes the bare columns from the row
        // holding the maximum (the newest line).
        let mut st = self.db.prepare_cached("SELECT name, query, MAX(time) FROM lines WHERE network = ?1 GROUP BY target ORDER BY target")?;
        let rows = st.query_map(params![network], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Up to when discovery of busy targets (TARGETS) is complete for
    /// `network`, if it has ever been.
    pub fn discovered(&self, network: &str) -> Result<Option<DateTime<Utc>>, CacheError> {
        let ms: Option<Option<i64>> = self.db.query_row("SELECT discovered FROM meta WHERE network = ?1", params![network], |r| r.get(0)).optional()?;
        Ok(ms.flatten().and_then(DateTime::from_timestamp_millis))
    }

    /// Whether `network`'s last connection could serve history, if it has
    /// ever connected.
    pub fn history(&self, network: &str) -> Result<Option<bool>, CacheError> {
        Ok(self.db.query_row("SELECT history FROM networks WHERE network = ?1", params![network], |r| r.get(0)).optional()?)
    }

    pub fn set_history(&mut self, network: &str, history: bool) -> Result<(), CacheError> {
        self.db.execute(
            "INSERT INTO networks (network, history) VALUES (?1, ?2) ON CONFLICT (network) DO UPDATE SET history = excluded.history",
            params![network, history],
        )?;
        Ok(())
    }

    pub fn set_discovered(&mut self, network: &str, time: DateTime<Utc>) -> Result<(), CacheError> {
        self.db.execute(
            "INSERT INTO meta (network, discovered) VALUES (?1, ?2) ON CONFLICT (network) DO UPDATE SET discovered = excluded.discovered",
            params![network, time.timestamp_millis()],
        )?;
        Ok(())
    }
}

/// One reaction as the cache holds it (see `Event::Reaction`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reacted {
    pub parent: String,
    pub emoji: String,
    pub nick: String,
    pub on: bool,
    pub time: DateTime<Utc>,
}

/// One run, inside `put`'s transaction.
fn put_run(db: &Connection, e: &Entry) -> Result<usize, CacheError> {
    let Some(name) = e.target.name() else { return Ok(0) };
    let target = name.to_ascii_lowercase();
    let query = matches!(e.target, Target::Query(_));
    let mut lines: Vec<&Line> = e.lines.iter().filter(|l| cacheable(&e.target, l)).collect();
    lines.sort_by_key(|l| l.time);
    let (Some(oldest), Some(newest)) = (lines.first().and_then(|l| l.msgid.clone()), lines.last().and_then(|l| l.msgid.clone())) else { return Ok(0) };
    let mut insert = db.prepare_cached(
        "INSERT OR IGNORE INTO lines (network, target, name, query, msgid, time, kind, nick, text, own, brk, rowid, via, reply_to)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?;
    // Lines tied in time are ordered by rowid. A run from before what is
    // held is stored after it, so it takes rowids below every row's (in
    // its own order), else it would read as newer than the lines it ties
    // with, and paging up from them would skip it. Others get the next.
    let below = if e.older {
        let min: i64 = db.query_row("SELECT coalesce(min(rowid), 1) FROM lines", [], |r| r.get(0))?;
        Some(min.saturating_sub(i64::try_from(lines.len()).unwrap_or(i64::MAX)))
    } else {
        None
    };
    let mut heal = db.prepare_cached("UPDATE lines SET brk = 0 WHERE network = ?1 AND target = ?2 AND msgid = ?3")?;
    let mut held = db.prepare_cached("SELECT 1 FROM lines WHERE network = ?1 AND target = ?2 AND msgid = ?3")?;
    // On the time index: the same line held under the other kind of msgid
    // (made up on a network without chathistory, the server's own on one
    // with). Two lines alike with msgids of one kind are two lines.
    let mut said = db.prepare_cached(
        "SELECT 1 FROM lines WHERE network = ?1 AND target = ?2 AND time = ?3 AND kind = ?4 AND nick IS ?5 AND text = ?6 AND (substr(msgid, 1, 5) = 'hirc:') != ?7",
    )?;
    let (mut n, mut newest_is_new, mut skipped) = (0, false, false);
    for (i, l) in lines.iter().enumerate() {
        let ms = l.time.timestamp_millis();
        // Skipped: the held copy is where it is, and what is around it is
        // not known to be what is around this line (two lines can read the
        // same), so no join is claimed across it: not at the held copy, and
        // not at the next line that goes in.
        if !held.exists(params![e.network, target, l.msgid])?
            && said.exists(params![e.network, target, ms, kind_name(l.kind), l.nick, l.text, l.msgid.as_deref().is_some_and(crate::msgid::is_made_up)])?
        {
            (skipped, newest_is_new) = (true, false);
            continue;
        }
        let rowid = below.map(|b| b + i64::try_from(i).unwrap_or(0));
        let row = params![e.network, target, name, query, l.msgid, ms, kind_name(l.kind), l.nick, l.text, l.own, i == 0 || skipped, rowid, l.via, l.reply_to];
        let new = insert.execute(row)? == 1;
        if !new && i > 0 && !skipped {
            heal.execute(params![e.network, target, l.msgid])?;
        }
        n += usize::from(new);
        newest_is_new = new;
        skipped = false;
    }
    if let Below::Joins(prev) = &e.below
        && neighbour(db, &e.network, &target, &oldest, false)?.as_ref() == Some(prev)
    {
        heal.execute(params![e.network, target, oldest])?;
    }
    let next = neighbour(db, &e.network, &target, &newest, true)?;
    match (&e.above, next) {
        (Above::Leave, _) | (_, None) => {}
        (Above::Joins(id), Some(next)) if *id == next => {
            heal.execute(params![e.network, target, next])?;
        }
        (_, Some(next)) if newest_is_new => {
            db.execute("UPDATE lines SET brk = 1 WHERE network = ?1 AND target = ?2 AND msgid = ?3", params![e.network, target, next])?;
        }
        _ => {}
    }
    Ok(n)
}

/// The msgid of the line held just older (or newer) than the one with
/// `msgid`, in (time, stored order).
fn neighbour(db: &Connection, network: &str, target: &str, msgid: &str, newer: bool) -> Result<Option<String>, CacheError> {
    let sql = if newer {
        "SELECT l.msgid FROM lines a JOIN lines l ON l.network = a.network AND l.target = a.target
         WHERE a.network = ?1 AND a.target = ?2 AND a.msgid = ?3 AND (l.time > a.time OR (l.time = a.time AND l.rowid > a.rowid))
         ORDER BY l.time, l.rowid LIMIT 1"
    } else {
        "SELECT l.msgid FROM lines a JOIN lines l ON l.network = a.network AND l.target = a.target
         WHERE a.network = ?1 AND a.target = ?2 AND a.msgid = ?3 AND (l.time < a.time OR (l.time = a.time AND l.rowid < a.rowid))
         ORDER BY l.time DESC, l.rowid DESC LIMIT 1"
    };
    Ok(db.prepare_cached(sql)?.query_row(params![network, target, msgid], |r| r.get(0)).optional()?)
}

/// Whether `e` says the file itself is no use as a cache, as against a
/// passing failure (busy, full, no permission) that says nothing of it.
fn unusable(e: &CacheError) -> bool {
    match e {
        CacheError::Version(_) => true,
        CacheError::Db(rusqlite::Error::SqliteFailure(f, _)) => matches!(f.code, ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt),
        _ => false,
    }
}

/// Rename the file, and its WAL and index, to a name nothing has.
fn move_aside(path: &Path) -> Result<(), CacheError> {
    let stamp = Utc::now().timestamp_millis();
    let name = |n: u32, suffix: &str| {
        let mut p = path.as_os_str().to_owned();
        p.push(if n == 0 { format!(".bad-{stamp}") } else { format!(".bad-{stamp}-{n}") });
        p.push(suffix);
        PathBuf::from(p)
    };
    let n = (0..).find(|&n| !name(n, "").exists() && !name(n, "-wal").exists() && !name(n, "-shm").exists()).unwrap_or(0);
    for suffix in ["", "-wal", "-shm"] {
        let mut from = path.as_os_str().to_owned();
        from.push(suffix);
        match std::fs::rename(&from, name(n, suffix)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            r => r?,
        }
    }
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// The directory, private to the user from the start.
fn make_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

/// Make the file, if it isn't there, private to the user from the start.
fn create_private(path: &Path) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    match o.open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        r => r.map(drop),
    }
}

/// Mode 0600 on the file and any WAL or index beside it, however they got
/// there.
fn tighten(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    for suffix in ["", "-wal", "-shm"] {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::set_permissions(sidecar(path, suffix), std::fs::Permissions::from_mode(0o600)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            r => r?,
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn kind_name(kind: LineKind) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == kind).map_or("privmsg", |(_, n)| n)
}

fn kind_of(name: &str) -> Option<LineKind> {
    KINDS.iter().find(|(_, n)| *n == name).map(|(k, _)| *k)
}

enum Job {
    Put(Entry),
    React(String, Target, Reacted),
    Discovered(String, DateTime<Utc>),
    History(String, bool),
    /// Answered once every job before it is written.
    Flush(mpsc::Sender<bool>),
}

/// The cache, shared by the network thread (reads) and a writer thread.
#[derive(Clone)]
pub struct Shared {
    cache: Arc<Mutex<Cache>>,
    jobs: mpsc::Sender<Job>,
}

impl Shared {
    /// Open the cache and start its writer. The writer ends once every
    /// `Shared` is dropped.
    pub fn open(path: &Path) -> Result<Shared, CacheError> {
        let cache = Arc::new(Mutex::new(Cache::open(path)?));
        let (jobs, rx) = mpsc::channel::<Job>();
        let writer = cache.clone();
        std::thread::Builder::new().name("hirc-cache".into()).spawn(move || {
            // A write failed since the last flush: the next flush says so.
            let mut failed = false;
            // Lines were lost: whatever is written next may sit past them.
            let mut lost = false;
            while let Ok(job) = rx.recv() {
                // Everything queued so far goes in one transaction.
                let mut puts = Vec::new();
                let mut marks = Vec::new();
                let mut reactions = Vec::new();
                let mut histories = Vec::new();
                let mut done = Vec::new();
                for job in std::iter::once(job).chain(rx.try_iter()) {
                    match job {
                        Job::Put(e) => puts.push(e),
                        Job::React(n, t, r) => reactions.push((n, t, r)),
                        Job::Discovered(n, t) => marks.push((n, t)),
                        Job::History(n, h) => histories.push((n, h)),
                        Job::Flush(tx) => done.push(tx),
                    }
                }
                let mut ok = true;
                {
                    // A panic elsewhere while the lock was held leaves the
                    // database as it was: carry on.
                    let mut c = writer.lock().unwrap_or_else(|p| {
                        tracing::error!("history cache: its lock was poisoned; carrying on");
                        writer.clear_poison();
                        p.into_inner()
                    });
                    if lost {
                        for e in &mut puts {
                            e.below = Below::Breaks;
                        }
                    }
                    if !puts.is_empty() {
                        match c.put(&puts) {
                            Ok(_) => lost = false,
                            Err(e) => {
                                tracing::warn!("history cache: {e}");
                                (ok, lost) = (false, true);
                            }
                        }
                    }
                    for (network, target, r) in reactions {
                        if let Err(e) = c.react(&network, &target, &r) {
                            tracing::warn!("history cache: {e}");
                            ok = false;
                        }
                    }
                    for (network, time) in marks {
                        if let Err(e) = c.set_discovered(&network, time) {
                            tracing::warn!("history cache: {e}");
                            ok = false;
                        }
                    }
                    for (network, history) in histories {
                        if let Err(e) = c.set_history(&network, history) {
                            tracing::warn!("history cache: {e}");
                            ok = false;
                        }
                    }
                }
                failed |= !ok;
                if !done.is_empty() {
                    for tx in done {
                        let _ = tx.send(!failed);
                    }
                    failed = false;
                }
            }
        })?;
        Ok(Shared { cache, jobs })
    }

    /// Queue lines to be written. Never waits on the disk, and never blocks:
    /// the queue is unbounded. If writing a batch fails, the whole batch is
    /// dropped (and logged); the lines are not retried, and the oldest line
    /// of each run in the next batch written is a break.
    pub fn write(&self, entry: Entry) {
        let _ = self.jobs.send(Job::Put(entry));
    }

    /// Queue the time up to which discovery is complete for `network`.
    pub fn write_discovered(&self, network: &str, time: DateTime<Utc>) {
        let _ = self.jobs.send(Job::Discovered(network.to_owned(), time));
    }

    /// Queue a reaction (or its taking back) to be written. Like `write`,
    /// it never waits on the disk.
    pub fn write_reaction(&self, network: &str, target: &Target, r: Reacted) {
        let _ = self.jobs.send(Job::React(network.to_owned(), target.clone(), r));
    }

    /// Queue whether `network` can serve history now.
    pub fn write_history(&self, network: &str, history: bool) {
        let _ = self.jobs.send(Job::History(network.to_owned(), history));
    }

    /// Wait until everything queued so far is written. True if it all
    /// went in; false if any write failed since the last flush, or the
    /// writer is gone.
    ///
    /// This blocks on the disk: never call it on the UI thread. It is to be
    /// called at quit, from a thread that may wait.
    pub fn flush(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        self.jobs.send(Job::Flush(tx)).is_ok() && rx.recv().unwrap_or(false)
    }

    /// Read on the caller's thread (the network thread). A panic while
    /// the lock was held left the database as it was, so reads carry on.
    pub fn read<T>(&self, f: impl FnOnce(&Cache) -> Result<T, CacheError>) -> Result<T, CacheError> {
        let c = self.cache.lock().unwrap_or_else(|p| {
            self.cache.clear_poison();
            p.into_inner()
        });
        f(&c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1_800_000_000_000 + secs * 1000 + 250).unwrap()
    }

    fn line(id: Option<&str>, secs: i64, text: &str) -> Line {
        Line {
            seq: 7,
            time: at(secs),
            msgid: id.map(Into::into),
            kind: LineKind::Action,
            nick: Some("bob".into()),
            text: text.into(),
            own: false,
            delivery: Delivery::Sent,
            boundary: false,
            replayed: false,
            via: None,
            reply_to: None,
        }
    }

    fn entry(target: Target, lines: Vec<Line>) -> Entry {
        Entry { network: "net".into(), target, lines, below: Below::Breaks, above: Above::Leave, older: false }
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    fn brk(path: &Path, msgid: &str) -> bool {
        Connection::open(path).unwrap().query_row("SELECT brk FROM lines WHERE msgid = ?1", [msgid], |r| r.get(0)).unwrap()
    }

    fn chan(name: &str) -> Target {
        Target::Channel(name.into())
    }

    #[test]
    fn the_path_follows_xdg_data_home_else_home() {
        assert_eq!(path_from(Some("/data".into()), Some("/home/me".into())), PathBuf::from("/data/hirc/history.db"));
        assert_eq!(path_from(Some("".into()), Some("/home/me".into())), PathBuf::from("/home/me/.local/share/hirc/history.db"));
        assert_eq!(path_from(Some("relative".into()), Some("/home/me".into())), PathBuf::from("/home/me/.local/share/hirc/history.db"));
        assert_eq!(path_from(None, Some("/home/me".into())), PathBuf::from("/home/me/.local/share/hirc/history.db"));
    }

    #[test]
    fn lines_come_back_whole_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        let mut own = line(Some("m2"), 2, "two");
        own.own = true;
        own.kind = LineKind::Privmsg;
        assert_eq!(c.put(&[entry(chan("#A"), vec![line(Some("m3"), 3, "three"), own.clone(), line(Some("m1"), 1, "one")])]).unwrap(), 3);
        // Again: nothing new.
        assert_eq!(c.put(&[entry(chan("#a"), vec![line(Some("m1"), 1, "one")])]).unwrap(), 0);
        let got = c.latest("net", "#a", 2).unwrap();
        assert_eq!(got.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["two", "three"]);
        assert_eq!(got[0], Line { seq: 0, replayed: true, ..own });
        assert_eq!(got[1].time, at(3));
        assert_eq!(got[1].kind, LineKind::Action);
        // Strictly before: the anchor itself never comes back. The run's
        // oldest line is a break, so paging stops there.
        let older = c.before("net", "#A", "m2", 10, Reach::ToBreak).unwrap();
        assert_eq!((texts(&older.lines), older.broke), (vec!["one"], true));
        assert_eq!(c.before("net", "#a", "m1", 10, Reach::ToBreak).unwrap(), Older { lines: Vec::new(), broke: true });
        assert!(c.latest("other", "#a", 10).unwrap().is_empty());
    }

    #[test]
    fn only_the_servers_lines_and_never_a_services_query() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        let mut pending = line(Some("p"), 1, "pending");
        pending.delivery = Delivery::Pending;
        let mut boundary = line(Some("b"), 1, "Disconnected");
        boundary.boundary = true;
        let n = c
            .put(&[
                entry(chan("#a"), vec![line(None, 1, "no msgid"), pending, boundary, line(Some("ok"), 1, "kept")]),
                entry(Target::Server, vec![line(Some("s"), 1, "motd")]),
                entry(Target::Query("NickServ".into()), vec![line(Some("ns"), 1, "IDENTIFY hunter2")]),
                entry(Target::Query("ChanServ".into()), vec![line(Some("cs"), 1, "hi")]),
            ])
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(c.targets("net").unwrap(), [("#a".to_owned(), false)]);
        assert!(is_service("NICKSERV") && is_service("HistServ") && !is_service("bob"));
    }

    #[test]
    fn targets_keep_their_shown_name_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        c.put(&[entry(chan("#Rust"), vec![line(Some("r"), 1, "x")]), entry(Target::Query("Bob".into()), vec![line(Some("q"), 1, "y")])]).unwrap();
        assert_eq!(c.targets("net").unwrap(), [("#Rust".to_owned(), false), ("Bob".to_owned(), true)]);
    }

    #[test]
    fn a_broken_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, b"this is not a database, it is a file full of words").unwrap();
        let mut c = Cache::open(&path).unwrap();
        assert_eq!(c.put(&[entry(chan("#a"), vec![line(Some("m"), 1, "x")])]).unwrap(), 1);
        let aside: Vec<String> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.contains(".bad-")).collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        let kept = std::fs::read(dir.path().join(&aside[0])).unwrap();
        assert_eq!(kept, b"this is not a database, it is a file full of words");
    }

    #[test]
    fn a_second_bad_file_never_replaces_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        for i in 0..3 {
            std::fs::write(&path, format!("garbage number {i} which is not a database")).unwrap();
            drop(Cache::open(&path).unwrap());
        }
        let n = std::fs::read_dir(dir.path()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".bad-")).count();
        assert_eq!(n, 3);
    }

    #[test]
    fn the_sidecars_move_with_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, b"this is not a database, it is a file full of words").unwrap();
        std::fs::write(dir.path().join("history.db-wal"), b"wal bytes").unwrap();
        std::fs::write(dir.path().join("history.db-shm"), b"shm bytes").unwrap();
        drop(Cache::open(&path).unwrap());
        let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        let aside = names.iter().find(|n| n.contains(".bad-") && !n.ends_with("-wal") && !n.ends_with("-shm")).unwrap_or_else(|| panic!("{names:?}"));
        assert_eq!(std::fs::read(dir.path().join(format!("{aside}-wal"))).unwrap(), b"wal bytes");
        // SQLite rewrites the index as it looks; it need only be kept.
        assert!(dir.path().join(format!("{aside}-shm")).exists());
    }

    #[test]
    fn a_locked_file_is_an_error_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let other = Connection::open(&path).unwrap();
        other.busy_timeout(std::time::Duration::ZERO).unwrap();
        other.execute_batch("CREATE TABLE t (x); BEGIN EXCLUSIVE;").unwrap();
        assert!(Cache::open(&path).is_err());
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".bad-")));
        other.execute_batch("COMMIT").unwrap();
        assert!(path.exists());
    }

    #[test]
    fn another_version_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch("PRAGMA user_version = 99; CREATE TABLE t (x);").unwrap();
        }
        let mut c = Cache::open(&path).unwrap();
        assert_eq!(c.put(&[entry(chan("#a"), vec![line(Some("m"), 1, "x")])]).unwrap(), 1);
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".bad-")));
    }

    #[test]
    fn lines_sharing_a_timestamp_are_not_skipped_when_paging() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        c.put(&[entry(chan("#a"), vec![line(Some("a"), 1, "a"), line(Some("b"), 1, "b"), line(Some("c"), 1, "c"), line(Some("d"), 2, "d")])]).unwrap();
        let mut got = Vec::new();
        let mut page = c.latest("net", "#a", 1).unwrap();
        while let Some(l) = page.pop() {
            got.push(l.text.clone());
            page = c.before("net", "#a", l.msgid.as_deref().unwrap(), 1, Reach::ToBreak).unwrap().lines;
        }
        assert_eq!(got, ["d", "c", "b", "a"]);
        // An anchor the cache doesn't hold: it can't say what is before it.
        assert_eq!(c.before("net", "#a", "nope", 10, Reach::ToBreak).unwrap(), Older { lines: Vec::new(), broke: true });
    }

    #[test]
    fn a_found_anchor_ties_on_its_own_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        c.put(&[entry(chan("#a"), vec![line(Some("a"), 1, "a"), line(Some("b"), 1, "b"), line(Some("c"), 1, "c")])]).unwrap();
        // Ties go by stored order: c itself never comes back.
        let got = c.before("net", "#a", "c", 10, Reach::ToBreak).unwrap();
        assert_eq!(texts(&got.lines), ["a", "b"]);
    }

    #[test]
    fn a_read_after_a_panic_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Shared::open(&dir.path().join(FILE)).unwrap();
        let s2 = shared.clone();
        let _ = std::thread::spawn(move || {
            let _ = s2.read(|_| -> Result<(), CacheError> { panic!("poison") });
        })
        .join();
        assert!(shared.read(|c| c.latest("net", "#a", 10)).unwrap().is_empty());
    }

    #[test]
    fn a_failed_write_since_the_last_flush_fails_the_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let shared = Shared::open(&path).unwrap();
        Connection::open(&path).unwrap().execute_batch("DROP TABLE meta").unwrap();
        shared.write_discovered("net", at(1));
        assert!(!shared.flush());
        // Said once: the next flush is about what came after.
        shared.write(entry(chan("#a"), vec![line(Some("n"), 2, "y")]));
        assert!(shared.flush());
    }

    #[test]
    fn a_run_breaks_at_its_oldest_unless_it_joins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut c = Cache::open(&path).unwrap();
        c.put(&[entry(chan("#a"), vec![line(Some("a1"), 1, "a1"), line(Some("a2"), 2, "a2")])]).unwrap();
        // Follows a2: no break.
        c.put(&[Entry { below: Below::Joins("a2".into()), ..entry(chan("#a"), vec![line(Some("a3"), 3, "a3")]) }]).unwrap();
        // Says it follows a1, but a2 is between: a break.
        c.put(&[Entry { below: Below::Joins("a1".into()), ..entry(chan("#a"), vec![line(Some("a4"), 4, "a4")]) }]).unwrap();
        assert_eq!([brk(&path, "a1"), brk(&path, "a2"), brk(&path, "a3"), brk(&path, "a4")], [true, false, false, true]);
        // Before a break: nothing the cache can vouch for.
        assert_eq!(c.before("net", "#a", "a4", 10, Reach::ToBreak).unwrap(), Older { lines: Vec::new(), broke: true });
        // Down to and including the first break, never past it.
        let got = c.before("net", "#a", "a3", 10, Reach::ToBreak).unwrap();
        assert_eq!((texts(&got.lines), got.broke), (vec!["a1", "a2"], true));
        let got = c.before("net", "#a", "a3", 1, Reach::ToBreak).unwrap();
        assert_eq!((texts(&got.lines), got.broke), (vec!["a2"], false));
        // The newest page stops at a break too.
        assert_eq!(texts(&c.latest("net", "#a", 10).unwrap()), ["a4"]);
    }

    #[test]
    fn a_page_heals_the_breaks_it_spans_and_an_island_breaks_what_is_above() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut c = Cache::open(&path).unwrap();
        c.put(&[entry(chan("#a"), vec![line(Some("a5"), 5, "a5"), line(Some("a6"), 6, "a6")])]).unwrap();
        c.put(&[entry(chan("#a"), vec![line(Some("a8"), 8, "a8")])]).unwrap();
        assert!(brk(&path, "a8"));
        // A page holding a6..a8 says a8 follows on: healed. Its oldest
        // line was held already: left as it is.
        c.put(&[entry(chan("#a"), vec![line(Some("a6"), 6, "a6"), line(Some("a7"), 7, "a7"), line(Some("a8"), 8, "a8")])]).unwrap();
        assert_eq!([brk(&path, "a5"), brk(&path, "a6"), brk(&path, "a8")], [true, false, false]);
        // An island below a5 that isn't known to reach it: a5 stays a break.
        c.put(&[Entry { above: Above::Breaks, ..entry(chan("#a"), vec![line(Some("a1"), 1, "a1")]) }]).unwrap();
        assert!(brk(&path, "a5") && brk(&path, "a1"));
        // A page before a5 that reaches it heals it.
        c.put(&[Entry { above: Above::Joins("a5".into()), ..entry(chan("#a"), vec![line(Some("a3"), 3, "a3"), line(Some("a4"), 4, "a4")]) }]).unwrap();
        assert!(!brk(&path, "a5") && brk(&path, "a3"));
        // An island inside a run breaks the line above it.
        c.put(&[Entry { above: Above::Breaks, ..entry(chan("#a"), vec![line(Some("x"), 7, "x")]) }]).unwrap();
        assert!(brk(&path, "a8"), "a7 and a8 were stored before x, so a8 is above it");
        // Joins naming a line that isn't next above it: taken as a break.
        assert!(!brk(&path, "a4"));
        c.put(&[Entry { above: Above::Joins("a8".into()), ..entry(chan("#a"), vec![line(Some("y"), 3, "y")]) }]).unwrap();
        assert!(brk(&path, "a4"));
    }

    #[test]
    fn breaks_are_read_past_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        c.put(&[entry(chan("#a"), vec![line(Some("a1"), 1, "a1"), line(Some("a2"), 2, "a2")])]).unwrap();
        c.put(&[entry(chan("#a"), vec![line(Some("a3"), 3, "a3"), line(Some("a4"), 4, "a4")])]).unwrap();
        assert_eq!(texts(&c.latest_in("net", "#a", 10, Reach::ToBreak).unwrap()), ["a3", "a4"]);
        assert_eq!(texts(&c.latest_in("net", "#a", 10, Reach::All).unwrap()), ["a1", "a2", "a3", "a4"]);
        assert_eq!(c.before("net", "#a", "a3", 10, Reach::ToBreak).unwrap(), Older { lines: Vec::new(), broke: true });
        let got = c.before("net", "#a", "a3", 10, Reach::All).unwrap();
        assert_eq!((texts(&got.lines), got.broke), (vec!["a1", "a2"], false));
        let got = c.before("net", "#a", "a4", 2, Reach::All).unwrap();
        assert_eq!((texts(&got.lines), got.broke), (vec!["a2", "a3"], false));
    }

    #[test]
    fn whether_a_network_has_history_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let shared = Shared::open(&path).unwrap();
        assert_eq!(shared.read(|c| c.history("net")).unwrap(), None);
        shared.write_history("net", false);
        shared.write_history("other", true);
        assert!(shared.flush());
        drop(shared);
        let c = Cache::open(&path).unwrap();
        assert_eq!((c.history("net").unwrap(), c.history("other").unwrap()), (Some(false), Some(true)));
    }

    #[test]
    fn a_failed_batch_breaks_the_next_one_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let shared = Shared::open(&path).unwrap();
        shared.write(entry(chan("#a"), vec![line(Some("a1"), 1, "a1"), line(Some("a2"), 2, "a2")]));
        assert!(shared.flush());
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TRIGGER no BEFORE INSERT ON lines WHEN NEW.msgid = 'bad' BEGIN SELECT RAISE(ABORT, 'no'); END;")
            .unwrap();
        shared.write(Entry { below: Below::Joins("a2".into()), ..entry(chan("#a"), vec![line(Some("bad"), 3, "bad")]) });
        assert!(!shared.flush());
        // It says it follows a2, and a2 is the line below it; but what was
        // lost may have been between them.
        shared.write(Entry { below: Below::Joins("a2".into()), ..entry(chan("#a"), vec![line(Some("a4"), 4, "a4")]) });
        assert!(shared.flush());
        assert!(brk(&path, "a4"));
        // So older lines are the server's to give.
        assert_eq!(shared.read(|c| c.before("net", "#a", "a4", 10, Reach::ToBreak)).unwrap(), Older { lines: Vec::new(), broke: true });
        // Once one batch goes in, runs join again.
        shared.write(Entry { below: Below::Joins("a4".into()), ..entry(chan("#a"), vec![line(Some("a5"), 5, "a5")]) });
        assert!(shared.flush());
        assert!(!brk(&path, "a5"));
    }

    #[test]
    fn version_1_gains_breaks_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE lines (network TEXT NOT NULL, target TEXT NOT NULL, name TEXT NOT NULL, query INTEGER NOT NULL, msgid TEXT NOT NULL,
                     time INTEGER NOT NULL, kind TEXT NOT NULL, nick TEXT, text TEXT NOT NULL, own INTEGER NOT NULL, PRIMARY KEY (network, target, msgid));
                 CREATE INDEX lines_by_time ON lines (network, target, time);
                 CREATE TABLE meta (network TEXT PRIMARY KEY, discovered INTEGER);
                 INSERT INTO lines VALUES ('net', '#a', '#a', 0, 'o1', 1800000001000, 'privmsg', 'bob', 'one', 0);
                 INSERT INTO lines VALUES ('net', '#a', '#a', 0, 'o2', 1800000002000, 'privmsg', 'bob', 'two', 0);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let c = Cache::open(&path).unwrap();
        assert_eq!(texts(&c.latest("net", "#a", 10).unwrap()), ["one", "two"]);
        // What came before can't be vouched for: its oldest is a break.
        assert!(brk(&path, "o1") && !brk(&path, "o2"));
        let v: i64 = Connection::open(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, VERSION);
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains(".bad-")));
        drop(c);
        // And opens again as version 2.
        assert_eq!(Cache::open(&path).unwrap().latest("net", "#a", 10).unwrap().len(), 2);
    }

    #[test]
    fn version_2_gains_bridges_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE lines (network TEXT NOT NULL, target TEXT NOT NULL, name TEXT NOT NULL, query INTEGER NOT NULL, msgid TEXT NOT NULL,
                     time INTEGER NOT NULL, kind TEXT NOT NULL, nick TEXT, text TEXT NOT NULL, own INTEGER NOT NULL,
                     brk INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (network, target, msgid));
                 INSERT INTO lines VALUES ('net', '#a', '#a', 0, 'o1', 1800000001000, 'privmsg', 'bob', 'one', 0, 1);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let c = Cache::open(&path).unwrap();
        let got = c.latest("net", "#a", 10).unwrap();
        assert_eq!((texts(&got).as_slice(), got[0].via.as_deref()), (["one"].as_slice(), None));
        let v: i64 = Connection::open(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, VERSION);
    }

    fn reacted(parent: &str, emoji: &str, nick: &str, on: bool, secs: i64) -> Reacted {
        Reacted { parent: parent.into(), emoji: emoji.into(), nick: nick.into(), on, time: at(secs) }
    }

    #[test]
    fn reactions_are_kept_newest_change_winning() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::open(&dir.path().join(FILE)).unwrap();
        c.react("net", &chan("#a"), &reacted("m1", "👍", "bob", true, 10)).unwrap();
        c.react("net", &chan("#a"), &reacted("m1", "🎉", "carol", true, 11)).unwrap();
        c.react("net", &chan("#a"), &reacted("m1", "👍", "Bob", false, 12)).unwrap();
        // An older copy from history doesn't bring it back.
        c.react("net", &chan("#a"), &reacted("m1", "👍", "bob", true, 9)).unwrap();
        let got = c.reactions("net", "#A", &["m1", "m2"]).unwrap();
        assert_eq!(got, [reacted("m1", "🎉", "carol", true, 11), reacted("m1", "👍", "Bob", false, 12)]);
    }

    #[test]
    fn nothing_from_a_services_query_and_nothing_to_a_made_up_id() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::open(&dir.path().join(FILE)).unwrap();
        c.react("net", &Target::Query("NickServ".into()), &reacted("m1", "👍", "bob", true, 10)).unwrap();
        c.react("net", &chan("#a"), &reacted("hirc:0123456789abcdef", "👍", "bob", true, 10)).unwrap();
        assert!(c.reactions("net", "nickserv", &["m1"]).unwrap().is_empty());
        assert!(c.reactions("net", "#a", &["hirc:0123456789abcdef"]).unwrap().is_empty());
    }

    #[test]
    fn a_reply_keeps_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        let reply = Line { reply_to: Some("m1".into()), ..line(Some("m2"), 2, "the usual") };
        c.put(&[entry(chan("#a"), vec![line(Some("m1"), 1, "lunch?"), reply])]).unwrap();
        let got = c.latest("net", "#a", 10).unwrap();
        assert_eq!(got.iter().map(|l| l.reply_to.as_deref()).collect::<Vec<_>>(), [None, Some("m1")]);
    }

    #[test]
    fn version_3_gains_replies_and_reactions_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE lines (network TEXT NOT NULL, target TEXT NOT NULL, name TEXT NOT NULL, query INTEGER NOT NULL, msgid TEXT NOT NULL,
                     time INTEGER NOT NULL, kind TEXT NOT NULL, nick TEXT, text TEXT NOT NULL, own INTEGER NOT NULL,
                     brk INTEGER NOT NULL DEFAULT 0, via TEXT, PRIMARY KEY (network, target, msgid));
                 CREATE TABLE meta (network TEXT PRIMARY KEY, discovered INTEGER);
                 CREATE TABLE networks (network TEXT PRIMARY KEY, history INTEGER NOT NULL);
                 INSERT INTO lines VALUES ('net', '#a', '#a', 0, 'o1', 1800000001000, 'privmsg', 'bob', 'one', 0, 1, NULL);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let c = Cache::open(&path).unwrap();
        let got = c.latest("net", "#a", 10).unwrap();
        assert_eq!((texts(&got).as_slice(), got[0].reply_to.as_deref()), (["one"].as_slice(), None));
        let v: i64 = Connection::open(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, VERSION);
        assert_eq!(VERSION, 4);
        c.react("net", &chan("#a"), &reacted("o1", "👍", "bob", true, 10)).unwrap();
        assert_eq!(c.reactions("net", "#a", &["o1"]).unwrap(), [reacted("o1", "👍", "bob", true, 10)]);
    }

    #[test]
    fn a_relayed_line_keeps_its_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        let relayed = Line { via: Some("IrcBot".into()), ..line(Some("r1"), 1, "hi") };
        c.put(&[entry(chan("#a"), vec![relayed, line(Some("r2"), 2, "yo")])]).unwrap();
        let got = c.latest("net", "#a", 10).unwrap();
        assert_eq!(got.iter().map(|l| l.via.as_deref()).collect::<Vec<_>>(), [Some("IrcBot"), None]);
    }

    #[test]
    fn a_target_shows_its_newest_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        c.put(&[entry(chan("#zed"), vec![line(Some("1"), 1, "x")]), entry(chan("#Zed"), vec![line(Some("2"), 2, "x")])]).unwrap();
        c.put(&[entry(chan("#ZED"), vec![line(Some("0"), 0, "x")])]).unwrap();
        assert_eq!(c.targets("net").unwrap(), [("#Zed".to_owned(), false)]);
    }

    #[test]
    fn no_services_line_is_ever_kept_from_any_run() {
        // Every write to the cache is a run through `put`: none of these
        // targets' lines go in, however the run says it meets what is held.
        let dir = tempfile::tempdir().unwrap();
        let mut c = Cache::open(&dir.path().join(FILE)).unwrap();
        let names = ["NickServ", "BouncerServ", "bouncerserv@soju", "*status", "*controlpanel"];
        let runs: Vec<Entry> = names
            .iter()
            .enumerate()
            .flat_map(|(i, n)| {
                let id = format!("s{i}");
                let lines = vec![line(Some(&id), 1, "sasl set-plain alice soju-test-only")];
                [
                    entry(Target::Query((*n).into()), lines.clone()),
                    Entry { below: Below::Joins("x".into()), above: Above::Joins("y".into()), older: true, ..entry(Target::Query((*n).into()), lines) },
                ]
            })
            .collect();
        assert_eq!(c.put(&runs).unwrap(), 0);
        // Nor one taken for a channel.
        assert_eq!(c.put(&[entry(chan("*status"), vec![line(Some("c"), 1, "x")])]).unwrap(), 0);
        assert!(c.targets("net").unwrap().is_empty());
    }

    /// `l` under its made-up id in `target`, as a ZNC era of the network
    /// cached it.
    fn made_up(target: &Target, l: Line) -> Line {
        Line { msgid: Some(crate::msgid::made_up(target, &l)), ..l }
    }

    fn rows(path: &Path) -> i64 {
        Connection::open(path).unwrap().query_row("SELECT count(*) FROM lines", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_line_held_under_another_msgid_is_not_kept_twice() {
        // The network's history source changes: ZNC (made-up ids), then
        // soju or Ergo (theirs), then ZNC again.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut c = Cache::open(&path).unwrap();
        let a = chan("#a");
        assert_eq!(c.put(&[entry(a.clone(), vec![made_up(&a, line(None, 1, "one")), made_up(&a, line(None, 2, "two"))])]).unwrap(), 2);
        let real = vec![line(Some("r1"), 1, "one"), line(Some("r2"), 2, "two"), line(Some("r3"), 3, "three")];
        assert_eq!(c.put(&[Entry { older: true, ..entry(a.clone(), real) }]).unwrap(), 1);
        let back = vec![made_up(&a, line(None, 2, "two")), made_up(&a, line(None, 3, "three")), made_up(&a, line(None, 4, "four"))];
        assert_eq!(c.put(&[entry(a.clone(), back)]).unwrap(), 1);
        assert_eq!(rows(&path), 4);
        assert_eq!(texts(&c.latest_in("net", "#a", 10, Reach::All).unwrap()), ["one", "two", "three", "four"]);
        // Another time, nick or kind is another line; so is another target.
        let mut notice = line(Some("n"), 4, "four");
        notice.kind = LineKind::Notice;
        let carol = Line { nick: Some("carol".into()), ..line(Some("c"), 4, "four") };
        assert_eq!(
            c.put(&[entry(a.clone(), vec![line(Some("l"), 5, "four"), notice, carol]), entry(chan("#b"), vec![line(Some("b"), 4, "four")])]).unwrap(),
            4
        );
    }

    #[test]
    fn no_join_is_claimed_across_a_line_held_under_another_msgid() {
        // `two` is held under its made-up id, an island. A run through it
        // can't vouch for what lies around the held copy: it could be
        // another line that only reads the same.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut c = Cache::open(&path).unwrap();
        let a = chan("#a");
        let two = made_up(&a, line(None, 2, "two"));
        let held = two.msgid.clone().unwrap();
        c.put(&[entry(a.clone(), vec![line(Some("r0"), 0, "zero")]), entry(a.clone(), vec![two])]).unwrap();
        assert!(brk(&path, &held));
        let run = vec![line(Some("r1"), 1, "one"), line(Some("r2"), 2, "two"), line(Some("r3"), 3, "three")];
        assert_eq!(c.put(&[Entry { below: Below::Joins("r0".into()), ..entry(a.clone(), run) }]).unwrap(), 2);
        assert!(!brk(&path, "r1"), "r1 joins r0, as said");
        assert!(brk(&path, &held), "the held copy is not healed");
        assert!(brk(&path, "r3"), "the line after the skip follows on from nothing known");
        // A run whose oldest line is held under another id: no join there.
        c.put(&[entry(a.clone(), vec![made_up(&a, line(None, 5, "five"))])]).unwrap();
        let run = vec![line(Some("q5"), 5, "five"), line(Some("q6"), 6, "six")];
        c.put(&[Entry { below: Below::Joins("r3".into()), ..entry(a.clone(), run) }]).unwrap();
        assert!(!brk(&path, "r1") && brk(&path, "q6"));
        // Two lines alike, both with the server's ids, are two lines.
        let run = vec![line(Some("r1b"), 1, "one")];
        assert_eq!(c.put(&[entry(a.clone(), run)]).unwrap(), 1);
        // Above: a page before r3 whose newest line is held under another
        // id doesn't heal r3.
        let run = vec![line(Some("p2"), 2, "two")];
        c.put(&[Entry { above: Above::Joins("r3".into()), older: true, ..entry(a.clone(), run) }]).unwrap();
        assert!(brk(&path, "r3"));
    }

    #[test]
    fn a_service_with_a_host_is_still_a_service() {
        let mut l = line(Some("ns"), 1, "IDENTIFY hunter2");
        l.own = true;
        assert!(!cacheable(&Target::Query("NickServ@services.libera.chat".into()), &l));
        assert!(cacheable(&Target::Query("bob".into()), &l));
    }

    #[test]
    fn a_poisoned_lock_does_not_stop_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let shared = Shared::open(&path).unwrap();
        let s2 = shared.clone();
        let _ = std::thread::spawn(move || {
            let _ = s2.read(|_| -> Result<(), CacheError> { panic!("poison") });
        })
        .join();
        shared.write(entry(chan("#a"), vec![line(Some("m"), 1, "x")]));
        assert!(shared.flush());
        assert_eq!(Cache::open(&path).unwrap().latest("net", "#a", 10).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_the_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(FILE);
        Cache::open(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    }

    /// The new file is made 0600 (`create_private`) before SQLite opens it.
    /// A test sees only the mode after `prepare`'s `tighten`, which is 0600
    /// either way, so it can't tell the two apart (that would need the
    /// process umask, which tests share); it guards the end state.
    #[cfg(unix)]
    #[test]
    fn the_file_started_after_a_move_aside_is_the_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, b"this is not a database, it is a file full of words").unwrap();
        drop(Cache::open(&path).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_file_and_its_sidecars_are_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let c = Cache::open(&path).unwrap();
        let wal = dir.path().join("history.db-wal");
        assert!(wal.exists());
        for p in [&path, &wal] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let _keep = c;
        let _c2 = Cache::open(&path).unwrap();
        for p in [&path, &wal] {
            assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600, "{p:?}");
        }
    }

    #[test]
    fn writes_go_through_the_writer_thread() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Shared::open(&dir.path().join(FILE)).unwrap();
        for i in 0..50 {
            let below = if i == 0 { Below::Breaks } else { Below::Joins(format!("m{}", i - 1)) };
            shared.write(Entry { below, ..entry(chan("#a"), vec![line(Some(&format!("m{i}")), i, "x")]) });
        }
        assert!(shared.flush());
        assert_eq!(shared.read(|c| c.latest("net", "#a", 100)).unwrap().len(), 50);
    }

    #[test]
    fn the_discovery_time_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let shared = Shared::open(&path).unwrap();
        assert_eq!(shared.read(|c| c.discovered("net")).unwrap(), None);
        shared.write_discovered("net", at(5));
        shared.write_discovered("net", at(9));
        shared.write_discovered("other", at(1));
        shared.flush();
        drop(shared);
        let c = Cache::open(&path).unwrap();
        assert_eq!(c.discovered("net").unwrap(), Some(at(9)));
        assert_eq!(c.discovered("other").unwrap(), Some(at(1)));
        assert_eq!(c.discovered("none").unwrap(), None);
    }

    /// A page before what is held, stored after it, in the millisecond of
    /// the line it was asked before: paging up from that line finds it,
    /// once, and it meets that line.
    #[test]
    fn an_older_run_in_the_anchors_millisecond_is_neither_skipped_nor_doubled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut c = Cache::open(&path).unwrap();
        c.put(&[entry(chan("#a"), vec![line(Some("n1"), 5, "n1"), line(Some("n2"), 5, "n2")])]).unwrap();
        let before = Entry { above: Above::Joins("n1".into()), older: true, ..entry(chan("#a"), vec![line(Some("o1"), 5, "o1"), line(Some("o2"), 5, "o2")]) };
        c.put(&[before]).unwrap();
        assert_eq!([brk(&path, "o1"), brk(&path, "o2"), brk(&path, "n1")], [true, false, false]);
        let older = c.before("net", "#a", "n1", 10, Reach::ToBreak).unwrap();
        assert_eq!((texts(&older.lines), older.broke), (vec!["o1", "o2"], true));
        assert_eq!(texts(&c.before("net", "#a", "n2", 2, Reach::All).unwrap().lines), ["o2", "n1"]);
        assert_eq!(texts(&c.before("net", "#a", "o2", 10, Reach::All).unwrap().lines), ["o1"]);
        assert!(c.before("net", "#a", "o1", 10, Reach::All).unwrap().lines.is_empty());
        assert_eq!(texts(&c.latest_in("net", "#a", 10, Reach::All).unwrap()), ["o1", "o2", "n1", "n2"]);
    }
}
