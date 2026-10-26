//! A throwaway Ergo per test, plus a bare-socket IRC client to talk to
//! hirc through it. Needs `scripts/ergo-test.sh setup`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

pub struct Ergo {
    child: Child,
    _dir: tempfile::TempDir,
    pub plain_port: u16,
    pub tls_port: u16,
    pub ca: PathBuf,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl Ergo {
    pub fn start() -> Ergo {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let state = root.join("target/ergo");
        let bin = state.join("ergo");
        assert!(bin.exists(), "run scripts/ergo-test.sh setup first");
        let dir = tempfile::tempdir().unwrap();
        let (plain_port, tls_port) = (free_port(), free_port());
        let yaml = std::fs::read_to_string(root.join("tests/ergo/ircd.yaml.in"))
            .unwrap()
            .replace("@PLAIN_PORT@", &plain_port.to_string())
            .replace("@TLS_PORT@", &tls_port.to_string());
        std::fs::write(dir.path().join("ircd.yaml"), yaml).unwrap();
        for f in ["fullchain.pem", "privkey.pem"] {
            std::fs::copy(state.join(f), dir.path().join(f)).unwrap();
        }
        let init = Command::new(&bin).args(["initdb", "--conf", "ircd.yaml", "--quiet"]).current_dir(dir.path()).status().unwrap();
        assert!(init.success(), "ergo initdb failed");
        let child = Command::new(&bin)
            .args(["run", "--conf", "ircd.yaml", "--quiet"])
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Owned by the guard from here on, so Drop kills it on any panic too.
        let ergo = Ergo { child, _dir: dir, plain_port, tls_port, ca: state.join("ca.pem") };
        let mut ergo = ergo;
        for _ in 0..100 {
            if let Some(status) = ergo.child.try_wait().unwrap() {
                panic!("ergo exited during startup: {status}");
            }
            if std::net::TcpStream::connect(("127.0.0.1", tls_port)).is_ok() && std::net::TcpStream::connect(("127.0.0.1", plain_port)).is_ok() {
                return ergo;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("ergo did not start");
    }
}

impl Drop for Ergo {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A plaintext loopback client that speaks just enough IRC.
pub struct Raw {
    r: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
}

impl Raw {
    pub async fn connect(port: u16, nick: &str) -> Raw {
        let (r, w) = TcpStream::connect(("127.0.0.1", port)).await.unwrap().into_split();
        let mut c = Raw { r: BufReader::new(r), w };
        c.send(&format!("NICK {nick}")).await;
        c.send(&format!("USER {nick} 0 * :{nick}")).await;
        c.expect(" 001 ").await;
        c
    }

    pub async fn send(&mut self, line: &str) {
        self.w.write_all(format!("{line}\r\n").as_bytes()).await.unwrap();
    }

    /// Reads until a line contains `needle` (answering PINGs). Panics after 10s.
    pub async fn expect(&mut self, needle: &str) -> String {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                if self.r.read_line(&mut line).await.unwrap() == 0 {
                    panic!("connection closed waiting for {needle:?}");
                }
                if let Some(rest) = line.strip_prefix("PING ") {
                    self.send(&format!("PONG {}", rest.trim_end())).await;
                }
                if line.contains(needle) {
                    return line;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"))
    }
}

impl Raw {
    /// The value of ISUPPORT token `name` (`CHATHISTORY=100` -> `"100"`),
    /// reading on from where the connection is (right after 001, the 005s).
    pub async fn isupport_value(&mut self, name: &str) -> String {
        let line = self.expect(&format!(" {name}=")).await;
        let token = line.split_whitespace().find_map(|t| t.strip_prefix(&format!("{name}="))).unwrap_or_default();
        token.to_owned()
    }
}

/// Registers `nick` with NickServ, then disconnects.
pub async fn register_account(port: u16, nick: &str, password: &str) {
    let mut c = Raw::connect(port, nick).await;
    c.send(&format!("PRIVMSG NickServ :REGISTER {password}")).await;
    c.expect("Account created").await;
    c.send("QUIT").await;
}

/// Registers `nick` and makes it always-on (Ergo keeps its client, its
/// history and its read markers while no connection is up), then
/// disconnects.
pub async fn register_always_on(port: u16, nick: &str, password: &str) {
    let mut c = Raw::connect(port, nick).await;
    c.send(&format!("PRIVMSG NickServ :REGISTER {password}")).await;
    c.expect("Account created").await;
    c.send("PRIVMSG NickServ :SET always-on true").await;
    // Ergo 2.19.1 answers with the stored setting, then whether the
    // server's settings make it so: the second is the one that counts.
    c.expect("your client is always-on").await;
    c.send("QUIT").await;
}
