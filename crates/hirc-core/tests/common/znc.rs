//! A throwaway ZNC 1.10 with its playback module, in front of a test
//! Ergo: one user, one network, already in `#test`. Needs the flox
//! environment (`znc` on PATH, `zncModules.playback` in `$FLOX_ENV`).

use super::{Ergo, free_port};
use std::fs::File;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

pub const USER: &str = "alice";
pub const NETWORK: &str = "ergo";
/// A fixed test password: never a real one, never from `dot_config/`.
pub const PASSWORD: &str = "znc-test-only";

pub struct Znc {
    child: Child,
    dir: tempfile::TempDir,
    pub port: u16,
}

/// hirc's `PASS` for ZNC: `user/network:password`.
pub fn pass() -> String {
    format!("{USER}/{NETWORK}:{PASSWORD}")
}

/// The playback module flox installed (`zncModules.playback`).
fn playback_module() -> PathBuf {
    let env = std::env::var_os("FLOX_ENV").map(PathBuf::from).expect("run inside `flox activate`");
    let so = env.join("lib/znc/playback.so");
    assert!(so.exists(), "no playback module at {}: is zncModules.playback installed?", so.display());
    so
}

/// One user and one network pointing at the test Ergo. Buffers are kept
/// after playback (the playback module asks for that), and are `buffer`
/// lines long. playback is a global module: ZNC 1.10.3 refuses it as a
/// user or network one.
fn conf(port: u16, ergo_port: u16, buffer: usize) -> String {
    format!(
        "Version = 1.10.3
ConnectDelay = 1
ServerThrottle = 1
LoadModule = playback
<Listener irc>
    Host = 127.0.0.1
    Port = {port}
    IPv4 = true
    IPv6 = false
    SSL = false
    AllowIRC = true
    AllowWeb = false
</Listener>
<User {USER}>
    Pass = plain#{PASSWORD}
    Nick = {USER}
    AltNick = {USER}_
    Ident = {USER}
    RealName = {USER}
    ChanBufferSize = {buffer}
    QueryBufferSize = {buffer}
    AutoClearChanBuffer = false
    AutoClearQueryBuffer = false
    <Network {NETWORK}>
        Server = 127.0.0.1 {ergo_port}
        <Chan #test>
        </Chan>
    </Network>
</User>
"
    )
}

impl Znc {
    pub fn start(ergo: &Ergo) -> Znc {
        Znc::with_buffer(ergo, 50)
    }

    /// A ZNC keeping `buffer` lines per channel and query.
    pub fn with_buffer(ergo: &Ergo, buffer: usize) -> Znc {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let (configs, modules) = (dir.path().join("configs"), dir.path().join("modules"));
        std::fs::create_dir_all(&configs).unwrap();
        std::fs::create_dir_all(&modules).unwrap();
        std::os::unix::fs::symlink(playback_module(), modules.join("playback.so")).unwrap();
        std::fs::write(configs.join("znc.conf"), conf(port, ergo.plain_port, buffer)).unwrap();
        let log = File::create(dir.path().join("znc.log")).unwrap();
        let child = Command::new("znc")
            .arg("--foreground")
            .arg("--datadir")
            .arg(dir.path())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("znc on PATH: run inside `flox activate`");
        // Owned by the guard from here on, so Drop kills it on any panic too.
        let mut znc = Znc { child, dir, port };
        for _ in 0..100 {
            if let Some(status) = znc.child.try_wait().unwrap() {
                panic!("znc exited during startup ({status}):\n{}", znc.log());
            }
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return znc;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("znc did not start:\n{}", znc.log());
    }

    /// What ZNC has logged, for a failing test to show. ZNC never logs
    /// the user's password.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("znc.log")).unwrap_or_default()
    }
}

impl Drop for Znc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
