//! OpenSSH-style SHA-256 fingerprints of public key lines.

use ring::digest;

/// Standard base64 alphabet, as used by OpenSSH key blobs and fingerprints.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Fingerprint of an OpenSSH public key line (`type base64 [comment]`), in the
/// `SHA256:<base64 without padding>` form printed by `ssh-keygen -lf`.
/// Returns `None` when the line has no decodable key field.
pub(crate) fn sha256(public_key_line: &str) -> Option<String> {
    let blob = decode(public_key_line.split_whitespace().nth(1)?)?;
    let hash = digest::digest(&digest::SHA256, &blob);
    Some(format!("SHA256:{}", encode(hash.as_ref())))
}

/// Decodes standard base64, tolerating missing or present `=` padding.
/// Returns `None` for an empty input, a stray character or an impossible length.
fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    if text.is_empty() || text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for byte in text.bytes() {
        let value = ALPHABET.iter().position(|&c| c == byte)?;
        acc = (acc << 6) | u32::try_from(value).ok()?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

/// Encodes standard base64 without `=` padding.
fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut group: u32 = 0;
        for (index, byte) in chunk.iter().enumerate() {
            group |= u32::from(*byte) << (16 - 8 * index);
        }
        // A chunk of n bytes yields n + 1 sextets.
        for index in 0..=chunk.len() {
            let sextet = usize::try_from((group >> (18 - 6 * index)) & 0x3f).unwrap_or(0);
            out.extend(ALPHABET.get(sextet).map(|&c| char::from(c)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sha256;
    use std::process::Command;

    /// Line whose blob is not a real key; hash computed with
    /// `base64 -d | openssl dgst -sha256 -binary | base64`.
    const FIXED_LINE: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeBlobForTestsOnly1234567 test@host";
    /// Expected fingerprint of [`FIXED_LINE`].
    const FIXED_FINGERPRINT: &str = "SHA256:mI952mft6caHLWe6V/XkEB6X6SsZiz2kycZR5KjlD2M";

    #[test]
    fn matches_ssh_keygen_for_generated_key() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let key = dir.path().join("k");
        let made = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", ""])
            .arg("-f")
            .arg(&key)
            .status()?;
        assert!(made.success());
        let pub_path = dir.path().join("k.pub");
        let listing = Command::new("ssh-keygen")
            .arg("-lf")
            .arg(&pub_path)
            .output()?;
        let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
        let expected = listing.split_whitespace().nth(1).unwrap_or_default();
        let line = std::fs::read_to_string(&pub_path)?;
        assert_eq!(sha256(&line).as_deref(), Some(expected));
        Ok(())
    }

    #[test]
    fn fixed_vector() {
        assert_eq!(sha256(FIXED_LINE).as_deref(), Some(FIXED_FINGERPRINT));
    }

    #[test]
    fn malformed_is_none() {
        assert_eq!(sha256(""), None);
        assert_eq!(sha256("ssh-ed25519"), None);
        assert_eq!(sha256("ssh-ed25519 !!!!"), None);
        assert_eq!(sha256("ssh-ed25519 A"), None);
    }
}
