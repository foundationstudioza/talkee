//! Pairing and payload encryption.
//!
//! # The threat this closes
//!
//! A microphone is a sensitive device. Before this crate existed the host bound
//! `0.0.0.0:47801`, advertised itself over mDNS, and accepted audio from anyone
//! who could reach the port — so on an office, dorm, conference or cafe network a
//! stranger could put audio into the virtual microphone that Zoom and Discord
//! read. Advertising over mDNS made that *findable* rather than merely possible.
//!
//! Three things close it, and each one is load-bearing:
//!
//! * **A key that never crosses the wire.** The host draws 32 bytes from the OS
//!   RNG and shows them once as a `talkee://pair?...` link (see `pair`). The link
//!   is the only copy outside the platform keystore.
//! * **Proof of possession, not transmission.** The sender answers a 32-byte
//!   challenge with `HMAC-SHA256(psk, ...)` (see `proof`). An eavesdropper cannot
//!   answer a *different* challenge, and learns no key material at all.
//! * **AEAD payloads with a strictly increasing sequence number.** Every audio
//!   datagram is AES-256-GCM, and a frame whose `seq` is not ahead of the highest
//!   already authenticated is dropped before its tag is even checked (see
//!   `session`). A replayed datagram is worthless; that is the whole point of
//!   tracking the high-water mark.
//!
//! # The construction, exactly
//!
//! | Purpose | Construction |
//! |---|---|
//! | pairing key | 32 bytes from the OS RNG |
//! | session id | 4 bytes from the OS RNG, fresh for every stream |
//! | pairing proof | `HMAC-SHA256(psk, "talkee-pair-v1" || nonce || session_id)` |
//! | session key | `HMAC-SHA256(psk, "talkee-session-v1" || session_id)` |
//! | payload | AES-256-GCM, 12-byte nonce, 16-byte tag |
//! | nonce | `[direction bit][seq, little-endian u32][7 zero bytes]` |
//! | replay rule | accept only if `seq` is ahead of the highest authenticated |
//!
//! # Why there is a per-session key at all
//!
//! "The nonce is `seq` plus a direction bit" is only safe if `seq` never repeats
//! under one key. It does repeat: a phone that restarts begins again at zero,
//! while a persisted pairing key stays the same. Reusing a GCM nonce under a fixed
//! key is a catastrophic failure, not a small one — it leaks the XOR of two
//! plaintexts *and* it lets an attacker forge tags. Deriving a fresh session key
//! per stream, from a session id the sender chooses at random, is what makes the
//! inexpensive nonce legitimate.
//!
//! This crate knows nothing about the wire format. It seals and opens byte slices,
//! so there is exactly one implementation of the HMAC, one of the GCM seal, and no
//! way for the framing and the crypto to disagree about where a nonce ends or a tag
//! begins.

#![forbid(unsafe_code)]

pub mod pair;
pub mod proof;
pub mod psk;
pub mod session;

pub use pair::PairLink;
pub use proof::{proof, proof_hex, verify_proof, Challenge, SessionId};
pub use psk::{Psk, PSK_LEN};
pub use session::{Direction, OpenError, Session, NONCE_LEN, TAG_LEN};

/// Bytes an AEAD payload carries on top of its plaintext: the nonce and the tag.
///
/// The wire format needs this number to check a declared payload length before it
/// allocates anything, which is why it lives here rather than as a local `12 + 16`
/// in each crate that has to reason about the size of a decrypted frame.
pub const AEAD_OVERHEAD: usize = NONCE_LEN + TAG_LEN;