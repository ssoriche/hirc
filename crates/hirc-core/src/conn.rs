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
    Closed {
        registered: bool,
    },
}

async fn run_server(name: String, server: Server, mut cmds: UnboundedReceiver<Command>, events: UnboundedSender<Event>) {
    let status = |status| {
        let _ = events.send(Event::Status { network: name.clone(), status });
    };
    let mut backoff = Duration::from_secs(1);
    loop {
        status(ConnStatus::Connecting);
        match connect_once(&name, &server, &mut cmds, &events).await {
            Ok(End::Stopped(reason)) => {
                status(ConnStatus::Failed { reason });
                if wait_reconnect(&mut cmds).await.is_none() {
                    return;
                }
                backoff = Duration::from_secs(1);
                continue;
            }
            Ok(End::Closed { registered }) => {
                if registered {
                    backoff = Duration::from_secs(1);
                }
                status(ConnStatus::Disconnected { reason: "connection closed".into() });
            }
            Err(e) => status(ConnStatus::Disconnected { reason: e.to_string() }),
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            r = wait_reconnect(&mut cmds) => {
                if r.is_none() {
                    return;
                }
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
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

fn tls_config(root_cert: Option<&Path>) -> Result<Arc<ClientConfig>, Error> {
    let config = match root_cert {
        Some(path) => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_file_iter(path)? {
                roots.add(cert?)?;
            }
            ClientConfig::builder().with_root_certificates(roots).with_no_client_auth()
        }
        None => ClientConfig::with_platform_verifier()?,
    };
    Ok(Arc::new(config))
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

async fn connect_once(name: &str, server: &Server, cmds: &mut UnboundedReceiver<Command>, events: &UnboundedSender<Event>) -> Result<End, Error> {
    if matches!(server.sasl, Some(Sasl::External { .. })) {
        return Ok(End::Stopped("SASL EXTERNAL isn't supported yet".into()));
    }
    let s = server.clone();
    let cfg = match tokio::task::spawn_blocking(move || session_config(&s)).await? {
        Ok(cfg) => cfg,
        Err(e) => return Ok(End::Stopped(e.to_string())),
    };
    let host = server.server.clone();
    let tcp = tokio::time::timeout(Duration::from_secs(30), TcpStream::connect((host.as_str(), server.port()))).await??;
    let stream: Box<dyn Stream> = if server.use_tls {
        let connector = TlsConnector::from(tls_config(server.root_cert_path.as_deref())?);
        Box::new(connector.connect(ServerName::try_from(host)?, tcp).await?)
    } else if is_loopback(&host) {
        Box::new(tcp)
    } else {
        return Ok(End::Stopped("plaintext is only allowed to localhost; set use_tls = true".into()));
    };
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = LineReader::new(BufReader::new(r));
    let mut session = Session::new(name, cfg);
    let mut registered = false;
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
                        registered = true;
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
                let Some(line) = line? else { return Ok(End::Closed { registered }) };
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
                    return Ok(End::Closed { registered: true });
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
