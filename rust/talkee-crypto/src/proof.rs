//! Proof that the far end holds the pairing key, and derivation of the session key.
//!
//! # The attacks this is built against
//!
//! The network is treated as hostile: a cafe access point, a dorm switch, an
//! office VLAN. Two attacks matter here.
//!
//! * **Passive.** An eavesdropper records the handshake and the audio. It must
//!   learn nothing reusable. Because the key is never transmitted and the proof is
//!   a one-way function of it, the recording is worth nothing.
//! * **Active replay.** An attacker cannot answer a challenge it did not see, so
//!   the receiver draws a fresh 32-byte nonce for *every* control connection. A
//!   recorded `PAIR` is worthless against the next connection, which is why the
//!   nonce is not a constant.
//!
//! # Why the labels are in the MAC inputs
//!
//! `HMAC-SHA256(psk, "talkee-pair-v1" || ...)` and
//! `HMAC-SHA256(psk, "talkee-session-v1" || ...)` are the same primitive over
//! different inputs, and the labels are what keeps them apart. Without them, a
//! value computed for one purpose could be replayed as the other — the classic
//! mistake that turns two individually-fine uses of a key into one broken one.
//!
//! The MAC is computed over the raw 32 challenge **bytes**, never over their hex
//! text, because two implementations disagreeing about that is exactly how a
//! phone and a desktop end up unable to pair for no visible reason. The
//! known-answer test at the bottom of this file exists to pin that down.

use crate::psk::Psk;
use anyhow::Result;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fmt;

/// Bytes in a `CHALLENGE` nonce.
///
pub const CHALLENGE_LEN: usize = 32;

/// Bytes of HMAC-SHA256 output, and therefore of a decoded `PAIR.proof`.
pub const PROOF_LEN: usize = 32;

/// Bytes in a session id. Four bytes because it only has to be unique between
/// restarts of one sender (see the crate docs on nonce reuse), not unguessable:
/// guessing a session id buys an attacker nothing, since the session key is
/// derived from the pairing key it does not know.
pub const SESSION_ID_LEN: usize = 4;

/// Domain separation for the pairing proof.
const PAIR_LABEL: &[u8] = b"talkee-pair-v1";

/// Domain separation for the per-session key.
const SESSION_LABEL: &[u8] = b"talkee-session-v1";

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 over the concatenation of `parts`.
///
/// One helper rather than three call sites: the labels and the concatenation
/// order *are* the protocol, and they should be readable in one place.
fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; PROOF_LEN] {
    let mut mac = HmacSha256::new_from_slice(key)
        .expect("HMAC-SHA256 accepts a key of any length, so this cannot fail");
    for part in parts {
        mac.update(part);
    }
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; PROOF_LEN];
    out.copy_from_slice(&tag);
    out
}/// A single-use challenge sent by the receiver in `CHALLENGE`.
///
/// Not secret — it travels in the clear by design — so `Debug` prints it in full.
/// Anything that makes a failed pairing diagnosable without echoing key material is
/// worth having.
#[derive(Clone, PartialEq, Eq)]
pub struct Challenge([u8; CHALLENGE_LEN]);

impl Challenge {
    /// Draws a fresh nonce. One per control connection, from the OS RNG.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; CHALLENGE_LEN];
        getrandom::getrandom(&mut bytes)
            .map_err(|e| anyhow::anyhow!("reading 32 bytes from the OS random number generator: {e}"))?;
        Ok(Self(bytes))
    }

    /// Parses the hex form that `CHALLENGE.nonce` carries on the wire.
    ///
    /// Strict about length: a short nonce is a peer that disagrees about the
    /// protocol, and the failure it would otherwise cause is a proof that never
    /// verifies, which reads like a wrong key.
    pub fn parse(hex_text: &str) -> Result<Self> {
        let mut bytes = [0u8; CHALLENGE_LEN];
        hex::decode_to_slice(hex_text.trim(), &mut bytes).map_err(|_| {
            anyhow::anyhow!("a challenge nonce is exactly {CHALLENGE_LEN} bytes in hex")
        })?;
        Ok(Self(bytes))
    }

    /// The bytes the proof is computed over.
    pub fn as_bytes(&self) -> &[u8; CHALLENGE_LEN] {
        &self.0
    }

    /// The hex form for `CHALLENGE.nonce`.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for Challenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Challenge({})", self.to_hex())
    }
}

/// Identifies one streaming session, and so selects the per-session key.
///
/// Drawn by the *sender* at the start of every stream and returned in `PAIR`, where
/// it is covered by the proof. The receiver never chooses it, because the sender is
/// the one that has to promise not to reuse a nonce, and it can only promise that
/// about a value it owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId([u8; SESSION_ID_LEN]);

impl SessionId {
    /// Draws a fresh session id from the OS RNG.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; SESSION_ID_LEN];
        getrandom::getrandom(&mut bytes)
            .map_err(|e| anyhow::anyhow!("reading 4 bytes from the OS random number generator: {e}"))?;
        Ok(Self(bytes))
    }

    /// Wraps an explicit id, for tests and for a caller that has its own entropy.
    pub fn from_bytes(bytes: [u8; SESSION_ID_LEN]) -> Self {
        Self(bytes)
    }

    /// Parses the 8 hex characters carried in `PAIR.session`.
    pub fn from_hex(text: &str) -> Result<Self> {
        let mut bytes = [0u8; SESSION_ID_LEN];
        hex::decode_to_slice(text.trim(), &mut bytes)
            .map_err(|_| anyhow::anyhow!("a session id is exactly 8 hex characters"))?;
        Ok(Self(bytes))
    }

    /// The hex form for `PAIR.session`.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The bytes mixed into the proof and the session KDF.
    pub fn as_bytes(&self) -> &[u8; SESSION_ID_LEN] {
        &self.0
    }
}/// The `PAIR.proof` value: `HMAC-SHA256(psk, "talkee-pair-v1" || nonce || session_id)`.
///
/// `nonce` is the raw 32 bytes of the challenge, not its hex text.
pub fn proof(psk: &Psk, nonce: &[u8], session: &SessionId) -> [u8; PROOF_LEN] {
    hmac(psk.as_bytes(), &[PAIR_LABEL, nonce, session.as_bytes()])
}

/// The `PAIR.proof` value in the hex form the wire carries.
///
/// Provided so that a caller can produce the wire value without reaching for a hex
/// encoder of its own: two encoders is two chances to disagree about case or
/// padding, and this one is tested against the same known-answer vector as the bytes.
pub fn proof_hex(psk: &Psk, nonce: &[u8], session: &SessionId) -> String {
    hex::encode(proof(psk, nonce, session))
}

/// Checks a hex-encoded `PAIR.proof` against the challenge we actually sent.
///
/// Returns `false` rather than an error for every kind of bad input: a malformed
/// proof and a wrong proof are the same event as far as the receiver is concerned,
/// and distinguishing them on the wire would only help an attacker probing for
/// which part it got right.
///
/// The comparison is `Mac::verify_slice`, which is constant time, so a response
/// cannot be found byte by byte with a stopwatch.
pub fn verify_proof(psk: &Psk, nonce: &[u8], session: &SessionId, claimed_hex: &str) -> bool {
    let mut claimed = [0u8; PROOF_LEN];
    if hex::decode_to_slice(claimed_hex.trim(), &mut claimed).is_err() {
        return false;
    }
    let mut mac = HmacSha256::new_from_slice(psk.as_bytes())
        .expect("HMAC-SHA256 accepts a key of any length, so this cannot fail");
    mac.update(PAIR_LABEL);
    mac.update(nonce);
    mac.update(session.as_bytes());
    mac.verify_slice(&claimed).is_ok()
}

/// The key every frame in one session is sealed with.
///
/// `HMAC-SHA256(psk, "talkee-session-v1" || session_id)` is HKDF-Extract with the
/// pairing key as the salt. There is no Expand step because a single 32-byte
/// output is all AES-256 needs, and HKDF-Expand earns its keep only when the
/// output must be longer than one hash or split into several keys.
pub(crate) fn session_key(psk: &Psk, session: &SessionId) -> [u8; 32] {
    hmac(psk.as_bytes(), &[SESSION_LABEL, session.as_bytes()])
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A key equal to the bytes 0x00..0x1F, so the test inputs are visible in the
    /// assertion rather than hidden behind a random draw.
    fn psk() -> Psk {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        Psk::from_slice(&bytes).unwrap()
    }

    fn nonce() -> [u8; CHALLENGE_LEN] {
        let mut bytes = [0u8; CHALLENGE_LEN];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        bytes
    }

    #[test]
    fn proof_matches_an_independently_computed_vector() {
        // Values produced by .NET's HMACSHA256 over the same inputs, i.e. by an
        // implementation that shares no code with this one. If a change to the
        // labels, the concatenation order or the hex/bytes choice breaks the
        // protocol, this test says so before a phone in another language does.
        //
        //   psk       = 00 01 .. 1f
        //   nonce     = 00 01 .. 1f
        //   session   = 01 02 03 04
        let session = SessionId::from_hex("01020304").unwrap();
        let computed = proof(&psk(), &nonce(), &session);
        assert_eq!(
            hex::encode(computed),
            "8cd6e1b4ba154c4521f0de5d302208a1f10478bf708bcae98038aeb5b1c60593"
        );
        assert_eq!(hex::encode(session_key(&psk(), &session)),
            "acbcb7a70f5e3ba04af029614c9d5959ab97d297fc807264dc30e6f4d3b9af49");
    }

    #[test]
    fn a_proof_verifies_only_against_the_challenge_it_answers() {
        let session = SessionId::from_bytes([1, 2, 3, 4]);
        let good = hex::encode(proof(&psk(), &nonce(), &session));
        assert!(verify_proof(&psk(), &nonce(), &session, &good));

        let mut other_nonce = nonce();
        other_nonce[0] ^= 0x01;
        assert!(!verify_proof(&psk(), &other_nonce, &session, &good),
            "a recorded proof must not answer a different challenge");

        let other_session = SessionId::from_bytes([1, 2, 3, 5]);
        assert!(!verify_proof(&psk(), &nonce(), &other_session, &good),
            "the session id is covered by the proof, or a replay could pick its own");
    }

    #[test]
    fn a_proof_from_the_wrong_key_does_not_verify() {
        let session = SessionId::from_bytes([9, 9, 9, 9]);
        let mut wrong = [0u8; 32];
        for (i, b) in wrong.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(1);
        }
        let claimed = hex::encode(proof(&Psk::from_slice(&wrong).unwrap(), &nonce(), &session));
        assert!(!verify_proof(&psk(), &nonce(), &session, &claimed));
    }

    #[test]
    fn malformed_proofs_are_rejected_without_panicking() {
        let session = SessionId::from_bytes([0, 0, 0, 1]);
        for claimed in ["", "00", "zz", "0", "8cd6", &"ab".repeat(31), &"ab".repeat(33)] {
            assert!(!verify_proof(&psk(), &nonce(), &session, claimed), "accepted {claimed:?}");
        }
        // Upper case hex decodes, but the value still has to be right.
        let good = hex::encode(proof(&psk(), &nonce(), &session)).to_uppercase();
        assert!(verify_proof(&psk(), &nonce(), &session, &good));
    }

    #[test]
    fn challenge_and_session_id_round_trip_their_hex_form() {
        let c = Challenge::generate().unwrap();
        assert_eq!(Challenge::parse(&c.to_hex()).unwrap(), c);
        assert_eq!(c.to_hex().len(), CHALLENGE_LEN * 2);
        assert!(Challenge::parse(&"00".repeat(CHALLENGE_LEN - 1)).is_err());
        assert!(Challenge::parse("not hex").is_err());
        assert!(Challenge::parse("").is_err());

        let s = SessionId::generate().unwrap();
        assert_eq!(SessionId::from_hex(&s.to_hex()).unwrap(), s);
        assert_eq!(s.to_hex().len(), SESSION_ID_LEN * 2);
        assert!(SessionId::from_hex("0102").is_err());

        assert_ne!(Challenge::generate().unwrap(), Challenge::generate().unwrap());
        assert_ne!(SessionId::generate().unwrap(), SessionId::generate().unwrap());
    }
}