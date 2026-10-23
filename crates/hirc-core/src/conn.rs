//! Sockets. One thread runs a tokio runtime with a task per server. Each
//! task connects, feeds lines through its `Session`, sends events to the
//! UI, and reconnects with backoff. The UI never waits on any of it.

use crate::config::{self, Config, ConfigError, Sasl, Server};
use crate::event::{Command, ConnStatus, Event};
use crate::proto::Message;
use crate::session::{Output, SaslPlain, Session, SessionConfig};
use crate::wire::LineReader;
use chrono::Utc;
use rustls::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls_platform_verifier::ConfigVerifierExt;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_rustls::TlsConnector;

type Error = Box<dyn std::error::Error + Send + Sync>;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Send a PING after this long without hearing from the server...
const IDLE_PING: Duration = Duration::from_secs(120);
/// ...and give up on the connection after this long.
const IDLE_DEAD: Duration = Duration::from_secs(300);

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// Starts the network thread. Send `Command`s in; `Event`s come out.
pub fn spawn(config: Config) -> (UnboundedSender<Command>, UnboundedReceiver<Event>) {
    let (cmd_tx, mut cmd_rx) = unbounded_channel::<Command>();
    let (ev_tx, ev_rx) = unbounded_channel::<Event>();
    std::thread::Builder::new()
        .name("hirc-net".into())
        .spawn(move || {
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
                    tokio::spawn(run_server(name, server, rx, ev_tx.clone()));
                }
                while let Some(cmd) = cmd_rx.recv().await {
                    if let Some(tx) = routes.get(cmd.network()) {
                        let _ = tx.send(cmd);
                    }
                }
            });
        })
        .expect("spawn network thread");
    (cmd_tx, ev_rx)
}

enum End {
    /// Don't reconnect until /reconnect.
    Stopped(String),
    Closed,
}

/// The delay before the next attempt and the backoff to carry forward. A
/// session that got as far as registering resets the backoff, however it
/// ended (clean close, reset, TLS error, idle timeout).
fn retry_delay(backoff: Duration, registered: bool) -> (Duration, Duration) {
    let delay = if registered { MIN_BACKOFF } else { backoff };
    (delay, (delay * 2).min(MAX_BACKOFF))
}

async fn run_server(name: String, server: Server, mut cmds: UnboundedReceiver<Command>, events: UnboundedSender<Event>) {
    let status = |status| {
        let _ = events.send(Event::Status { network: name.clone(), status });
    };
    let mut backoff = MIN_BACKOFF;
    // Resolved secrets live for the process so reconnects don't re-run
    // `password_command` (each run can prompt). A Stop clears them.
    let mut cached: Option<SessionConfig> = None;
    // Channels joined at runtime, rejoined after a reconnect.
    let mut rejoin: Vec<String> = Vec::new();
    loop {
        status(ConnStatus::Connecting);
        let mut registered = false;
        match connect_once(&name, &server, &mut cached, &mut rejoin, &mut registered, &mut cmds, &events).await {
            Ok(End::Stopped(reason)) => {
                cached = None;
                status(ConnStatus::Failed { reason });
                if wait_reconnect(&mut cmds).await.is_none() {
                    return;
                }
                backoff = MIN_BACKOFF;
                continue;
            }
            Ok(End::Closed) => status(ConnStatus::Disconnected { reason: "connection closed".into() }),
            Err(e) => status(ConnStatus::Disconnected { reason: e.to_string() }),
        }
        let (delay, next) = retry_delay(backoff, registered);
        backoff = next;
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            r = wait_reconnect(&mut cmds) => {
                if r.is_none() {
                    return;
                }
            }
        }
    }
}

/// Waits for /reconnect, dropping other commands (there's nowhere to send
/// them). `None` when the app is shutting down.
async fn wait_reconnect(cmds: &mut UnboundedReceiver<Command>) -> Option<()> {
    loop {
        match cmds.recv().await? {
            Command::Reconnect { .. } => return Some(()),
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
    Ok(SessionConfig {
        nickname: s.nickname.clone(),
        alt_nicks: s.alt_nicks.clone(),
        username: s.username.clone().unwrap_or_else(|| s.nickname.clone()),
        realname: s.realname.clone().unwrap_or_else(|| s.nickname.clone()),
        password: config::secret(s.password.as_deref(), s.password_file.as_deref(), s.password_command.as_deref())?,
        sasl,
        channels: s.channels.clone(),
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
    rejoin: &mut Vec<String>,
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
    cfg.channels = merge_channels(&cfg.channels, rejoin);
    let mut session = Session::new(name, cfg);
    let mut outs = session.start();
    let mut last_rx = Instant::now();
    let mut pinged = false;
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        for out in outs.drain(..) {
            match out {
                Output::Send(m) => {
                    w.write_all(m.to_wire().as_bytes()).await?;
                    w.write_all(b"\r\n").await?;
                }
                Output::Event(e) => {
                    if matches!(e, Event::Status { status: ConnStatus::Connected { .. }, .. }) {
                        *registered = true;
                    }
                    match &e {
                        Event::Joined { channel, .. } if !rejoin.iter().any(|c| c.eq_ignore_ascii_case(channel)) => rejoin.push(channel.clone()),
                        Event::Parted { channel, .. } => rejoin.retain(|c| !c.eq_ignore_ascii_case(channel)),
                        _ => {}
                    }
                    let _ = events.send(e);
                }
                Output::Stop(reason) => {
                    let _ = w.flush().await;
                    return Ok(End::Stopped(reason));
                }
            }
        }
        w.flush().await?;
        tokio::select! {
            line = reader.next_line() => {
                let Some(line) = line? else { return Ok(End::Closed) };
                last_rx = Instant::now();
                pinged = false;
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
                    return Ok(End::Closed);
                }
                Some(c) => outs = session.command(c, Utc::now()),
            },
            _ = tick.tick() => {
                let idle = last_rx.elapsed();
                if idle > IDLE_DEAD {
                    return Err("no reply from server".into());
                }
                if idle > IDLE_PING && !pinged {
                    pinged = true;
                    outs = vec![Output::Send(Message::new("PING", ["hirc"]))];
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
