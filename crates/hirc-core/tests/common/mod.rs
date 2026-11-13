//! A throwaway Ergo per test, plus a bare-socket IRC client to talk to
//! hirc through it. Needs `scripts/ergo-test.sh setup`.

#![allow(dead_code)]

pub mod events;
pub mod proxy;
pub mod soju;
pub mod znc;

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
    /// A socket with nothing sent yet: for a bouncer that wants PASS and
    /// CAP before NICK.
    pub async fn open(port: u16) -> Raw {
        let (r, w) = TcpStream::connect(("127.0.0.1", port)).await.unwrap().into_split();
        Raw { r: BufReader::new(r), w }
    }

    pub async fn connect(port: u16, nick: &str) -> Raw {
        let mut c = Raw::open(port).await;
        c.send(&format!("NICK {nick}")).await;
        c.send(&format!("USER {nick} 0 * :{nick}")).await;
        c.expect(" 001 ").await;
        c
    }

    pub async fn send(&mut self, line: &str) {
        self.w.write_all(format!("{line}\r\n").as_bytes()).await.unwrap();
    }

    /// Several lines in one write, so the server reads them together (one
    /// write per line lets a slow machine stamp the first a few ms early).
    pub async fn send_burst(&mut self, lines: &[String]) {
        let all: String = lines.iter().map(|l| format!("{l}\r\n")).collect();
        self.w.write_all(all.as_bytes()).await.unwrap();
    }

    /// The next line, without its CRLF (answering a PING). Panics after 10s.
    pub async fn next(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut line = String::new();
            if self.r.read_line(&mut line).await.unwrap() == 0 {
                panic!("connection closed");
            }
            let line = line.trim_end_matches(['\r', '\n']).to_owned();
            if let Some(rest) = line.strip_prefix("PING ") {
                self.send(&format!("PONG {rest}")).await;
            }
            line
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a line"))
    }

    /// Every line up to and including the first that contains `needle`.
    pub async fn until(&mut self, needle: &str) -> Vec<String> {
        let mut got = Vec::new();
        loop {
            let line = self.next().await;
            let done = line.contains(needle);
            got.push(line);
            if done {
                return got;
            }
        }
    }

    /// Reads until a line contains `needle` (answering PINGs). Panics after 10s.
    pub async fn expect(&mut self, needle: &str) -> String {
        loop {
            let line = self.next().await;
            if line.contains(needle) {
                return line;
            }
        }
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

/// The caps a server lists in its `CAP LS` lines, without values.
fn offered(ls: &[String]) -> Vec<String> {
    ls.iter()
        .filter_map(|l| l.split_once(" LS ").map(|(_, rest)| rest))
        .flat_map(|rest| rest.trim_start_matches("* ").trim_start_matches(':').split_whitespace())
        .map(|c| c.split('=').next().unwrap_or(c).to_owned())
        .collect()
}

/// Connects to a bouncer the way a client does:
/// - `CAP LS 302`
/// - `PASS`
/// - a `CAP REQ` for the caps in `want` that it offers
/// - `NICK`, `USER`, `CAP END`
///
/// It reads up to the end of the MOTD and gives back the client and every
/// line it got, the CAP LS lines first. PASS is never in what it gives back.
pub async fn register(port: u16, pass: &str, nick: &str, user: &str, want: &[&str]) -> (Raw, Vec<String>) {
    let mut c = Raw::open(port).await;
    c.send("CAP LS 302").await;
    let mut got = Vec::new();
    loop {
        let line = c.next().await;
        let last = line.contains(" LS ") && !line.contains(" LS * ");
        got.push(line);
        if last {
            break;
        }
    }
    let offered = offered(&got);
    let req: Vec<&str> = want.iter().copied().filter(|w| offered.iter().any(|o| o == w)).collect();
    c.send(&format!("PASS {pass}")).await;
    if !req.is_empty() {
        c.send(&format!("CAP REQ :{}", req.join(" "))).await;
    }
    c.send(&format!("NICK {nick}")).await;
    c.send(&format!("USER {user} 0 * :{nick}")).await;
    if !req.is_empty() {
        got.extend(c.until(" ACK ").await);
    }
    c.send("CAP END").await;
    loop {
        let line = c.next().await;
        let done = [" 376 ", " 422 "].iter().any(|n| line.contains(n));
        got.push(line);
        if done {
            return (c, got);
        }
    }
}

/// Waits until `nick` is connected to `ergo` (a bouncer's own client),
/// asking with ISON every 100 ms for up to 10 s.
pub async fn wait_online(ergo: &Ergo, nick: &str) {
    let mut w = Raw::connect(ergo.plain_port, &format!("w{}", free_port())).await;
    for _ in 0..100 {
        w.send(&format!("ISON {nick}")).await;
        let reply = w.expect(" 303 ").await;
        if reply.rsplit(':').next().is_some_and(|names| names.split_whitespace().any(|n| n.eq_ignore_ascii_case(nick))) {
            w.send("QUIT").await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{nick} never came online");
}

/// Waits until `nick` is in `chan` on `ergo`, asking with NAMES every
/// 100 ms for up to 10 s.
pub async fn wait_in_channel(ergo: &Ergo, nick: &str, chan: &str) {
    let mut w = Raw::connect(ergo.plain_port, &format!("w{}", free_port())).await;
    for _ in 0..100 {
        w.send(&format!("NAMES {chan}")).await;
        let lines = w.until(" 366 ").await;
        let here = lines
            .iter()
            .filter(|l| l.contains(" 353 "))
            .any(|l| l.rsplit(':').next().is_some_and(|names| names.split_whitespace().any(|n| n.trim_start_matches(['@', '+']).eq_ignore_ascii_case(nick))));
        if here {
            w.send("QUIT").await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{nick} never joined {chan}");
}

/// Writes a transcript to `tests/data/<name>`: the server's lines as they
/// came, and ours marked `> `. It refuses any line that holds a test
/// password, and PASS is never one of the lines.
pub fn record(name: &str, lines: &[String]) {
    for l in lines {
        assert!(!l.contains(znc::PASSWORD) && !l.contains(soju::PASSWORD) && !l.starts_with("> PASS"), "a password in the transcript for {name}");
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name);
    std::fs::write(path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
}
