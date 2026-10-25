//! Talkee wire format.
//!
//! This crate is the single written-down contract between the phone and the
//! desktop host. It contains **no** platform code and **no** DSP, so it compiles
//! for every one of our targets (and its tests run on any of them).
//!
//! # Scope
//!
//! M0 shipped **raw PCM**: no codec, no encryption, no discovery. M1 adds Opus
//! (`format = 2`) and M2 adds pairing and AEAD payloads. Neither changed the
//! header shape, which is the entire reason the header carries an explicit format
//! tag and payload length instead of inferring either one.
//!
//! Three parts of the contract live here and are not allowed to drift apart:
//!
//! * the 24-byte audio header and its validation rules ([`AudioHeader`]);
//! * the control-plane messages ([`ControlMsg`]) and their JSON framing;
//! * the crypto those rules are stated in terms of, re-exported as [`crypto`].
//!   `talkee-core`'s receive path and both CLI commands need it, and their
//!   manifests are not ours to extend, so the dependency runs one way —
//!   `talkee-proto` knows about `talkee-crypto`, and `talkee-crypto` knows nothing
//!   about the wire.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Bumped whenever the audio header layout or control messages change in a way
/// that is not backwards compatible. Peers refuse to talk across versions.
pub const PROTOCOL_VERSION: u8 = 1;

/// On the wire, little-endian `0x4B54` renders as the ASCII bytes `TK`.
pub const MAGIC: u16 = 0x4B54;

/// Control plane (newline-delimited JSON over TCP). Carries the pairing
/// handshake, which is what decides whether audio is accepted at all.
pub const DEFAULT_CONTROL_PORT: u16 = 47800;

/// Audio plane (one UDP datagram per frame).
pub const DEFAULT_AUDIO_PORT: u16 = 47801;

/// mDNS service type we will advertise/browse from M2.
pub const MDNS_SERVICE: &str = "_talkee._tcp.local.";

/// Deep-link scheme for QR pairing. Format is specified in `docs/PROTOCOL.md`.
pub const QR_SCHEME: &str = "talkee";

/// Fixed size of every audio datagram header, in bytes.
pub const HEADER_LEN: usize = 24;

/// Maximum payload we are willing to allocate for a single datagram.
///
/// 10 ms of 48 kHz stereo `f32` is 3.8 KiB; Opus at 64 kbps is 80 bytes. 8 KiB
/// leaves generous headroom while still bounding what a hostile packet can make
/// us allocate.
///
/// This bounds the bytes *after* the header, which for an encrypted frame includes
/// the 12-byte nonce and the 16-byte tag. One bound on the wire, so a receiver's
/// buffer of `HEADER_LEN + MAX_PAYLOAD_LEN` is always big enough and can never
/// silently truncate a datagram.
pub const MAX_PAYLOAD_LEN: usize = 8 * 1024;

/// Sample encoding of the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SampleFormat {
    /// Interleaved signed 16-bit little-endian. Broadest compat, half the
    /// bandwidth of `F32`.
    I16 = 0,
    /// Interleaved 32-bit float in `[-1.0, 1.0]`. Matches what both Android
    /// (`ENCODING_PCM_FLOAT`) and CoreAudio/WASAPI natively deal in, so it
    /// avoids a conversion step on each end.
    F32 = 1,
    /// One Opus packet. **Frozen**, so the codec work and the phone client can be
    /// written against a tag that will not move.
    ///
    /// Unlike the PCM tags, `samples` does not determine the payload length: an
    /// Opus packet is as long as the encoder made it and the length is carried in
    /// the header. `samples` still says how many frames per channel the packet
    /// decodes to, so a receiver knows what it will get before it decodes.
    Opus = 2,
}

impl SampleFormat {
    /// Bytes used by one sample, or `None` when the payload length cannot be
    /// derived from the sample count.
    ///
    /// `Option` rather than a sentinel like `0`, because every caller of this
    /// function is computing a buffer length, and a wrong length is how a decoder
    /// reads past the end of a packet.
    pub const fn bytes_per_sample(self) -> Option<usize> {
        match self {
            SampleFormat::I16 => Some(2),
            SampleFormat::F32 => Some(4),
            SampleFormat::Opus => None,
        }
    }

    /// True for the encodings whose payload length follows from `samples`.
    pub const fn is_pcm(self) -> bool {
        matches!(self, SampleFormat::I16 | SampleFormat::F32)
    }

    /// Decodes the format tag, returning `None` for unknown values.
    ///
    /// Unknown tags must be *rejected*, never guessed: a peer that does not speak
    /// Opus must not parse an Opus packet as PCM and play the noise, because that
    /// failure is heard as "the microphone is broken" rather than as a version
    /// mismatch.
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(SampleFormat::I16),
            1 => Some(SampleFormat::F32),
            2 => Some(SampleFormat::Opus),
            _ => None,
        }
    }
}

/// Per-datagram flag bits.
pub mod flags {
    /// Payload carries Opus in-band FEC (M1+).
    pub const FEC: u8 = 0b0000_0001;
    /// Encoder was in discontinuous-transmission mode; payload is a comfort
    /// noise marker rather than real audio (M1+).
    pub const DTX: u8 = 0b0000_0010;
    /// Sender detected no voice activity and sent this frame deliberately.
    pub const SILENCE: u8 = 0b0000_0100;
    /// Duplicate of an earlier frame, sent for loss resilience (M1+).
    pub const REDUNDANT: u8 = 0b0000_1000;
    /// Payload is authenticated+encrypted with AES-256-GCM: 12 bytes of nonce
    /// followed by ciphertext (M2+).
    pub const ENCRYPTED: u8 = 0b0001_0000;
}

/// Control-plane messages and their newline-delimited JSON framing.
///
/// Public because the pairing handshake is what decides whether audio is accepted:
/// a host cannot enforce a policy it cannot speak.
pub mod control;

mod header;

pub use control::ControlMsg;
pub use header::{AudioHeader, ParseError};

/// Pairing and payload encryption.
///
/// Re-exported by `pub use` rather than duplicated, because there must be exactly
/// one implementation of each primitive in a build: a second copy of a GCM seal is
/// a second place for the same subtle mistake to live. It is re-exported *here*
/// because this is the crate every consumer already depends on — the receive path
/// in `talkee-core` and the two CLI commands included — so nothing has to grow a
/// dependency it does not own.
pub use talkee_crypto as crypto;

/// Bytes an encrypted payload adds to its plaintext: the nonce and the tag.
///
/// Named here because the header's payload-length rule is stated in terms of it,
/// and the crypto crate is what knows how big they really are.
pub use talkee_crypto::AEAD_OVERHEAD;

