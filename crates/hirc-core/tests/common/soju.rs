//! A throwaway soju 0.11 in front of a test Ergo: one user with one
//! network, already in `#test`. Needs the flox environment (`soju` and
//! `sojuctl` on PATH).

use super::{Ergo, free_port, register, wait_in_channel, wait_online};
use std::fs::File;
use std::process::{Child, Command};
use std::time::Duration;

pub const USER: &str = "alice";
pub const NETWORK: &str = "ergo";
/// A fixed test password: never a real one, never from `dot_config/`.
pub const PASSWORD: &str = "soju-test-only";

pub struct Soju {
    child: Child,
    dir: tempfile::TempDir,
    pub port: u16,
}

/// The username a client gives: `user/network` binds the connection to
/// that one network.
pub fn username() -> String {
    format!("{USER}/{NETWORK}")
}

impl Soju {
    pub async fn start(ergo: &Ergo) -> Soju {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let conf = dir.path().join("soju.conf");
        std::fs::write(
            &conf,
            format!(
                "listen irc+insecure://127.0.0.1:{port}\nlisten unix+admin://{admin}\ndb sqlite3 {db}\nmessage-store db\nhostname localhost\n",
                admin = dir.path().join("admin.sock").display(),
                db = dir.path().join("soju.db").display(),
            ),
        )
        .unwrap();
        let log = File::create(dir.path().join("soju.log")).unwrap();
        let child = Command::new("soju")
            .arg("-config")
            .arg(&conf)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("soju on PATH: run inside `flox activate`");
        // Owned by the guard from here on, so Drop kills it on any panic too.
        let mut soju = Soju { child, dir, port };
        soju.wait_listening();
        soju.ctl(&["user", "create", "-username", USER, "-password", PASSWORD]);
        let addr = format!("irc+insecure://127.0.0.1:{}", ergo.plain_port);
        soju.ctl(&["user", "run", USER, "network", "create", "-addr", &addr, "-name", NETWORK, "-nick", USER]);
        wait_online(ergo, USER).await;
        // Joined once through soju, so soju stays in the channel from now on.
        let (mut c, _) = register(port, PASSWORD, USER, &username(), &[]).await;
        c.send("JOIN #test").await;
        c.expect(" JOIN ").await;
        c.send("QUIT").await;
        wait_in_channel(ergo, USER, "#test").await;
        soju
    }

    fn wait_listening(&mut self) {
        for _ in 0..100 {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("soju exited during startup ({status}):\n{}", self.log());
            }
            if self.dir.path().join("admin.sock").exists() && std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("soju did not start:\n{}", self.log());
    }

    /// `sojuctl` against this soju's admin socket. On failure it shows the
    /// first two words of the command, never the rest (which may hold the
    /// password).
    fn ctl(&self, args: &[&str]) {
        let out = Command::new("sojuctl").arg("-config").arg(self.dir.path().join("soju.conf")).args(args).output().expect("sojuctl on PATH");
        assert!(
            out.status.success(),
            "sojuctl {} failed: {}{}\n{}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            self.log()
        );
    }

    /// Waits until soju's store holds every one of `texts` in `target`'s
    /// history. soju stores a line some time after it relays it, so a test
    /// asks it (with LATEST) instead of guessing how long that takes.
    /// Panics after 5s.
    pub async fn holds(&self, target: &str, texts: &[&str]) {
        let (mut c, _) = register(self.port, PASSWORD, USER, &username(), &["batch", "server-time", "message-tags", "draft/chathistory"]).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            c.send(&format!("CHATHISTORY LATEST {target} * 100")).await;
            c.send("PING :soju-holds").await;
            let got = c.until("soju-holds").await;
            let said: Vec<String> = got
                .iter()
                .filter_map(|l| hirc_core::proto::Message::parse(l).ok())
                .filter(|m| m.command == "PRIVMSG")
                .filter_map(|m| m.params.last().cloned())
                .collect();
            if texts.iter().all(|t| said.iter().any(|s| s == t)) {
                c.send("QUIT").await;
                return;
            }
            assert!(std::time::Instant::now() < deadline, "soju never stored {texts:?} in {target}: {got:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// What soju has logged, for a failing test to show.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("soju.log")).unwrap_or_default()
    }
}

impl Drop for Soju {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
