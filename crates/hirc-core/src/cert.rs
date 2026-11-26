//! The client certificate hirc logs in with (SASL EXTERNAL): one PEM per
//! server, holding the certificate and its private key, readable only by
//! the user. The key never leaves this file except into rustls.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub struct Cert {
    pub path: PathBuf,
    /// SHA-256 of the certificate, lowercase hex: Ergo's `certfp`.
    pub fingerprint: String,
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl Clone for Cert {
    fn clone(&self) -> Cert {
        Cert { path: self.path.clone(), fingerprint: self.fingerprint.clone(), chain: self.chain.clone(), key: self.key.clone_key() }
    }
}

impl std::fmt::Debug for Cert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cert").field("path", &self.path).field("fingerprint", &self.fingerprint).field("key", &"<redacted>").finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("can't read {}: {source}", path.display())]
    Read { path: PathBuf, source: std::io::Error },
    #[error("can't write {}: {source}", path.display())]
    Write { path: PathBuf, source: std::io::Error },
    #[error("no certificate in {}", .0.display())]
    NoCert(PathBuf),
    #[error("no private key in {}", .0.display())]
    NoKey(PathBuf),
    #[error("can't make a certificate: {0}")]
    Make(String),
}

/// `$XDG_DATA_HOME/hirc/certs`, else `~/.local/share/hirc/certs`.
pub fn dir() -> PathBuf {
    dir_from(std::env::var_os("XDG_DATA_HOME"), dirs::home_dir())
}

/// `dir` from the environment it is given. A relative `XDG_DATA_HOME` is
/// ignored, as XDG says.
pub fn dir_from(xdg: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    let base = xdg.map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.unwrap_or_default().join(".local/share"));
    base.join("hirc").join("certs")
}

/// The file for `network` (the config's server name) in `dir`.
pub fn file_in(dir: &Path, network: &str) -> PathBuf {
    let safe: String = network.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    dir.join(format!("{safe}.pem"))
}

pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// A new self-signed certificate for `nick` at `path`, replacing any there.
pub fn make(path: &Path, nick: &str) -> Result<Cert, CertError> {
    let fail = |e: rcgen::Error| CertError::Make(e.to_string());
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(fail)?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).map_err(fail)?;
    params.distinguished_name.push(rcgen::DnType::CommonName, nick);
    let today = chrono::Utc::now().date_naive();
    use chrono::Datelike;
    // Day 28 at the latest: every month has one.
    let (m, d) = (today.month() as u8, today.day().min(28) as u8);
    params.not_before = rcgen::date_time_ymd(today.year(), m, d);
    params.not_after = rcgen::date_time_ymd(today.year() + 10, m, d);
    let cert = params.self_signed(&key).map_err(fail)?;
    let pem = format!("{}{}", cert.pem(), key.serialize_pem());
    write_private(path, pem.as_bytes())?;
    load(path)
}

/// Written beside, private from the start, then renamed over `path`.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CertError> {
    let werr = |source| CertError::Write { path: path.to_owned(), source };
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(werr)?;
    }
    let tmp = path.with_extension("pem.new");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(werr)?;
    f.write_all(bytes).and_then(|_| f.sync_all()).map_err(werr)?;
    std::fs::rename(&tmp, path).map_err(werr)
}

/// The certificate(s) and key in one PEM file.
pub fn load(path: &Path) -> Result<Cert, CertError> {
    load_parts(path, None)
}

/// `cert`'s certificates and the key in `key`, or in `cert` when `None`.
pub fn load_parts(cert: &Path, key: Option<&Path>) -> Result<Cert, CertError> {
    let read = |p: &Path| std::fs::read(p).map_err(|source| CertError::Read { path: p.to_owned(), source });
    let cert_pem = read(cert)?;
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem).filter_map(Result::ok).collect();
    let first = chain.first().ok_or_else(|| CertError::NoCert(cert.to_owned()))?;
    let key_path = key.unwrap_or(cert);
    let key_pem = if key.is_some() { read(key_path)? } else { cert_pem.clone() };
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|_| CertError::NoKey(key_path.to_owned()))?;
    Ok(Cert { path: cert.to_owned(), fingerprint: fingerprint(first.as_ref()), chain, key })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_known_place_follows_xdg() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(dir_from(None, home.clone()), PathBuf::from("/home/u/.local/share/hirc/certs"));
        assert_eq!(dir_from(Some("/x".into()), home.clone()), PathBuf::from("/x/hirc/certs"));
        // A relative XDG_DATA_HOME is ignored.
        assert_eq!(dir_from(Some("rel".into()), home), PathBuf::from("/home/u/.local/share/hirc/certs"));
    }

    #[test]
    fn network_names_are_safe_file_names() {
        let d = Path::new("/c");
        assert_eq!(file_in(d, "libera"), PathBuf::from("/c/libera.pem"));
        assert_eq!(file_in(d, "my net/../x"), PathBuf::from("/c/my_net_.._x.pem"));
        assert_eq!(file_in(d, "s8i.io-2"), PathBuf::from("/c/s8i.io-2.pem"));
    }

    #[test]
    fn made_certs_load_back_private_to_the_user() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("certs").join("net.pem");
        let made = make(&path, "alice").unwrap();
        assert_eq!(made.fingerprint.len(), 64);
        assert!(made.fingerprint.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(made.fingerprint, fingerprint(made.chain[0].as_ref()));
        let loaded = load(&path).unwrap();
        assert_eq!((loaded.fingerprint.as_str(), loaded.chain.len()), (made.fingerprint.as_str(), 1));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // Making again replaces it.
        assert_ne!(make(&path, "alice").unwrap().fingerprint, made.fingerprint);
    }

    #[test]
    fn fingerprint_is_sha256_of_the_der() {
        assert_eq!(fingerprint(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn broken_files_are_errors_naming_them() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("net.pem");
        std::fs::write(&path, "not a pem").unwrap();
        let e = load(&path).unwrap_err();
        assert!(e.to_string().contains("net.pem"), "{e}");
        assert!(matches!(load(&tmp.path().join("none.pem")), Err(CertError::Read { .. })));
        // A cert without its key.
        let full = std::fs::read_to_string({
            make(&tmp.path().join("k.pem"), "a").unwrap();
            tmp.path().join("k.pem")
        })
        .unwrap();
        let cert_only: String = full.split_inclusive('\n').take_while(|l| !l.contains("PRIVATE KEY")).collect();
        std::fs::write(&path, cert_only).unwrap();
        assert!(matches!(load(&path), Err(CertError::NoKey(_))));
    }

    #[test]
    fn config_parts_load_with_the_key_apart_or_together() {
        let tmp = tempfile::tempdir().unwrap();
        let both = tmp.path().join("both.pem");
        let made = make(&both, "a").unwrap();
        assert_eq!(load_parts(&both, None).unwrap().fingerprint, made.fingerprint);
        let full = std::fs::read_to_string(&both).unwrap();
        let at = full.find("-----BEGIN PRIVATE KEY").unwrap();
        let (c, k) = (tmp.path().join("c.pem"), tmp.path().join("k.pem"));
        std::fs::write(&c, &full[..at]).unwrap();
        std::fs::write(&k, &full[at..]).unwrap();
        assert_eq!(load_parts(&c, Some(&k)).unwrap().fingerprint, made.fingerprint);
    }

    #[test]
    fn debug_never_shows_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("n.pem");
        let c = make(&path, "a").unwrap();
        let shown = format!("{c:?}");
        let pem = std::fs::read_to_string(&path).unwrap();
        let key_line = pem.lines().skip_while(|l| !l.contains("PRIVATE KEY")).nth(1).unwrap();
        assert!(!shown.contains(key_line) && shown.contains(&c.fingerprint), "{shown}");
    }
}
