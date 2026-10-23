//! SASL helpers. PLAIN for now; EXTERNAL and SCRAM-SHA-256 come later.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// The base64 PLAIN payload: `authzid NUL authcid NUL password`.
pub fn plain(authzid: &str, authcid: &str, password: &str) -> String {
    STANDARD.encode(format!("{authzid}\0{authcid}\0{password}"))
}

/// AUTHENTICATE takes at most 400 bytes per line. A payload that is empty
/// or an exact multiple of 400 ends with a lone "+".
pub fn chunks(payload: &str) -> Vec<String> {
    let mut out: Vec<String> = payload.as_bytes().chunks(400).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    if payload.len().is_multiple_of(400) {
        out.push("+".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_encodes_nul_separated_fields() {
        assert_eq!(plain("", "alice", "hunter22"), "AGFsaWNlAGh1bnRlcjIy");
    }

    #[test]
    fn chunks_split_at_400_and_end_with_plus_on_exact_multiple() {
        assert_eq!(chunks("abc"), ["abc"]);
        assert_eq!(chunks(""), ["+"]);
        let exact = "a".repeat(400);
        assert_eq!(chunks(&exact), [exact.clone(), "+".into()]);
        let long = "a".repeat(401);
        assert_eq!(chunks(&long), [exact, "a".into()]);
    }
}
