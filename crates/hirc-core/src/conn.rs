//! Sockets. One thread runs a tokio runtime with a task per server. Each
//! task connects, feeds lines through its `Session`, sends events to the
//! UI, and reconnects with backoff. The UI never waits on any of it.

use crate::cache::{Reach, Reacted, Shared};
use crate::config::{self, Config, ConfigError, Sasl, Server};
use crate::event::{Command, ConnStatus, Event, Target};
use crate::history::{self, Anchor, Book};
use crate::irccloud;
use crate::isupport::Isupport;
use crate::proto::Message;
use crate::services;
use crate::session::{Output, SaslPlain, Session, SessionConfig};
use crate::store::{Delivery, Line, LineKind};
use crate::wire::LineReader;
use chrono::{DateTime, Utc};
use rustls::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls_platform_verifier::ConfigVerifierExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_rustls::TlsConnector;

type Error = Box<dyn std::error::Error + Send + Sync>;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Send a PING after this long without hearing from the server
/// (`ping_time`)...
const PING_TIME: Duration = Duration::from_secs(120);
/// ...and give up on the connection this long after that (`ping_timeout`).
const PING_TIMEOUT: Duration = Duration::from_secs(180);
/// How often, at most, a quiet connection is looked at.
const IDLE_CHECK: Duration = Duration::from_secs(30);

/// When a quiet connection is pinged, and when it is given up on, by how
/// long since the server last said anything; and how often to look.
fn idle_limits(server: &Server) -> (Duration, Duration, Duration) {
    let secs = |s: Option<u64>, default| s.filter(|&s| s > 0).map_or(default, Duration::from_secs);
    let (ping, wait) = (secs(server.ping_time, PING_TIME), secs(server.ping_timeout, PING_TIMEOUT));
    (ping, ping + wait, IDLE_CHECK.min(ping.min(wait) / 2))
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// The history cache's writer, for the app to wait on as it quits.
#[derive(Clone)]
pub struct Flush(Arc<OnceLock<Option<Shared>>>);

impl Flush {
    /// Wait until every line queued for the cache so far is written. True
    /// if all went in, or there is no cache. It blocks on the disk (and on
    /// the cache opening): never call it on the UI thread.
    pub fn wait(&self) -> bool {
        self.0.wait().as_ref().is_none_or(Shared::flush)
    }
}

/// Starts the network thread. Send `Command`s in; `Event`s come out.
/// `cache` is where the history cache lives (`cache::path()`); `None` runs
/// without one. `Flush` is for writing out what is queued, at quit.
pub fn spawn(config: Config, cache: Option<PathBuf>) -> (UnboundedSender<Command>, UnboundedReceiver<Event>, Flush) {
    let (cmd_tx, mut cmd_rx) = unbounded_channel::<Command>();
    let (ev_tx, ev_rx) = unbounded_channel::<Event>();
    let flush = Flush(Arc::new(OnceLock::new()));
    let opened = flush.0.clone();
    std::thread::Builder::new()
        .name("hirc-net".into())
        .spawn(move || {
            // Opened here, never on the caller's (the UI's) thread.
            let shared = match cache.as_deref().map(|p| (p, Shared::open(p))) {
                Some((_, Ok(shared))) => Some(shared),
                Some((path, Err(e))) => {
                    let text = format!("No history cache ({}): {e}", path.display());
                    for name in config.servers.keys() {
                        let _ = ev_tx.send(Event::Line { network: name.clone(), target: Target::Server, line: note(text.clone()) });
                    }
                    None
                }
                None => None,
            };
            let _ = opened.set(shared.clone());
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("no tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                let mut routes = HashMap::new();
                for (name, server) in config.servers {
                    let (tx, rx) = unbounded_channel();
                    routes.insert(name.clone(), tx);
                    tokio::spawn(run_server(name, server, rx, ev_tx.clone(), shared.clone()));
                }
                while let Some(cmd) = cmd_rx.recv().await {
                    if let Some(tx) = routes.get(cmd.network()) {
                        let _ = tx.send(cmd);
                    }
                }
            });
        })
        .expect("spawn network thread");
    (cmd_tx, ev_rx, flush)
}

enum End {
    /// Don't reconnect until /reconnect.
    Stopped(String),
    /// The user quit; don't reconnect until /reconnect.
    Quit,
    /// The socket closed; the server's `ERROR` text, if it sent one, says why.
    Closed(Option<String>),
}

/// What outlives one connection.
#[derive(Default)]
struct Carry {
    /// Channels joined at runtime, rejoined after a reconnect.
    rejoin: Vec<String>,
    /// What history each target holds, so a reconnect asks only for the gap.
    book: Book,
    /// The history cache, when there is one.
    cache: Option<Shared>,
    /// The book's discovery time as last written to the cache.
    discovered: Option<DateTime<Utc>>,
    /// The last registered connection's ISUPPORT: which names are channels
    /// while offline.
    isupport: Isupport,
}

/// The delay before the next attempt and the backoff to carry forward. A
/// session that got as far as registering resets the backoff, however it
/// ended (clean close, reset, TLS error, idle timeout).
pub fn retry_delay(backoff: Duration, registered: bool) -> (Duration, Duration) {
    let delay = if registered { MIN_BACKOFF } else { backoff };
    (delay, (delay * 2).min(MAX_BACKOFF))
}

/// hirc's own error line, for the server buffer.
fn note(text: String) -> Line {
    Line {
        seq: 0,
        time: Utc::now(),
        msgid: None,
        kind: LineKind::Error,
        nick: None,
        text,
        own: true,
        delivery: Delivery::Sent,
        boundary: false,
        replayed: false,
        via: None,
        reply_to: None,
    }
}

/// The cache's latest page of each window worth opening on it, before the
/// first connection: every query (but services'), and the configured
/// channels (one joined last time but not configured isn't rejoined, so its
/// window would stay dead). The book learns what they hold, both ends, so
/// the server is asked only for what is newer; and where discovery got to.
/// A network that can't serve history (turned off, or it had none last
/// time) opens on the cache's lines past breaks: they are all there is.
fn seed(name: &str, server: &Server, cache: &Shared, book: &mut Book) -> Vec<Event> {
    let reach = if !server.chathistory || matches!(cache.read(|c| c.history(name)), Ok(Some(false))) { Reach::All } else { Reach::ToBreak };
    match cache.read(|c| c.discovered(name)) {
        Ok(Some(t)) => book.set_discovered(t),
        Ok(None) => {}
        Err(e) => tracing::warn!("{name}: history cache: {e}"),
    }
    let page = history::page_size(server.history_page_size, None);
    let configured = |t: &str| server.channels.iter().any(|c| c.split(' ').next().is_some_and(|c| c.eq_ignore_ascii_case(t)));
    let targets = match cache.read(|c| c.targets(name)) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("{name}: history cache: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (target, query) in targets.into_iter().filter(|(t, q)| if *q { !services::is_service(t) } else { configured(t) }) {
        let lines = match cache.read(|c| c.latest_in(name, &target, page, reach)) {
            Ok(lines) if !lines.is_empty() => lines,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!("{name}: history cache: {e}");
                continue;
            }
        };
        // The page's ends, not `note`: lines in one millisecond are told
        // apart by order only, and the last is the newest.
        book.reach(&target, query, lines.first().map(Anchor::of).as_ref(), lines.last().map(Anchor::of).as_ref());
        let target = if query { Target::Query(target) } else { Target::Channel(target) };
        let page = Event::History { network: name.to_owned(), target, lines, start: false, older: false };
        let reactions = cached_reactions(cache, &page);
        out.push(page);
        out.extend(reactions);
    }
    out
}

/// The cache's reactions to a page's lines, to follow the page out (the
/// store keeps them till their lines are in).
fn cached_reactions(cache: &Shared, ev: &Event) -> Vec<Event> {
    let Event::History { network, target, lines, .. } = ev else { return Vec::new() };
    let Some(name) = target.name() else { return Vec::new() };
    let ids: Vec<&str> = lines.iter().filter_map(|l| l.msgid.as_deref()).filter(|m| !crate::msgid::is_made_up(m)).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    match cache.read(|c| c.reactions(network, name, &ids)) {
        Ok(rows) => rows
            .into_iter()
            .map(|r| Event::Reaction {
                network: network.clone(),
                target: target.clone(),
                parent: r.parent,
                emoji: r.emoji,
                nick: r.nick,
                add: r.on,
                time: r.time,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("{network}: history cache: {e}");
            Vec::new()
        }
    }
}

/// Older lines for a window: the cache's while it has lines older than the
/// oldest shown that follow on from it, then the server's. Where the cache
/// stops at a break, or runs short, the server is asked for what is before
/// the cache's oldest at once: the lines between are never skipped.
/// Without server history, or once the server has nothing older on this
/// connection (its history ends before a break), the cache is read past
/// its breaks, and the session says "start" only once the cache has
/// nothing older. A break alone is never the start.
fn older(session: &mut Session, cache: Option<&Shared>, network: &str, cmd: Command) -> Vec<Output> {
    if let (Some(cache), Command::Older { target, .. }) = (cache, &cmd)
        && let Some(anchor) = session.book().oldest(target).cloned()
        && let Some(id) = anchor.msgid.clone()
    {
        let page = session.page();
        let reach = if session.history_off() || session.server_ended(target) { Reach::All } else { Reach::ToBreak };
        // Read past breaks, an anchor not held (a page the writer hasn't
        // put in yet) is read from by its time.
        let read = |c: &crate::cache::Cache| match c.before(network, target, &id, page, reach)? {
            o if o.broke && reach == Reach::All => c.before_time(network, target, anchor.time, page),
            o => Ok(o),
        };
        match cache.read(read) {
            Ok(older) if !older.lines.is_empty() => {
                let more = older.broke || older.lines.len() < page;
                let mut out = session.cached(target, older.lines);
                if more {
                    out.extend(session.command(cmd, Utc::now()));
                }
                return out;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("{network}: history cache: {e}"),
        }
    }
    session.command(cmd, Utc::now())
}

/// The cache's latest page for a window just joined that nothing else
/// fills, past its breaks: without server history they are all there is.
fn latest(session: &mut Session, cache: Option<&Shared>, network: &str, target: &str) -> Vec<Output> {
    let Some(cache) = cache else { return Vec::new() };
    match cache.read(|c| c.latest_in(network, target, session.page(), Reach::All)) {
        Ok(lines) if !lines.is_empty() => session.cached(target, lines),
        Ok(_) => Vec::new(),
        Err(e) => {
            tracing::warn!("{network}: history cache: {e}");
            Vec::new()
        }
    }
}

/// The session's outputs, with each `CacheOlder` answered: the server's
/// history for that target ended, so the cache is read past its breaks,
/// and "start" is said only when it has nothing older either.
fn resolve(session: &mut Session, cache: Option<&Shared>, network: &str, outs: Vec<Output>) -> Vec<Output> {
    let mut done = Vec::with_capacity(outs.len());
    for out in outs {
        match out {
            Output::CacheOlder(target) => done.extend(older(session, cache, network, Command::Older { network: network.to_owned(), target })),
            Output::CacheLatest(target) => done.extend(latest(session, cache, network, &target)),
            out => done.push(out),
        }
    }
    done
}

/// What the session says to keep goes to the cache's writer.
fn keep(cache: Option<&Shared>, session: &mut Session) {
    if let Some(cache) = cache {
        for entry in session.kept() {
            cache.write(entry);
        }
    }
}

/// Where discovery got to goes to the cache whenever it moves, so a
/// restart's TARGETS starts there too.
fn keep_discovered(cache: &Shared, network: &str, book: &Book, said: &mut Option<DateTime<Utc>>) {
    if let Some(t) = book.discovered()
        && *said != Some(t)
    {
        cache.write_discovered(network, t);
        *said = Some(t);
    }
}

async fn run_server(name: String, server: Server, mut cmds: UnboundedReceiver<Command>, events: UnboundedSender<Event>, cache: Option<Shared>) {
    let status = |status| {
        let _ = events.send(Event::Status { network: name.clone(), status });
    };
    let mut backoff = MIN_BACKOFF;
    // Resolved secrets live for the process so reconnects don't re-run
    // `password_command` (each run can prompt). A Stop clears them.
    let mut cached: Option<SessionConfig> = None;
    let mut carry = Carry { cache, ..Carry::default() };
    if let Some(cache) = carry.cache.clone() {
        // Reads the disk: off this runtime's thread, so another network's
        // connect isn't held up.
        let (n, s) = (name.clone(), server.clone());
        match tokio::task::spawn_blocking(move || {
            let mut book = Book::default();
            let events = seed(&n, &s, &cache, &mut book);
            (book, events)
        })
        .await
        {
            Ok((book, seeded)) => {
                carry.book = book;
                for ev in seeded {
                    let _ = events.send(ev);
                }
            }
            Err(e) => tracing::warn!("{name}: history cache: {e}"),
        }
        carry.discovered = carry.book.discovered();
    }
    loop {
        status(ConnStatus::Connecting);
        let mut registered = false;
        let reason = match connect_once(&name, &server, &mut cached, &mut carry, &mut registered, &mut cmds, &events).await {
            Ok(end @ (End::Stopped(_) | End::Quit)) => {
                cached = None;
                status(match end {
                    End::Stopped(reason) => ConnStatus::Failed { reason },
                    _ => ConnStatus::Offline,
                });
                if wait_reconnect(&mut cmds, &mut carry.book, &carry.isupport).await.is_none() {
                    return;
                }
                backoff = MIN_BACKOFF;
                continue;
            }
            Ok(End::Closed(why)) => why.unwrap_or_else(|| "connection closed".into()),
            Err(e) => e.to_string(),
        };
        let (delay, next) = retry_delay(backoff, registered);
        backoff = next;
        status(ConnStatus::Disconnected { reason, retry: delay });
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            r = wait_reconnect(&mut cmds, &mut carry.book, &carry.isupport) => {
                if r.is_none() {
                    return;
                }
            }
        }
    }
}

/// Waits for /reconnect, dropping other commands (there's nowhere to send
/// them). A window opened meanwhile is kept, for the next connection to
/// backfill. `None` when the app is shutting down.
/// `isupport` (the last connection's) says which names are channels.
async fn wait_reconnect(cmds: &mut UnboundedReceiver<Command>, book: &mut Book, isupport: &Isupport) -> Option<()> {
    loop {
        match cmds.recv().await? {
            Command::Reconnect { .. } => return Some(()),
            Command::Open { target, .. } => book.open(&target, !isupport.is_channel(&target)),
            _ => continue,
        }
    }
}

/// Configured channels plus those joined at runtime, deduplicated on the
/// channel name (a configured entry may carry a key after the name).
fn merge_channels(configured: &[String], rejoin: &[String]) -> Vec<String> {
    let name = |c: &str| c.split(' ').next().unwrap_or("").to_ascii_lowercase();
    let mut out = configured.to_vec();
    for ch in rejoin {
        if !out.iter().any(|o| name(o) == name(ch)) {
            out.push(ch.clone());
        }
    }
    out
}

fn session_config(s: &Server) -> Result<SessionConfig, ConfigError> {
    let sasl = match &s.sasl {
        Some(Sasl::Plain { username, password, password_file, password_command }) => {
            let password = config::secret(password.as_deref(), password_file.as_deref(), password_command.as_deref())?.unwrap_or_default();
            Some(SaslPlain { username: username.clone(), password })
        }
        Some(Sasl::External { .. }) | None => None,
    };
    // IRCCloud's: with this client's id in it, so it replays what we missed.
    let mut password = config::secret(s.password.as_deref(), s.password_file.as_deref(), s.password_command.as_deref())?;
    let irccloud = irccloud::is_host(&s.server);
    if irccloud && let (Some(p), Some(id)) = (&mut password, &s.bouncer_client_id) {
        *p = irccloud::password(p, id);
    }
    Ok(SessionConfig {
        nickname: s.nickname.clone(),
        alt_nicks: s.alt_nicks.clone(),
        username: s.username.clone().unwrap_or_else(|| s.nickname.clone()),
        realname: s.realname.clone().unwrap_or_else(|| s.nickname.clone()),
        self_replays: irccloud && password.as_deref().is_some_and(irccloud::replays),
        password,
        sasl,
        channels: s.channels.clone(),
        bridges: s.bridges.clone(),
        chathistory: s.chathistory,
        history_page: s.history_page_size,
    })
}

/// A configured `root_cert_path` adds roots on top of the platform's (Halloy
/// semantics). A missing, empty or invalid PEM is an error, not a fallback.
fn tls_config(root_cert: Option<&Path>) -> Result<Arc<ClientConfig>, Error> {
    let config = match root_cert {
        Some(path) => {
            let certs = CertificateDer::pem_file_iter(path)?.collect::<Result<Vec<_>, _>>()?;
            if certs.is_empty() {
                return Err(format!("no certificates in {}", path.display()).into());
            }
            let builder = ClientConfig::builder();
            let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(certs, builder.crypto_provider().clone())?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(verifier)).with_no_client_auth()
        }
        None => ClientConfig::with_platform_verifier()?,
    };
    Ok(Arc::new(config))
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

async fn connect_once(
    name: &str,
    server: &Server,
    cached: &mut Option<SessionConfig>,
    carry: &mut Carry,
    registered: &mut bool,
    cmds: &mut UnboundedReceiver<Command>,
    events: &UnboundedSender<Event>,
) -> Result<End, Error> {
    if matches!(server.sasl, Some(Sasl::External { .. })) {
        return Ok(End::Stopped("SASL EXTERNAL isn't supported yet".into()));
    }
    let host = server.server.clone();
    // Refuse before touching any secret.
    if !server.use_tls && !is_loopback(&host) {
        return Ok(End::Stopped("plaintext is only allowed to localhost; set use_tls = true".into()));
    }
    let connector = if server.use_tls {
        match tls_config(server.root_cert_path.as_deref()) {
            Ok(c) => Some(TlsConnector::from(c)),
            Err(e) => return Ok(End::Stopped(format!("TLS setup failed: {e}"))),
        }
    } else {
        None
    };
    let cfg = match cached {
        Some(cfg) => cfg.clone(),
        None => {
            let s = server.clone();
            match tokio::task::spawn_blocking(move || session_config(&s)).await? {
                Ok(cfg) => cached.insert(cfg).clone(),
                Err(e) => return Ok(End::Stopped(e.to_string())),
            }
        }
    };
    let server_name = ServerName::try_from(host.clone())?;
    let port = server.port();
    // One budget for TCP connect plus TLS handshake.
    let stream: Box<dyn Stream> = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        Ok::<_, Error>(match connector {
            Some(c) => Box::new(c.connect(server_name, tcp).await?) as Box<dyn Stream>,
            None => Box::new(tcp),
        })
    })
    .await
    .map_err(|_| Error::from("connect timed out"))??;
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = LineReader::new(BufReader::new(r));
    let mut cfg = cfg;
    cfg.channels = merge_channels(&cfg.channels, &carry.rejoin);
    let mut session = Session::new(name, cfg).with_book(std::mem::take(&mut carry.book));
    if carry.cache.is_some() {
        session = session.keeping();
    }
    let mut outs = session.start();
    let mut last_rx = Instant::now();
    let mut pinged = false;
    let (idle_ping, idle_dead, check) = idle_limits(server);
    let mut tick = tokio::time::interval(check);
    let end = async {
        loop {
            for out in resolve(&mut session, carry.cache.as_ref(), name, std::mem::take(&mut outs)) {
                match out {
                    Output::Send(m) => {
                        w.write_all(m.to_wire().as_bytes()).await?;
                        w.write_all(b"\r\n").await?;
                    }
                    Output::Event(e) => {
                        if matches!(e, Event::Status { status: ConnStatus::Connected { .. }, .. }) {
                            *registered = true;
                            // For the next start's seed: read past breaks?
                            if let Some(cache) = &carry.cache {
                                cache.write_history(name, !session.history_off());
                            }
                        }
                        match &e {
                            Event::Joined { channel, .. } if !carry.rejoin.iter().any(|c| c.eq_ignore_ascii_case(channel)) => {
                                carry.rejoin.push(channel.clone())
                            }
                            Event::Parted { channel, .. } => carry.rejoin.retain(|c| !c.eq_ignore_ascii_case(channel)),
                            _ => {}
                        }
                        if let (Some(cache), Event::Reaction { network, target, parent, emoji, nick, add, time }) = (&carry.cache, &e) {
                            cache.write_reaction(
                                network,
                                target,
                                Reacted { parent: parent.clone(), emoji: emoji.clone(), nick: nick.clone(), on: *add, time: *time },
                            );
                        }
                        // A page is followed by what the cache holds of
                        // reactions to its lines.
                        let after = carry.cache.as_ref().map(|cache| cached_reactions(cache, &e)).unwrap_or_default();
                        let _ = events.send(e);
                        for r in after {
                            let _ = events.send(r);
                        }
                    }
                    Output::Stop(reason) => {
                        let _ = w.flush().await;
                        return Ok(End::Stopped(reason));
                    }
                    Output::Quit => {
                        let _ = w.flush().await;
                        return Ok(End::Quit);
                    }
                    // Answered by `resolve`.
                    Output::CacheOlder(_) | Output::CacheLatest(_) => {}
                }
            }
            keep(carry.cache.as_ref(), &mut session);
            if let Some(cache) = &carry.cache {
                keep_discovered(cache, name, session.book(), &mut carry.discovered);
            }
            w.flush().await?;
            tokio::select! {
                line = reader.next_line() => {
                    let Some(line) = line? else { return Ok(End::Closed(session.closing().map(str::to_owned))) };
                    last_rx = Instant::now();
                    pinged = false;
                    // What the server sent, never what we send: ours can
                    // hold PASS or AUTHENTICATE. Off unless RUST_LOG asks.
                    tracing::trace!(target: "hirc::wire", "{name} <- {}", line.trim_end_matches(['\r', '\n']));
                    match Message::parse(&line) {
                        Ok(m) => outs = session.handle(m, Utc::now()),
                        Err(e) => tracing::debug!("{name}: dropped line ({e}): {line:?}"),
                    }
                }
                cmd = cmds.recv() => match cmd {
                    None => return Ok(End::Stopped("shutting down".into())),
                    Some(Command::Reconnect { .. }) => {
                        let _ = w.write_all(b"QUIT :Reconnecting\r\n").await;
                        let _ = w.flush().await;
                        return Ok(End::Closed(None));
                    }
                    Some(c @ Command::Older { .. }) => outs = older(&mut session, carry.cache.as_ref(), name, c),
                    Some(c) => outs = session.command(c, Utc::now()),
                },
                _ = tick.tick() => {
                    let idle = last_rx.elapsed();
                    if idle > idle_dead {
                        return Err("no reply from server".into());
                    }
                    if idle > idle_ping && !pinged {
                        pinged = true;
                        outs = vec![Output::Send(Message::new("PING", ["hirc"]))];
                    }
                }
            }
        }
    }
    .await;
    // A multiline batch cut off by the drop still shows what came of it.
    for out in session.end(Utc::now()) {
        if let Output::Event(e) = out {
            let _ = events.send(e);
        }
    }
    keep(carry.cache.as_ref(), &mut session);
    carry.book = session.take_book();
    if let Some(cache) = &carry.cache {
        keep_discovered(cache, name, &carry.book, &mut carry.discovered);
    }
    if *registered {
        carry.isupport = session.isupport().clone();
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Above, Below, Cache, Entry};
    use crate::session::tests::{asked, feed, historied, histories, irccloud, now, sent, zncd};

    fn line(id: &str, secs: i64) -> Line {
        Line {
            seq: 0,
            time: DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap(),
            msgid: Some(id.into()),
            kind: LineKind::Privmsg,
            nick: Some("bob".into()),
            text: id.into(),
            own: false,
            delivery: Delivery::Sent,
            boundary: false,
            replayed: false,
            via: None,
            reply_to: None,
        }
    }

    fn cache_with(dir: &tempfile::TempDir, entries: Vec<(Target, Vec<Line>)>) -> Shared {
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        for (target, lines) in entries {
            cache.write(Entry { network: "net".into(), target, lines, below: Below::Breaks, above: Above::Leave, older: false });
        }
        assert!(cache.flush());
        cache
    }

    #[test]
    fn the_cache_seeds_queries_and_configured_channels() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_with(
            &dir,
            vec![
                (Target::Channel("#a".into()), vec![line("a1", 1), line("a2", 2)]),
                (Target::Channel("#gone".into()), vec![line("g1", 1)]),
                (Target::Query("bob".into()), vec![line("q1", 3)]),
            ],
        );
        let server = Server { channels: vec!["#A key".into()], history_page_size: Some(1), ..Server::default() };
        let mut book = Book::default();
        let got: Vec<(Target, Vec<String>)> = seed("net", &server, &cache, &mut book)
            .into_iter()
            .map(|e| match e {
                Event::History { target, lines, start: false, .. } => (target, lines.into_iter().map(|l| l.text).collect()),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(got, [(Target::Channel("#a".into()), vec!["a2".to_owned()]), (Target::Query("bob".into()), vec!["q1".to_owned()])]);
        assert_eq!(book.newest("#a").and_then(|a| a.msgid.as_deref()), Some("a2"));
        assert_eq!(book.queries(), ["bob"]);
        assert_eq!(book.discovered(), None);
    }

    #[test]
    fn a_seeded_page_is_followed_by_its_cached_reactions() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_with(&dir, vec![(Target::Channel("#a".into()), vec![line("a1", 1), line("a2", 2)])]);
        let when = DateTime::from_timestamp_millis(1_800_000_000_000).unwrap();
        let r = |parent: &str, nick: &str, on: bool| crate::cache::Reacted { parent: parent.into(), emoji: "👍".into(), nick: nick.into(), on, time: when };
        cache.write_reaction("net", &Target::Channel("#a".into()), r("a1", "bob", true));
        cache.write_reaction("net", &Target::Channel("#a".into()), r("zzz", "bob", true));
        assert!(cache.flush());
        let got = seed("net", &Server { channels: vec!["#a".into()], ..Server::default() }, &cache, &mut Book::default());
        match got.as_slice() {
            [Event::History { .. }, Event::Reaction { parent, nick, add: true, .. }] => assert_eq!((parent.as_str(), nick.as_str()), ("a1", "bob")),
            e => panic!("{e:?}"),
        }
    }

    /// A bouncer joins us to channels the config doesn't name, so they
    /// weren't seeded: the cache fills each as we join it, past its breaks
    /// (there is no server history), then no more.
    #[test]
    fn a_channel_the_bouncer_joins_us_to_opens_on_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let chan = || Target::Channel("#test".into());
        let cache = cache_with(&dir, vec![(chan(), vec![line("c1", 1)]), (chan(), vec![line("c2", 2)])]);
        let mut s = irccloud(true).keeping();
        let outs = feed(&mut s, ":alice!a@h JOIN #test");
        assert_eq!(got(&served(&mut s, &cache, outs)), [(vec!["c1".to_owned(), "c2".to_owned()], false)]);
        assert_eq!(s.book().oldest("#test").and_then(|a| a.msgid.as_deref()), Some("c1"));
        // Joined again (a reconnect in the same run): the window has its lines.
        feed(&mut s, ":alice!a@h PART #test");
        let outs = feed(&mut s, ":alice!a@h JOIN #test");
        assert!(histories(&served(&mut s, &cache, outs)).is_empty());
    }

    #[test]
    fn a_seeded_page_in_one_second_is_held_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_with(&dir, vec![(Target::Channel("#a".into()), vec![line("a1", 1), line("a2", 1), line("a3", 1)])]);
        let server = Server { channels: vec!["#a".into()], ..Server::default() };
        let mut book = Book::default();
        seed("net", &server, &cache, &mut book);
        // The gap is asked for after the last line, not one tied with it.
        assert_eq!(book.newest("#a").and_then(|a| a.msgid.as_deref()), Some("a3"));
        assert_eq!(book.oldest("#a").and_then(|a| a.msgid.as_deref()), Some("a1"));
    }

    #[test]
    fn discovery_is_kept_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_with(&dir, Vec::new());
        let at = |s: i64| DateTime::from_timestamp(1_800_000_000 + s, 0).unwrap();
        cache.write_discovered("net", at(5));
        assert!(cache.flush());
        let mut book = Book::default();
        seed("net", &Server::default(), &cache, &mut book);
        assert_eq!(book.discovered(), Some(at(5)));
        // It is written through only when it moves.
        let mut said = book.discovered();
        keep_discovered(&cache, "net", &book, &mut said);
        book.set_discovered(at(9));
        keep_discovered(&cache, "net", &book, &mut said);
        assert_eq!(said, Some(at(9)));
        assert!(cache.flush());
        assert_eq!(cache.read(|c| c.discovered("net")).unwrap(), Some(at(9)));
    }

    #[test]
    fn older_lines_come_from_the_cache_while_it_has_them() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_with(&dir, vec![(Target::Channel("#a".into()), vec![line("c1", 1), line("c2", 2)])]);
        let mut book = Book::default();
        book.note("#a", false, &[line("c2", 2)]);
        // History is off for this session, so the server's answer is "the start".
        let mut session = Session::new("net", SessionConfig::default()).with_book(book);
        let cmd = || Command::Older { network: "net".into(), target: "#a".into() };
        let out = older(&mut session, Some(&cache), "net", cmd());
        // c1 is where the cache's run begins: the server is asked on at once.
        let got: Vec<(Vec<String>, bool)> = histories(&out).into_iter().map(|(_, l, start)| (l.into_iter().map(|l| l.text).collect(), start)).collect();
        assert_eq!(got, [(vec!["c1".to_owned()], false), (vec![], true)]);
        let out = older(&mut session, Some(&cache), "net", cmd());
        assert!(matches!(out.as_slice(), [Output::Event(Event::History { lines, start: true, .. })] if lines.is_empty()), "{out:?}");
    }

    #[test]
    fn without_server_history_the_cache_reads_past_its_breaks() {
        let dir = tempfile::tempdir().unwrap();
        let chan = || Target::Channel("#a".into());
        // Two connections' worth: c3 is a break.
        let cache = cache_with(&dir, vec![(chan(), vec![line("c1", 1), line("c2", 2)]), (chan(), vec![line("c3", 3), line("c4", 4)])]);
        let texts = |lines: &[Line]| lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>();
        let seeded = |server: &Server, book: &mut Book| match seed("net", server, &cache, book).as_slice() {
            [Event::History { lines, .. }] => texts(lines),
            e => panic!("{e:?}"),
        };
        let page3 = Server { channels: vec!["#a".into()], history_page_size: Some(3), ..Server::default() };
        // History off: the cache is all there is, breaks and all.
        let off = Server { chathistory: false, ..page3.clone() };
        let mut book = Book::default();
        assert_eq!(seeded(&off, &mut book), ["c2", "c3", "c4"]);
        let mut s = Session::new("net", SessionConfig { chathistory: false, history_page: Some(3), ..SessionConfig::default() }).with_book(book);
        let got: Vec<(Vec<String>, bool)> = histories(&older_of(&mut s, &cache, "#a")).into_iter().map(|(_, l, start)| (texts(&l), start)).collect();
        // Then the start: there is truly nothing older.
        assert_eq!(got, [(vec!["c1".to_owned()], false), (vec![], true)]);
        // History on, the same cache: the break stops it, and the server is asked.
        let mut book = Book::default();
        assert_eq!(seeded(&page3, &mut book), ["c3", "c4"]);
        let mut s = historied("", 3, book).keeping();
        let out = older_of(&mut s, &cache, "#a");
        assert!(histories(&out).is_empty(), "{out:?}");
        assert_eq!(asked(&out).0, "CHATHISTORY BEFORE #a msgid=c3 3");
        // A network last seen without history is read past its breaks too.
        cache.write_history("net", false);
        assert!(cache.flush());
        assert_eq!(seeded(&page3, &mut Book::default()), ["c2", "c3", "c4"]);
    }

    /// A page of `items` (msgid, minutes past 09:00) for `target` as batch
    /// `r`, answering `label` (none: nobody asked for it).
    fn batch(s: &mut Session, label: Option<&str>, r: &str, target: &str, items: &[(&str, u32)]) -> Vec<Output> {
        let tag = label.map_or(String::new(), |l| format!("@label={l} "));
        feed(s, &format!("{tag}:srv BATCH +{r} chathistory {target}"));
        for (id, min) in items {
            feed(s, &format!("@batch={r};time=2026-10-22T09:{min:02}:00.000Z;msgid={id} :bob!b@h PRIVMSG {target} :{id}"));
        }
        feed(s, &format!(":srv BATCH -{r}"))
    }

    fn live(s: &mut Session, target: &str, id: &str, min: u32) -> Vec<Output> {
        feed(s, &format!("@time=2026-10-22T11:{min:02}:00.000Z;msgid={id} :bob!b@h PRIVMSG {target} :{id}"))
    }

    /// Everything the session said to keep, written. True if it all went in.
    fn written(s: &mut Session, cache: &Shared) -> bool {
        keep(Some(cache), s);
        cache.flush()
    }

    fn older_of(s: &mut Session, cache: &Shared, target: &str) -> Vec<Output> {
        older(s, Some(cache), "net", Command::Older { network: "net".into(), target: target.into() })
    }

    /// A fresh hirc on `cache`, configured for `channels`: seeded, then
    /// connected and registered.
    fn restart(cache: &Shared, channels: &[&str]) -> Session {
        let server = Server { channels: channels.iter().map(|c| (*c).to_owned()).collect(), ..Server::default() };
        let mut book = Book::default();
        seed("net", &server, cache, &mut book);
        historied("", 3, book).keeping()
    }

    /// `#test` joined, with its latest page o7..o9 in the cache.
    fn joined(cache: &Shared) -> Session {
        let mut s = historied("", 3, Book::default()).keeping();
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        batch(&mut s, Some(&label), "l", "#test", &[("o7", 7), ("o8", 8), ("o9", 9)]);
        assert!(written(&mut s, cache));
        s
    }

    /// Older: nothing comes from the cache, and the server is asked for
    /// what is before `msgid`.
    fn asks_the_server_before(s: &mut Session, cache: &Shared, target: &str, msgid: &str) {
        let out = older_of(s, cache, target);
        assert!(histories(&out).is_empty(), "nothing from across the hole: {out:?}");
        assert_eq!(asked(&out).0, format!("CHATHISTORY BEFORE {target} msgid={msgid} 3"));
    }

    fn kept_ids(s: &mut Session) -> Vec<(Vec<String>, Below)> {
        s.kept().into_iter().map(|e| (e.lines.into_iter().filter_map(|l| l.msgid).collect(), e.below)).collect()
    }

    #[test]
    fn a_page_before_cut_off_never_hides_what_it_missed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let mut s = joined(&cache);
        let (wire, label) = asked(&older_of(&mut s, &cache, "#test"));
        assert_eq!(wire, "CHATHISTORY BEFORE #test msgid=o7 3");
        // o4..o6 is on its way; the drop cuts it off after o4.
        feed(&mut s, &format!("@label={label} :srv BATCH +b chathistory #test"));
        feed(&mut s, "@batch=b;time=2026-10-22T09:04:00.000Z;msgid=o4 :bob!b@h PRIVMSG #test :o4");
        s.end(now());
        assert!(written(&mut s, &cache));
        // The next connection, and a restart, ask the server for o5 and o6.
        let mut next = historied("", 3, s.take_book()).keeping();
        asks_the_server_before(&mut next, &cache, "#test", "o7");
        asks_the_server_before(&mut restart(&cache, &["#test"]), &cache, "#test", "o7");
    }

    #[test]
    fn a_page_nobody_asked_for_never_hides_what_is_between() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let mut s = joined(&cache);
        // A bouncer's replay of o2 and o3: it doesn't reach o7.
        batch(&mut s, None, "u", "#test", &[("o2", 2), ("o3", 3)]);
        assert!(written(&mut s, &cache));
        asks_the_server_before(&mut s, &cache, "#test", "o7");
        asks_the_server_before(&mut restart(&cache, &["#test"]), &cache, "#test", "o7");
    }

    #[test]
    fn a_cached_channel_joined_again_never_shows_its_old_lines_as_next() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let mut s = historied("", 3, Book::default()).keeping();
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #old"));
        batch(&mut s, Some(&label), "l", "#old", &[("x1", 1), ("x2", 2)]);
        assert!(written(&mut s, &cache));
        // Not configured now, so not seeded: /join gets its latest page.
        let mut s = restart(&cache, &["#test"]);
        let (wire, label) = asked(&feed(&mut s, ":alice!a@h JOIN #old"));
        assert_eq!(wire, "CHATHISTORY LATEST #old * 3");
        batch(&mut s, Some(&label), "l", "#old", &[("y7", 7), ("y8", 8), ("y9", 9)]);
        assert!(written(&mut s, &cache));
        asks_the_server_before(&mut s, &cache, "#old", "y7");
        asks_the_server_before(&mut restart(&cache, &["#old"]), &cache, "#old", "y7");
    }

    #[test]
    fn a_live_line_the_fill_missed_never_hides_what_is_below_it() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let mut s = historied("", 3, Book::default()).keeping();
        let (_, label) = asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        batch(&mut s, Some(&label), "l", "#test", &[("a1", 1), ("a2", 2)]);
        s.end(now());
        assert!(written(&mut s, &cache));
        let mut s = historied("", 3, s.take_book()).keeping();
        let (wire, label) = asked(&feed(&mut s, "@time=2026-10-22T11:00:00.000Z;msgid=j1 :alice!a@h JOIN #test"));
        assert_eq!(wire, "CHATHISTORY AFTER #test msgid=a2 3");
        live(&mut s, "#test", "l1", 1);
        // The fill's last page ends at a3: j1 and l1 aren't in it.
        batch(&mut s, Some(&label), "f", "#test", &[("a3", 3)]);
        live(&mut s, "#test", "l2", 2);
        assert!(written(&mut s, &cache));
        // A restart opens on j1..l2 and asks for what is after l2; scrolling
        // up asks the server for what is before j1, not the cache for a3.
        let mut r = restart(&cache, &["#test"]);
        assert_eq!(asked(&feed(&mut r, ":alice!a@h JOIN #test")).0, "CHATHISTORY AFTER #test msgid=l2 3");
        asks_the_server_before(&mut restart(&cache, &["#test"]), &cache, "#test", "j1");
    }

    #[test]
    fn a_failed_write_never_hides_what_it_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let cache = Shared::open(&path).unwrap();
        let mut s = joined(&cache);
        live(&mut s, "#test", "l0", 0);
        assert!(written(&mut s, &cache));
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TRIGGER no BEFORE INSERT ON lines WHEN NEW.msgid = 'l1' BEGIN SELECT RAISE(ABORT, 'no'); END;")
            .unwrap();
        live(&mut s, "#test", "l1", 1);
        assert!(!written(&mut s, &cache));
        live(&mut s, "#test", "l2", 2);
        assert!(written(&mut s, &cache));
        asks_the_server_before(&mut restart(&cache, &["#test"]), &cache, "#test", "l2");
    }

    #[test]
    fn live_lines_follow_on_until_one_the_cache_cannot_keep() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let mut s = joined(&cache);
        // Our JOIN (no msgid here) came before the latest page: a hole.
        live(&mut s, "#test", "l0", 0);
        live(&mut s, "#test", "l1", 1);
        // A line with no msgid to keep it by (ours, without echo-message, is one).
        feed(&mut s, ":carol!c@h PRIVMSG #test :no msgid on this one");
        live(&mut s, "#test", "l2", 2);
        // Ours, on a server without echo-message: shown with no msgid.
        feed(&mut s, ":srv CAP alice DEL :echo-message");
        s.command(Command::Say { network: "net".into(), target: "#test".into(), text: "hi".into() }, now());
        live(&mut s, "#test", "l3", 3);
        let want = [("l0", Below::Breaks), ("l1", Below::Joins("l0".into())), ("l2", Below::Breaks), ("l3", Below::Breaks)];
        assert_eq!(kept_ids(&mut s), want.map(|(id, b)| (vec![id.to_owned()], b)));
    }

    #[test]
    fn a_live_line_while_the_gap_fills_is_kept_as_a_break() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let mut s = historied("", 3, joined(&cache).take_book()).keeping();
        asked(&feed(&mut s, ":alice!a@h JOIN #test"));
        live(&mut s, "#test", "l1", 1);
        live(&mut s, "#test", "l2", 2);
        assert_eq!(kept_ids(&mut s), [(vec!["l1".to_owned()], Below::Breaks), (vec!["l2".to_owned()], Below::Joins("l1".into()))]);
    }

    /// What the network task makes of `outs`, the session's answer to a
    /// server line.
    fn served(s: &mut Session, cache: &Shared, outs: Vec<Output>) -> Vec<Output> {
        resolve(s, Some(cache), "net", outs)
    }

    /// `ids` (with their seconds, as `line` has them) as one labeled
    /// `chathistory` page for `#test`.
    fn page_at(s: &mut Session, cache: &Shared, label: &str, ids: &[(&str, i64)]) -> Vec<Output> {
        feed(s, &format!("@label={label} :srv BATCH +p chathistory #test"));
        for (id, secs) in ids {
            let t = line(id, *secs).time.format("%Y-%m-%dT%H:%M:%S%.3fZ");
            feed(s, &format!("@batch=p;time={t};msgid={id} :bob!b@h PRIVMSG #test :{id}"));
        }
        let outs = feed(s, ":srv BATCH -p");
        served(s, cache, outs)
    }

    fn got(out: &[Output]) -> Vec<(Vec<String>, bool)> {
        histories(out).into_iter().map(|(_, l, start)| (l.into_iter().map(|l| l.text).collect(), start)).collect()
    }

    /// Ten days away on a server that keeps a week: the fill's first line
    /// is a break, and the server has nothing before it. The months of
    /// cached lines below it are still history, and the start is only
    /// where the cache ends too.
    #[test]
    fn a_break_older_than_the_servers_history_never_hides_the_cache_below_it() {
        let dir = tempfile::tempdir().unwrap();
        let chan = || Target::Channel("#test".into());
        let old: Vec<Line> = (1..=6).map(|i| line(&format!("o{i}"), i)).collect();
        let fill = vec![line("f1", 1000), line("f2", 1001), line("f3", 1002)];
        let cache = cache_with(&dir, vec![(chan(), old), (chan(), fill)]);
        let mut s = restart(&cache, &["#test"]);
        // The break stops the cache: the server is asked.
        let (wire, label) = asked(&older_of(&mut s, &cache, "#test"));
        assert_eq!(wire, "CHATHISTORY BEFORE #test msgid=f1 3");
        // It has nothing that old: the cache goes on past the break.
        let out = page_at(&mut s, &cache, &label, &[]);
        assert_eq!(got(&out), [(vec!["o4".to_owned(), "o5".to_owned(), "o6".to_owned()], false)]);
        assert!(sent(&out).is_empty(), "{out:?}");
        // Further up, still the cache, never the server again.
        let out = older_of(&mut s, &cache, "#test");
        assert_eq!(got(&out), [(vec!["o1".to_owned(), "o2".to_owned(), "o3".to_owned()], false)]);
        assert!(sent(&out).is_empty(), "{out:?}");
        // Both have nothing older: now it is the start.
        let out = older_of(&mut s, &cache, "#test");
        assert_eq!(got(&out), [(vec![], true)]);
        assert!(sent(&out).is_empty(), "{out:?}");
    }

    /// A short page before a break is where the server's history ends:
    /// what is older comes from the cache, past the break.
    #[test]
    fn a_short_page_before_a_break_goes_on_in_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let chan = || Target::Channel("#test".into());
        let old: Vec<Line> = (1..=3).map(|i| line(&format!("o{i}"), i)).collect();
        let cache = cache_with(&dir, vec![(chan(), old), (chan(), vec![line("f1", 1000)])]);
        let mut s = restart(&cache, &["#test"]);
        let (_, label) = asked(&older_of(&mut s, &cache, "#test"));
        let out = page_at(&mut s, &cache, &label, &[("s1", 500)]);
        assert_eq!(got(&out), [(vec!["s1".to_owned()], false)]);
        // s1 isn't written yet (the writer runs on its own): read by its time.
        let out = older_of(&mut s, &cache, "#test");
        assert_eq!(got(&out), [(vec!["o1".to_owned(), "o2".to_owned(), "o3".to_owned()], false)]);
        assert!(sent(&out).is_empty(), "{out:?}");
        assert_eq!(got(&older_of(&mut s, &cache, "#test")), [(vec![], true)]);
    }

    /// With the server's history in full, its start is the start when the
    /// cache has nothing older either.
    #[test]
    fn an_empty_page_is_the_start_when_the_cache_has_nothing_older() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_with(&dir, vec![(Target::Channel("#test".into()), vec![line("f1", 1000)])]);
        let mut s = restart(&cache, &["#test"]);
        let (_, label) = asked(&older_of(&mut s, &cache, "#test"));
        assert_eq!(got(&page_at(&mut s, &cache, &label, &[])), [(vec![], true)]);
    }

    #[test]
    fn a_cache_that_will_not_open_is_one_line_in_each_server_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain");
        std::fs::write(&file, "not a directory").unwrap();
        // A server on a local port nothing listens on: no name to look up.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut config = Config::default();
        config.servers.insert("net".into(), Server { server: "127.0.0.1".into(), port: Some(port), use_tls: false, ..Server::default() });
        let (_tx, mut rx, flush) = spawn(config, Some(file.join("history.db")));
        let Some(Event::Line { network, target: Target::Server, line }) = rx.blocking_recv() else { panic!("no error line") };
        assert_eq!(network, "net");
        assert!(line.text.starts_with("No history cache"), "{}", line.text);
        assert_eq!(line.kind, LineKind::Error);
        // hirc runs on without one.
        assert!(matches!(rx.blocking_recv(), Some(Event::Status { status: ConnStatus::Connecting, .. })));
        assert!(flush.wait());
    }

    #[test]
    fn flush_waits_for_what_the_network_queued() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let (_tx, _rx, flush) = spawn(Config::default(), Some(path.clone()));
        assert!(flush.wait());
        assert!(path.exists());
        let (_tx, _rx, flush) = spawn(Config::default(), None);
        assert!(flush.wait());
        drop(Cache::open(&path).unwrap());
    }

    #[test]
    fn a_window_opened_while_offline_is_a_channel_by_the_servers_chantypes() {
        let mut isupport = Isupport::default();
        isupport.apply(&["alice".into(), "CHANTYPES=#+".into(), "are supported".into()]);
        let (tx, mut rx) = unbounded_channel();
        for target in ["+local", "bob"] {
            tx.send(Command::Open { network: "net".into(), target: target.into() }).unwrap();
        }
        tx.send(Command::Reconnect { network: "net".into() }).unwrap();
        let mut book = Book::default();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert!(rt.block_on(wait_reconnect(&mut rx, &mut book, &isupport)).is_some());
        assert_eq!(book.queries(), ["bob"]);
        assert!(book.get("+local").is_some());
    }

    #[test]
    fn a_quiet_server_is_pinged_then_given_up_on_as_configured() {
        let s = |ping_time, ping_timeout| idle_limits(&Server { ping_time, ping_timeout, ..Server::default() });
        assert_eq!(s(None, None), (PING_TIME, PING_TIME + PING_TIMEOUT, IDLE_CHECK));
        assert_eq!(s(Some(180), Some(20)), (Duration::from_secs(180), Duration::from_secs(200), Duration::from_secs(10)));
        // Short ones are looked at often enough to keep to them.
        assert_eq!(s(Some(1), Some(1)), (Duration::from_secs(1), Duration::from_secs(2), Duration::from_millis(500)));
        // 0 is no setting.
        assert_eq!(s(Some(0), Some(0)), s(None, None));
    }

    #[test]
    fn backoff_doubles_until_registered_then_resets() {
        let (d, b) = retry_delay(MIN_BACKOFF, false);
        assert_eq!((d, b), (Duration::from_secs(1), Duration::from_secs(2)));
        let (d, b) = retry_delay(Duration::from_secs(32), false);
        assert_eq!((d, b), (Duration::from_secs(32), MAX_BACKOFF));
        let (d, b) = retry_delay(MAX_BACKOFF, true);
        assert_eq!((d, b), (MIN_BACKOFF, Duration::from_secs(2)));
    }

    #[test]
    fn bad_root_cert_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(tls_config(Some(&dir.path().join("missing.pem"))).is_err());
        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        assert!(tls_config(Some(&empty)).is_err());
        std::fs::write(&empty, "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n").unwrap();
        assert!(tls_config(Some(&empty)).is_err());
    }

    #[test]
    fn merge_rejoin_dedups_case_insensitively_on_channel_name() {
        let configured = vec!["#a key".to_string(), "#b".to_string()];
        let rejoin = vec!["#A".to_string(), "#c".to_string()];
        assert_eq!(merge_channels(&configured, &rejoin), ["#a key", "#b", "#c"]);
    }

    /// A ZNC line said at 10:00:`secs`.500, live or (in `batch`) replayed.
    fn znc_line(s: &mut Session, batch: Option<&str>, text: &str, secs: u32) -> Vec<Output> {
        let tag = batch.map_or(String::new(), |b| format!("batch={b};"));
        feed(s, &format!("@{tag}time=2026-10-22T10:00:{secs:02}.500Z :bob!b@h PRIVMSG #test :{text}"))
    }

    /// ZNC's replay of `#test`: `lines` (text, seconds) as one batch.
    fn znc_replay(s: &mut Session, lines: &[(&str, u32)]) -> Vec<Output> {
        feed(s, ":irc.znc.in BATCH +p znc.in/playback #test");
        for (text, secs) in lines {
            znc_line(s, Some("p"), text, *secs);
        }
        feed(s, ":irc.znc.in BATCH -p")
    }

    fn brk_of(dir: &tempfile::TempDir, text: &str) -> bool {
        rusqlite::Connection::open(dir.path().join("history.db")).unwrap().query_row("SELECT brk FROM lines WHERE text = ?1", [text], |r| r.get(0)).unwrap()
    }

    /// Connected through ZNC and keeping a cache, `book` carried in.
    fn znc_keeping(book: Book) -> (Session, Vec<Output>) {
        let (s, out) = zncd(book);
        (s.keeping(), out)
    }

    #[test]
    fn a_znc_replay_that_meets_what_we_hold_has_no_break() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let (mut s, _) = znc_keeping(Book::default());
        znc_line(&mut s, None, "seen 1", 1);
        znc_line(&mut s, None, "seen 2", 2);
        assert!(written(&mut s, &cache));
        // Back: PLAY from seen 2's second, so ZNC sends seen 2 again first.
        let (mut s, out) = znc_keeping(s.take_book());
        assert_eq!(sent(&out).into_iter().filter(|m| m.contains("PLAY")).collect::<Vec<_>>(), ["PRIVMSG *playback :PLAY * 1792663202"]);
        znc_replay(&mut s, &[("seen 2", 2), ("away 1", 3)]);
        assert!(written(&mut s, &cache));
        assert!(!brk_of(&dir, "seen 2"));
        assert!(!brk_of(&dir, "away 1"));
        // Without batches, every line is a replay of its own, and each
        // follows on from the one kept before it.
        znc_replay(&mut s, &[("away 2", 4)]);
        znc_replay(&mut s, &[("away 3", 5)]);
        assert!(written(&mut s, &cache));
        assert!(!brk_of(&dir, "away 2"));
        assert!(!brk_of(&dir, "away 3"));
    }

    /// ZNC test 6 with every line in one millisecond, as a burst through
    /// Ergo is: the cache's pages end inside that millisecond, and read
    /// back up after a restart, the lines are still in the order said.
    #[test]
    fn one_millisecond_reads_back_in_order_across_pages() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let (mut s, _) = znc_keeping(Book::default());
        for t in ["seen 1", "seen 2", "seen 3"] {
            znc_line(&mut s, None, t, 1);
        }
        assert!(written(&mut s, &cache));
        let (mut s, _) = znc_keeping(s.take_book());
        znc_replay(&mut s, &[("away 4", 1), ("away 5", 1), ("away 6", 1), ("away 7", 1), ("away 8", 1)]);
        assert!(written(&mut s, &cache));
        cache.write_history("net", false);
        assert!(cache.flush());
        let server = Server { channels: vec!["#test".into()], history_page_size: Some(3), ..Server::default() };
        let mut book = Book::default();
        let mut store = crate::store::Store::new(["net".to_owned()]);
        for e in seed("net", &server, &cache, &mut book) {
            store.apply(e);
        }
        let (mut s, _) = znc_keeping(book);
        for _ in 0..4 {
            for o in older_of(&mut s, &cache, "#test") {
                if let Output::Event(e) = o {
                    store.apply(e);
                }
            }
        }
        let b = store.buffer_ix(0, &Target::Channel("#test".into()));
        let said: Vec<&str> = store.networks[0].buffers[b].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(said, ["seen 1", "seen 2", "seen 3", "away 4", "away 5", "away 6", "away 7", "away 8"]);
    }

    #[test]
    fn a_znc_replay_past_what_we_hold_is_a_break_that_the_cache_reads_past() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Shared::open(&dir.path().join("history.db")).unwrap();
        let (mut s, _) = znc_keeping(Book::default());
        znc_line(&mut s, None, "seen 1", 1);
        znc_line(&mut s, None, "seen 2", 2);
        assert!(written(&mut s, &cache));
        // ZNC's buffer ran out while we were away: its replay starts later.
        let (mut s, _) = znc_keeping(s.take_book());
        znc_replay(&mut s, &[("away 5", 10), ("away 6", 11)]);
        assert!(written(&mut s, &cache));
        assert!(brk_of(&dir, "away 5"));
        assert!(!brk_of(&dir, "away 6"));
        // A restart. The last connection had no server history, so the
        // cache is read past its breaks.
        cache.write_history("net", false);
        assert!(cache.flush());
        let server = Server { channels: vec!["#test".into()], history_page_size: Some(2), ..Server::default() };
        let mut book = Book::default();
        let seeded: Vec<Vec<String>> = seed("net", &server, &cache, &mut book)
            .into_iter()
            .map(|e| match e {
                Event::History { lines, .. } => lines.into_iter().map(|l| l.text).collect(),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(seeded, [vec!["away 5".to_owned(), "away 6".to_owned()]]);
        let (mut s, _) = znc_keeping(book);
        assert_eq!(got(&older_of(&mut s, &cache, "#test")), [(vec!["seen 1".to_owned(), "seen 2".to_owned()], false), (vec![], true)]);
    }

    #[test]
    fn irccloud_gets_the_client_id_in_its_password_and_replays() {
        let irccloud = |password: &str, id: Option<&str>| {
            let s =
                Server { server: "bnc.irccloud.com".into(), password: Some(password.into()), bouncer_client_id: id.map(str::to_owned), ..Server::default() };
            let c = session_config(&s).unwrap();
            (c.password.unwrap(), c.self_replays)
        };
        assert_eq!(irccloud("bnc:testkey", Some("laptop")), ("bnc@laptop:testkey".to_owned(), true));
        // No id: as IRCCloud gave it, and no replay.
        assert_eq!(irccloud("bnc:testkey", None), ("bnc:testkey".to_owned(), false));
        // An id written into the password by hand replays too.
        assert_eq!(irccloud("bnc@desk:testkey", None), ("bnc@desk:testkey".to_owned(), true));
        // Any other server's password goes as it is, id or not.
        let other =
            Server { server: "irc.example.org".into(), password: Some("bnc:testkey".into()), bouncer_client_id: Some("laptop".into()), ..Server::default() };
        let c = session_config(&other).unwrap();
        assert_eq!((c.password.as_deref(), c.self_replays), (Some("bnc:testkey"), false));
    }
}
