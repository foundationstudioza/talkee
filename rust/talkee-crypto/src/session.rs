//! AES-256-GCM payload sealing, and the replay rule that gives it meaning.
//!
//! # What the AEAD buys, and what it does not
//!
//! GCM authenticates: a frame whose ciphertext, tag, associated data or nonce was
//! altered does not open, and that is checked in constant time. What GCM does *not*
//! do is notice that a frame it already accepted has arrived a second time. A
//! replayed datagram is a byte-perfect, genuinely authentic datagram, so the only
//! defence is to remember which frames we have already accepted: hence the
//! high-water mark in [`Session::open`], which drops any frame whose `seq` is not
//! ahead of the highest one already authenticated.
//!
//! The order of the checks matters. Length, nonce shape and replay are settled
//! *before* the tag is checked, because they depend only on data the attacker
//! supplied; none of them touches the key, so distinguishing them leaks nothing
//! (a MAC failure, by contrast, is reported as one opaque error). The high-water
//! mark is advanced *only* after a successful tag check, so a forged frame cannot
//! push it forward and lock out the real stream that follows.
//!
//! # The nonce
//!
//! ```text
//! byte  0      direction: 0 = phone -> desktop, 1 = desktop -> phone
//! bytes 1..5   seq, little-endian u32
//! bytes 5..12  zero
//! ```
//!
//! 12 bytes is the size GCM is fastest and best analysed at (a 96-bit nonce needs
//! no hashing step to build the initial counter block). Uniqueness, not
//! unpredictability, is what a GCM nonce needs, and under one key the pair
//! (direction, seq) is unique by construction — because the key is derived per
//! session and `seq` is the sender"s monotonic counter. The direction byte keeps a
//! future downlink from colliding with the uplink it answers.

use crate::proof::{session_key, SessionId};
use crate::psk::Psk;
use aes_gcm::aead::consts::U12;
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{anyhow, Result};
use std::fmt;

/// Bytes of nonce prefixed to every sealed payload.
pub const NONCE_LEN: usize = 12;

/// Bytes of authentication tag appended to every sealed payload.
pub const TAG_LEN: usize = 16;
/// Direction of a frame, as the first nonce byte.
///
/// A single byte, and the whole reason it exists is that the two directions share a
/// session key: without it, the desktop"s own frame at `seq = 7` and the phone"s
/// frame at `seq = 7` would be sealed with the same key *and* the same nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    /// Phone to desktop: the audio direction, and the only one in use today.
    PhoneToDesktop = 0,
    /// Desktop to phone: reserved, so that the reserved direction cannot collide
    /// with the live one even though both use the same session key.
    DesktopToPhone = 1,
}

impl Direction {
    /// The value that goes into nonce byte 0.
    pub const fn bit(self) -> u8 {
        self as u8
    }
}

/// Why an encrypted payload was refused.
///
/// The three structural variants are decided from public bytes only (length, the
/// nonce we would have derived, the highest sequence number already accepted), so
/// naming them to the caller is safe and makes a failure diagnosable. [`Auth`]
/// covers every cryptographic check and deliberately says nothing more: GCM"s own
/// error is opaque, and an oracle that distinguished "tag wrong" from "plaintext
/// wrong" would be a forgery aid.
///
/// [`Auth`]: OpenError::Auth
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// Shorter than one nonce plus one tag, so it cannot be a sealed payload.
    Short {
        /// The length that arrived.
        len: usize,
    },
    /// The 12 bytes in front are not the nonce this session would have derived for
    /// that `seq` and direction.
    NonceMismatch,
    /// `seq` is not ahead of the highest already authenticated: a duplicate, a
    /// reordered frame, or a replayed datagram.
    Replay {
        /// The sequence number that arrived.
        seq: u32,
        /// The highest sequence number already authenticated in this session.
        highest: u32,
    },
    /// AES-256-GCM rejected the frame: wrong key, wrong session, tampered
    /// ciphertext, tampered tag, or tampered associated data.
    Auth,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Short { len } => write!(
                f,
                "encrypted payload of {len} bytes is shorter than the {} bytes of nonce and tag",
                NONCE_LEN + TAG_LEN
            ),
            OpenError::NonceMismatch => write!(f, "encrypted payload carries an unexpected nonce"),
            OpenError::Replay { seq, highest } => write!(
                f,
                "seq {seq} is not ahead of the highest authenticated seq {highest}: replayed or reordered"
            ),
            OpenError::Auth => write!(f, "AES-256-GCM authentication failed"),
        }
    }
}

impl std::error::Error for OpenError {}

/// The AES-GCM nonce type, spelled out so the size is visible at the call site.
type GcmNonce = Nonce<U12>;
/// One direction of one streaming session.
///
pub struct Session {
    id: SessionId,
    direction: Direction,
    cipher: Aes256Gcm,
    /// The highest sequence number this session has *used*: sealed, on the sending
    /// side, or authenticated, on the receiving side.
    ///
    /// One field for both roles because it is one rule — never touch the same
    /// sequence number twice under the same key — and the two halves of that rule
    /// (a sender that must not reuse a nonce, a receiver that must not accept a
    /// replay) are exactly where forgetting it does the damage.
    high_water: Option<u32>,
}

impl Session {
    /// Derives the session key from the pairing key and a fresh session id.
    pub fn new(psk: &Psk, id: SessionId, direction: Direction) -> Self {
        let key = session_key(psk, &id);
        let cipher = Aes256Gcm::new_from_slice(&key)
            .expect("the KDF always produces exactly 32 bytes, which is the AES-256 key size");
        Self {
            id,
            direction,
            cipher,
            high_water: None,
        }
    }

    /// The session id, which is the `PAIR.session` this session was built from.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The direction this session seals and opens.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The highest sequence number sealed or accepted so far, if any.
    ///
    /// Exposed for the receiver"s health line and for tests: a session whose
    /// high-water mark is stuck is a session that is dropping frames as replays.
    pub fn high_water(&self) -> Option<u32> {
        self.high_water
    }

    /// The 12-byte nonce for `seq`, per the layout in the module docs.
    ///
    /// Public and pure so the derivation can be asserted directly, rather than
    /// only through a round trip that would pass for any self-consistent scheme.
    pub fn nonce(&self, seq: u32) -> [u8; NONCE_LEN] {
        let mut nonce = [0u8; NONCE_LEN];
        nonce[0] = self.direction.bit();
        nonce[1..5].copy_from_slice(&seq.to_le_bytes());
        nonce
    }

    /// Seals `plaintext` into `out` as `[nonce][ciphertext][tag]`, replacing any
    /// previous contents.
    ///
    /// `aad` is authenticated but not encrypted; the wire format passes the 24-byte
    /// audio header, so a frame"s sample rate, channel count, format and flags are
    /// bound to the ciphertext and cannot be edited in flight.
    ///
    /// Refuses to seal a `seq` this session has already sealed. A sender bug that
    /// reset the counter would otherwise reuse a nonce under one key, which is the
    /// one mistake AES-GCM cannot survive, and it fails *loudly* here instead of
    /// quietly on the wire.
    pub fn seal(
        &mut self,
        seq: u32,
        aad: &[u8],
        plaintext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<()> {
        if let Some(last) = self.high_water {
            if !is_newer(seq, last) {
                return Err(anyhow!(
                    "refusing to seal seq {seq} after seq {last}: reusing a nonce under one key \
                     breaks AES-GCM, so this needs a new session id rather than a retry"
                ));
            }
        }
        let nonce = self.nonce(seq);
        let sealed = self
            .cipher
            .encrypt(GcmNonce::from_slice(&nonce), Payload { msg: plaintext, aad })
            .map_err(|_| anyhow!("AES-256-GCM refused to seal a {}-byte frame", plaintext.len()))?;
        out.clear();
        out.reserve(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        self.high_water = Some(seq);
        Ok(())
    }
    /// Opens a sealed payload, returning the plaintext only if it is authentic *and*
    /// is not something we have already seen.
    ///
    /// The checks are ordered cheapest-and-safest first; see the module docs for why
    /// the failing tag check must not be able to advance the high-water mark.
    pub fn open(&mut self, seq: u32, aad: &[u8], payload: &[u8]) -> Result<Vec<u8>, OpenError> {
        if payload.len() < NONCE_LEN + TAG_LEN {
            return Err(OpenError::Short { len: payload.len() });
        }
        let (nonce, sealed) = payload.split_at(NONCE_LEN);
        let expected = self.nonce(seq);
        if nonce != expected.as_slice() {
            return Err(OpenError::NonceMismatch);
        }
        if let Some(highest) = self.high_water {
            if !is_newer(seq, highest) {
                return Err(OpenError::Replay { seq, highest });
            }
        }
        let plaintext = self
            .cipher
            .decrypt(GcmNonce::from_slice(&expected), Payload { msg: sealed, aad })
            .map_err(|_| OpenError::Auth)?;
        self.high_water = Some(seq);
        Ok(plaintext)
    }
}

impl fmt::Debug for Session {
    /// Prints the session id and the replay state, never the key or the cipher.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("session_id", &self.id.to_hex())
            .field("direction", &self.direction)
            .field("high_water", &self.high_water)
            .finish_non_exhaustive()
    }
}

/// True when `a` is ahead of `b` in the wrapping sense used across this codebase
/// (`talkee_core::stream::classify_seq` does the same arithmetic).
///
/// Wrapping because a sender that runs long enough for `u32` to wrap — 497 days at
/// 10 ms frames — must not lock its own receiver out at the wrap; signed because
/// the comparison is "within half the sequence space", so an old frame reads as
/// late rather than as four billion frames early.
fn is_newer(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) as i32 > 0
}
#[cfg(test)]
mod tests {
    use super::*;

    use aes_gcm::aead::generic_array::typenum::Unsigned;
    use aes_gcm::aead::AeadCore;
    use aes_gcm::KeySizeUser;

    /// The authenticated associated data a frame carries on the wire in practice:
    /// the 24-byte audio header. Any other constant would test the same code path.
    const AAD: &[u8] = b"24 bytes of audio header, authenticated not encrypted";

    /// A frame"s worth of audio at 48 kHz mono f32, 480 samples.
    const AUDIO: &[u8] = &[0x5Au8; 1920];

    fn psk() -> Psk {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        Psk::from_slice(&bytes).unwrap()
    }

    fn other_psk() -> Psk {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(1);
        }
        Psk::from_slice(&bytes).unwrap()
    }

    fn session(psk: &Psk, id: [u8; 4], direction: Direction) -> Session {
        Session::new(psk, SessionId::from_bytes(id), direction)
    }

    fn sender() -> Session {
        session(&psk(), [1, 2, 3, 4], Direction::PhoneToDesktop)
    }

    fn seal(s: &mut Session, seq: u32, plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        s.seal(seq, AAD, plaintext, &mut out).expect("sealing must succeed");
        out
    }

    #[test]
    fn constants_match_the_primitive_they_describe() {
        // The wire format reasons about these numbers (12 and 16 appear in the
        // payload length rule), so they must be the ones AES-GCM actually uses.
        assert_eq!(NONCE_LEN, <Aes256Gcm as AeadCore>::NonceSize::USIZE);
        assert_eq!(TAG_LEN, <Aes256Gcm as AeadCore>::TagSize::USIZE);
        assert_eq!(crate::PSK_LEN, Aes256Gcm::key_size());
    }

    #[test]
    fn a_sealed_frame_opens_on_the_other_side() {
        let mut tx = sender();
        let mut rx = sender();
        let frame = seal(&mut tx, 7, AUDIO);
        assert_eq!(frame.len(), NONCE_LEN + AUDIO.len() + TAG_LEN);
        assert_ne!(&frame[NONCE_LEN..], AUDIO, "the audio must not be in the clear");
        assert_eq!(rx.open(7, AAD, &frame).unwrap(), AUDIO);
        assert_eq!(rx.high_water(), Some(7));
    }

    #[test]
    fn the_nonce_is_the_documented_layout() {
        let tx = sender();
        let nonce = tx.nonce(0x0403_0201);
        assert_eq!(nonce[0], Direction::PhoneToDesktop.bit());
        assert_eq!(&nonce[1..5], &[0x01, 0x02, 0x03, 0x04], "seq is little-endian");
        assert_eq!(&nonce[5..], &[0u8; 7], "the tail is zero, not random");
        // The two directions must never produce the same nonce for one seq.
        let other = session(&psk(), [1, 2, 3, 4], Direction::DesktopToPhone);
        assert_ne!(other.nonce(0x0403_0201), nonce);
        assert_eq!(other.nonce(0x0403_0201)[0], 1);
    }

    #[test]
    fn the_same_frame_sealed_twice_is_identical_which_is_why_seq_must_be_unique() {
        // Not a property to be proud of: it is the demonstration that a repeated
        // (key, nonce) pair leaks the XOR of the two plaintexts. The guard in
        // `seal` is what stops it from happening by accident.
        let mut first = sender();
        let mut second = sender();
        let a = seal(&mut first, 3, AUDIO);
        let b = seal(&mut second, 3, AUDIO);
        assert_eq!(a, b, "the same key and nonce must produce the same bytes");
        // A different sequence number is a different nonce, and therefore (for the
        // same plaintext) different ciphertext. Compared through the nonces so a
        // failure prints 12 bytes instead of 2 KiB.
        let mut third = sender();
        let nonce_three = third.nonce(3);
        let nonce_four = third.nonce(4);
        assert_ne!(nonce_three, nonce_four);
        let at_three = seal(&mut third, 3, AUDIO);
        let at_four = seal(&mut third, 4, AUDIO);
        assert_ne!(
            &at_three[..NONCE_LEN + 32],
            &at_four[..NONCE_LEN + 32],
            "a different seq must change the ciphertext"
        );
    }
    #[test]
    fn a_frame_will_not_open_under_the_wrong_key() {
        let mut tx = sender();
        let frame = seal(&mut tx, 1, AUDIO);
        let mut wrong_key = session(&other_psk(), [1, 2, 3, 4], Direction::PhoneToDesktop);
        assert_eq!(wrong_key.open(1, AAD, &frame), Err(OpenError::Auth));
        // A different session id is a different key, from the same pairing key.
        let mut wrong_session = session(&psk(), [1, 2, 3, 5], Direction::PhoneToDesktop);
        assert_eq!(wrong_session.open(1, AAD, &frame), Err(OpenError::Auth));
        // And the other direction is a different nonce, not a different key.
        let mut wrong_direction = session(&psk(), [1, 2, 3, 4], Direction::DesktopToPhone);
        assert_eq!(wrong_direction.open(1, AAD, &frame), Err(OpenError::NonceMismatch));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let mut tx = sender();
        let frame = seal(&mut tx, 4, AUDIO);
        for bit in [0x01u8, 0x80] {
            let mut damaged = frame.clone();
            damaged[NONCE_LEN] ^= bit; // first ciphertext byte
            damaged[NONCE_LEN + AUDIO.len() - 1] ^= bit; // last ciphertext byte
            let mut rx = sender();
            assert_eq!(rx.open(4, AAD, &damaged), Err(OpenError::Auth));
        }
    }

    #[test]
    fn tampered_tag_is_rejected() {
        let mut tx = sender();
        let frame = seal(&mut tx, 5, AUDIO);
        let mut damaged = frame.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xFF;
        let mut rx = sender();
        assert_eq!(rx.open(5, AAD, &damaged), Err(OpenError::Auth));
    }

    #[test]
    fn tampered_nonce_is_rejected() {
        // The nonce is not encrypted, but it is not free either: any change to it
        // changes the counter block, so the tag cannot verify. We reject it before
        // the tag check because we can, from the seq we were given.
        let mut tx = sender();
        let frame = seal(&mut tx, 6, AUDIO);
        let mut damaged = frame.clone();
        damaged[0] ^= 0x01;
        let mut rx = sender();
        assert_eq!(rx.open(6, AAD, &damaged), Err(OpenError::NonceMismatch));
    }

    #[test]
    fn tampered_associated_data_is_rejected() {
        // This is what stops an attacker relabelling a frame: changing the sample
        // rate, channel count or format in the header without breaking the tag.
        let mut tx = sender();
        let frame = seal(&mut tx, 8, AUDIO);
        let mut edited_aad = AAD.to_vec();
        edited_aad[3] ^= 0x01;
        let mut rx = sender();
        assert_eq!(rx.open(8, &edited_aad, &frame), Err(OpenError::Auth));
    }

    #[test]
    fn short_payloads_are_rejected_without_panicking() {
        // Every length from empty to one byte short of a sealed frame, including
        // the lengths that would make an unchecked split panic.
        for len in 0..(NONCE_LEN + TAG_LEN) {
            let mut rx = sender();
            let payload = vec![0u8; len];
            assert_eq!(rx.open(1, AAD, &payload), Err(OpenError::Short { len }));
        }
        // An empty plaintext is still a valid frame: one nonce and one tag.
        let mut tx = sender();
        let frame = seal(&mut tx, 1, &[]);
        assert_eq!(frame.len(), NONCE_LEN + TAG_LEN);
        let mut rx = sender();
        assert_eq!(rx.open(1, AAD, &frame).unwrap(), Vec::<u8>::new());
    }
    #[test]
    fn a_replayed_frame_is_dropped_the_second_time() {
        let mut tx = sender();
        let frame = seal(&mut tx, 12, AUDIO);
        let mut rx = sender();
        assert_eq!(rx.open(12, AAD, &frame).unwrap(), AUDIO, "the live frame arrives");
        assert_eq!(
            rx.open(12, AAD, &frame),
            Err(OpenError::Replay { seq: 12, highest: 12 }),
            "the same datagram replayed must be refused"
        );
    }

    #[test]
    fn out_of_order_frames_are_dropped_not_queued() {
        // Audio has a deadline, so a frame that arrives after a newer one is worth
        // less than nothing: playing it reorders the microphone.
        let mut tx = sender();
        let older = seal(&mut tx, 20, AUDIO);
        let newer = seal(&mut tx, 21, AUDIO);
        let mut rx = sender();
        assert!(rx.open(21, AAD, &newer).is_ok());
        assert_eq!(rx.open(20, AAD, &older), Err(OpenError::Replay { seq: 20, highest: 21 }));
        assert_eq!(rx.high_water(), Some(21), "a refused frame must not move the mark");
    }

    #[test]
    fn a_gap_is_not_a_replay() {
        // Losing datagrams is normal on UDP. The replay rule is "ahead of the
        // highest seen", not "exactly the next one", or every loss would end the
        // stream until the sender restarted.
        let mut tx = sender();
        for seq in [1u32, 9, 10_000] {
            let frame = seal(&mut tx, seq, AUDIO);
            let mut rx = sender();
            rx.high_water = Some(seq - 1);
            assert!(rx.open(seq, AAD, &frame).is_ok(), "seq {seq} after a gap must open");
        }
    }

    #[test]
    fn a_forged_frame_cannot_advance_the_high_water_mark() {
        // If a failed authentication moved the mark, anyone on the LAN could send
        // one bogus frame with seq = u32::MAX and mute the real microphone for good.
        let mut tx = sender();
        let real = seal(&mut tx, 2, AUDIO);
        let mut forged = real.clone();
        forged[NONCE_LEN + 1] ^= 0x40;
        let mut rx = sender();
        assert_eq!(rx.open(2, AAD, &forged), Err(OpenError::Auth));
        assert_eq!(rx.high_water(), None, "a forgery must not move the mark");
        assert_eq!(rx.open(2, AAD, &real).unwrap(), AUDIO, "the real frame still opens");
    }

    #[test]
    fn the_sequence_number_may_wrap_without_locking_the_receiver_out() {
        let mut rx = sender();
        rx.high_water = Some(u32::MAX);
        let mut tx = sender();
        tx.high_water = Some(u32::MAX);
        let frame = seal(&mut tx, 0, AUDIO);
        assert!(rx.open(0, AAD, &frame).is_ok(), "u32::MAX then 0 is forward, not back");
        // ...and a genuinely old frame after the wrap is still refused.
        rx.high_water = Some(0);
        let mut tx = sender();
        tx.high_water = Some(u32::MAX - 1);
        let stale = seal(&mut tx, u32::MAX, AUDIO);
        assert!(matches!(rx.open(u32::MAX, AAD, &stale), Err(OpenError::Replay { .. })));
    }

    #[test]
    fn sealing_the_same_sequence_number_twice_is_refused() {
        // The one mistake AES-GCM cannot survive, caught before it reaches the wire.
        let mut tx = sender();
        let _ = seal(&mut tx, 4, AUDIO);
        let mut out = Vec::new();
        assert!(tx.seal(4, AAD, AUDIO, &mut out).is_err());
        assert!(tx.seal(3, AAD, AUDIO, &mut out).is_err(), "nor an older one");
        assert!(tx.seal(5, AAD, AUDIO, &mut out).is_ok());
    }

    #[test]
    fn debug_output_does_not_leak_key_material() {
        let text = format!("{:?}", sender());
        assert!(text.contains("session_id"), "got {text}");
        assert!(text.contains(".."), "the cipher field must be elided: {text}");
    }
}