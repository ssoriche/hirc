//! A TCP relay between hirc and a server that a test can cut on demand,
//! which is how a network that dies without a QUIT looks to both ends.
//! It can also hold or cut the connection when the client sends a line
//! that matches, to land a drop at one exact point of a reconnect, or go
//! silent, as a dead link does: nothing gets through, nothing is closed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

/// A client line held back until the test lets it go.
pub struct Hold {
    reached: Notify,
    release: Notify,
}

impl Hold {
    /// Waits until the client has sent the line (it is held, not yet at the
    /// server). Panics after 10s.
    pub async fn reached(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.reached.notified()).await.expect("the proxy never saw the line it was to hold");
    }

    /// Lets the held line, and what follows it, through.
    pub fn release(&self) {
        self.release.notify_one();
    }
}

enum Action {
    /// Pass the line on, then drop the connection before the server's
    /// answer can come back.
    Sever,
    Hold(Arc<Hold>),
}

struct Trap {
    pattern: String,
    /// Matching lines to let through first.
    skip: usize,
    action: Action,
}

struct Inner {
    upstream: u16,
    blocked: AtomicBool,
    /// Bytes either way are swallowed; every socket stays open.
    silent: AtomicBool,
    cut: watch::Sender<u64>,
    trap: Mutex<Option<Trap>>,
}

impl Inner {
    fn spring(&self, line: &str) -> Option<Action> {
        let mut trap = self.trap.lock().unwrap();
        let t = trap.as_mut().filter(|t| line.contains(&t.pattern))?;
        if t.skip > 0 {
            t.skip -= 1;
            return None;
        }
        trap.take().map(|t| t.action)
    }
}

pub struct Proxy {
    pub port: u16,
    inner: Arc<Inner>,
    task: JoinHandle<()>,
}

impl Proxy {
    /// Relays 127.0.0.1:`port` to 127.0.0.1:`upstream`.
    pub async fn start(upstream: u16) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let inner =
            Arc::new(Inner { upstream, blocked: AtomicBool::new(false), silent: AtomicBool::new(false), cut: watch::channel(0).0, trap: Mutex::new(None) });
        let shared = inner.clone();
        let task = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                // A connection made while the network is down is dropped
                // at once: hirc's connect works, then it reads end of file.
                if shared.blocked.load(Ordering::SeqCst) {
                    continue;
                }
                tokio::spawn(relay(shared.clone(), client, shared.cut.subscribe()));
            }
        });
        Proxy { port, inner, task }
    }

    /// The network dies: every connection is dropped with no QUIT, and new
    /// ones are refused until `up`.
    pub fn down(&self) {
        self.inner.blocked.store(true, Ordering::SeqCst);
        self.inner.cut.send_modify(|n| *n += 1);
    }

    /// The network is back: new connections get through, and nothing is
    /// swallowed any more.
    pub fn up(&self) {
        self.inner.blocked.store(false, Ordering::SeqCst);
        self.inner.silent.store(false, Ordering::SeqCst);
    }

    /// The link goes dead with nothing closed: no end of file, no reset,
    /// just no bytes either way (a dropped Wi-Fi, a sleeping NAT) until `up`.
    pub fn silence(&self) {
        self.inner.silent.store(true, Ordering::SeqCst);
    }

    /// Drops the connection when the client sends its `nth` (1-based) line
    /// that contains `pattern`: the line reaches the server, its answer
    /// never reaches the client.
    pub fn sever_on(&self, pattern: &str, nth: usize) {
        *self.inner.trap.lock().unwrap() = Some(Trap { pattern: pattern.into(), skip: nth - 1, action: Action::Sever });
    }

    /// Holds back the next client line that contains `pattern`.
    pub fn hold_on(&self, pattern: &str) -> Arc<Hold> {
        let hold = Arc::new(Hold { reached: Notify::new(), release: Notify::new() });
        *self.inner.trap.lock().unwrap() = Some(Trap { pattern: pattern.into(), skip: 0, action: Action::Hold(hold.clone()) });
        hold
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn relay(inner: Arc<Inner>, client: TcpStream, mut cut: watch::Receiver<u64>) {
    let Ok(server) = TcpStream::connect(("127.0.0.1", inner.upstream)).await else { return };
    let (cr, mut cw) = client.into_split();
    let (sr, mut sw) = server.into_split();
    let quiet = inner.clone();
    let mute = Arc::new(AtomicBool::new(false));
    let muted = mute.clone();
    let up = async move {
        let mut r = BufReader::new(cr);
        let mut line = Vec::new();
        loop {
            line.clear();
            if r.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                return;
            }
            if inner.silent.load(Ordering::SeqCst) {
                continue;
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            match inner.spring(&text) {
                Some(Action::Sever) => {
                    mute.store(true, Ordering::SeqCst);
                    let _ = sw.write_all(&line).await;
                    let _ = sw.flush().await;
                    return;
                }
                Some(Action::Hold(h)) => {
                    h.reached.notify_one();
                    h.release.notified().await;
                }
                None => {}
            }
            if sw.write_all(&line).await.is_err() {
                return;
            }
        }
    };
    let down = async move {
        let mut r = sr;
        let mut buf = [0u8; 8192];
        loop {
            let n = r.read(&mut buf).await.unwrap_or(0);
            if n == 0 || muted.load(Ordering::SeqCst) {
                return;
            }
            if !quiet.silent.load(Ordering::SeqCst) && cw.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    };
    // Whichever ends first, the connection is over: the rest is dropped,
    // and with it both sockets.
    tokio::select! {
        _ = up => {}
        _ = down => {}
        _ = cut.changed() => {}
    }
}
