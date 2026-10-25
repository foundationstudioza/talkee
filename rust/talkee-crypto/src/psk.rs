//! The pairing key: 32 bytes the host draws from the OS RNG and shows exactly once.
//!
//! # Why the key is generated rather than derived
//!
//! A key derived from anything a user can see (a device name, a PIN, a machine id)
//! is a key a stranger on the same network can guess or already know. The only
//! honest way to bind a phone to a desktop is to transfer 256 bits of entropy out
//! of band, which is what the deep link in [`crate::pair`] is for.
//!
//! # Why the type exists at all
//!
//! `Psk` is a newtype so that a key is never handled as a plain `Vec<u8>`: it
//! cannot be formatted into a log line by accident (see `Debug`/`Display` below),
//! it cannot be built from a slice without a length check, and it can only be
//! spelled one way. A bare `[u8; 32]` would have been terser, but then a stray
//! `format!("{:?}", key)` would have printed the secret.

use anyhow::{bail, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::fmt;

/// Length of a pairing key, in bytes.
///
/// 32 bytes is the AES-256 key size, and it is what makes publishing the link in a
/// QR code safe: the phone that scans it receives 256 bits of secret.
pub const PSK_LEN: usize = 32;

/// Bytes of the (truncated) SHA-256 digest published as the host fingerprint.
const FINGERPRINT_LEN: usize = 4;

/// A pre-shared pairing key.
///
/// Not `Copy`, and never printed: see the module docs for why.
#[derive(Clone, PartialEq, Eq)]
pub struct Psk([u8; PSK_LEN]);

impl Psk {
    /// Draws a fresh key from the OS random number generator.
    ///
    /// `getrandom` reads the OS CSPRNG (`RtlGenRandom` on Windows, `getrandom(2)`
    /// elsewhere) rather than a userspace PRNG seeded once per process: a key a
    /// second process could predict is not a key.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; PSK_LEN];
        getrandom::getrandom(&mut bytes)
            .map_err(|e| anyhow::anyhow!("reading 32 bytes from the OS random number generator: {e}"))?;
        Ok(Self(bytes))
    }

    /// Wraps an existing key, rejecting anything that is not exactly 32 bytes.
    ///
    /// A short key is the quiet failure mode worth guarding: AES accepts it, the
    /// frames still decrypt, and the entropy is simply gone.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != PSK_LEN {
            bail!(
                "a pairing key is exactly {PSK_LEN} bytes, this one is {}",
                bytes.len()
            );
        }
        let mut out = [0u8; PSK_LEN];
        out.copy_from_slice(bytes);
        Ok(Self(out))
    }

    /// The raw key bytes, for the KDFs in [`crate::proof`] and [`crate::session`].
    pub fn as_bytes(&self) -> &[u8; PSK_LEN] {
        &self.0
    }

    /// The key in the `k` parameter of the deep link: base64url, no padding.
    ///
    /// Base64url rather than standard base64 because the link gets pasted into a
    /// terminal, a chat message and a URL query string, and `+` and `/` survive
    /// none of those unescaped.
    pub fn to_base64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }    /// Decodes a key from base64url text.
    ///
    /// Padding is accepted because an encoder on another platform may add it; the
    /// length check in [`Psk::from_slice`] is what actually enforces the key size.
    pub fn from_base64url(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.len() > 128 {
            // Refuse before decoding: a hostile 100 MB "key" must not be decoded
            // into memory only to be rejected for length.
            bail!("a base64url pairing key cannot be {} characters long", text.len());
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(text.trim_end_matches('='))
            .map_err(|_| anyhow::anyhow!("the pairing key is not valid base64url"))?;
        Self::from_slice(&decoded)
    }

    /// Decodes a key from 64 hex characters.
    pub fn from_hex(text: &str) -> Result<Self> {
        let mut out = [0u8; PSK_LEN];
        hex::decode_to_slice(text.trim(), &mut out)
            .map_err(|_| anyhow::anyhow!("a hex pairing key is exactly 64 hex characters"))?;
        Ok(Self(out))
    }

    /// Decodes a key from either encoding, so a user can paste whatever they have.
    ///
    /// Hex is only attempted when the text is exactly 64 hex digits; anything else
    /// is treated as base64url. Guessing wrong cannot produce a *valid* key by
    /// accident, because every candidate must decode to exactly 32 bytes.
    pub fn from_text(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.len() == PSK_LEN * 2 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Self::from_hex(text);
        }
        Self::from_base64url(text)
    }

    /// A short, non-secret digest of this key, published as `fpid` in the link.
    ///
    /// It exists so a phone can recognise a host it has paired with before, and so
    /// a human can tell two hosts apart in a list. A truncated SHA-256 of a 256-bit
    /// secret is safe to publish: recovering the key from 4 digest bytes is a 2^32
    /// search per *candidate key*, and any candidate that matches is a key the
    /// attacker would have had to guess outright. Publishing the key itself, or a
    /// full digest, would instead hand an offline attacker something to grind on.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.0);
        hex::encode(&digest[..FINGERPRINT_LEN])
    }
}

impl fmt::Debug for Psk {
    /// Redacted on purpose: a key must never reach a log by way of a `{:?}`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Psk(<redacted, fingerprint {}>)", self.fingerprint())
    }
}

impl fmt::Display for Psk {
    /// Also redacted. `{}` is the more likely accidental leak of the two.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Psk(<redacted, fingerprint {}>)", self.fingerprint())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_32_bytes_and_never_repeat() {
        // Not a statistical test of the RNG: a smoke test that the same key is not
        // handed out twice, which is the failure that would actually matter.
        let a = Psk::generate().unwrap();
        let b = Psk::generate().unwrap();
        assert_eq!(a.as_bytes().len(), PSK_LEN);
        assert_ne!(a, b);
    }

    #[test]
    fn base64url_round_trips() {
        let psk = Psk::generate().unwrap();
        let text = psk.to_base64url();
        assert_eq!(text.len(), 43, "32 bytes is 43 unpadded base64url characters");
        assert_eq!(Psk::from_base64url(&text).unwrap(), psk);
        // Padded input from another encoder must parse to the same key.
        assert_eq!(Psk::from_base64url(&format!("{text}=")).unwrap(), psk);
    }

    #[test]
    fn hex_round_trips_and_shorter_hex_is_rejected() {
        let psk = Psk::from_slice(&[0xABu8; PSK_LEN]).unwrap();
        let text = "ab".repeat(PSK_LEN);
        assert_eq!(Psk::from_hex(&text).unwrap(), psk);
        assert!(Psk::from_hex(&"ab".repeat(PSK_LEN - 1)).is_err());
        assert!(Psk::from_hex("").is_err());
        assert_eq!(Psk::from_text(&text).unwrap(), psk);
    }

    #[test]
    fn wrong_length_keys_are_rejected() {
        assert!(Psk::from_slice(&[]).is_err());
        assert!(Psk::from_slice(&[0u8; PSK_LEN - 1]).is_err());
        assert!(Psk::from_slice(&[0u8; PSK_LEN + 1]).is_err());
        assert!(Psk::from_base64url("AAAA").is_err(), "4 bytes is not a key");
        assert!(Psk::from_text("not-a-key").is_err());
    }

    #[test]
    fn decoding_never_echoes_the_key() {
        // An error message that quotes the input would put the key into a log file,
        // a shell history and a bug report.
        let secret = Psk::from_slice(&[7u8; PSK_LEN]).unwrap();
        let err = Psk::from_base64url(&format!("{}AAAA", secret.to_base64url()))
            .unwrap_err()
            .to_string();
        assert!(!err.contains(&secret.to_base64url()), "the error echoed the key: {err}");
        assert!(!err.contains(&hex::encode(secret.as_bytes())), "the error echoed the key");
    }

    #[test]
    fn debug_and_display_do_not_leak_the_key() {
        let psk = Psk::from_slice(&[0x42u8; PSK_LEN]).unwrap();
        for text in [format!("{psk:?}"), format!("{psk}")] {
            assert!(text.contains("redacted"), "got {text}");
            assert!(!text.contains(&psk.to_base64url()), "leaked the key: {text}");
            assert!(!text.contains("QkJC"), "leaked base64 of the key: {text}");
        }
    }

    #[test]
    fn fingerprint_is_stable_and_host_specific() {
        // Known answer computed independently of this crate (SHA-256 of 32 zero
        // bytes, first 4 bytes): if this ever changes, every stored pairing
        // silently points at a different host.
        let zeros = Psk::from_slice(&[0u8; PSK_LEN]).unwrap();
        assert_eq!(zeros.fingerprint(), "66687aad");
        assert_ne!(Psk::generate().unwrap().fingerprint(), zeros.fingerprint());
        assert_eq!(zeros.fingerprint().len(), FINGERPRINT_LEN * 2);
    }
}