//! The fixed header that prefixes every audio datagram, plus its parser.
//!
//! ```text
//! offset  size  field
//!      0     2  magic        u16 le  == MAGIC
//!      2     1  version      u8      == PROTOCOL_VERSION
//!      3     1  flags        u8      bitfield, see `flags`
//!      4     4  seq          u32 le  monotonic per sender, wraps
//!      8     4  ts_ms        u32 le  sender's monotonic clock, milliseconds
//!     12     4  sample_rate  u32 le  Hz
//!     16     2  samples      u16 le  frames per channel in this payload
//!     18     1  channels     u8
//!     19     1  format       u8      see `SampleFormat`
//!     20     4  payload_len  u32 le  bytes following this header
//! ```
//!
//! `payload_len` counts the bytes **after** the header. With `flags::ENCRYPTED`
//! set, those bytes are `[12-byte nonce][ciphertext][16-byte tag]`, so the
//! declared length is the plaintext length plus [`AEAD_OVERHEAD`], and the
//! plaintext length is what `samples`, `channels` and `format` imply. Both the
//! sender and the receiver derive the same number from the same fields, which is
//! what makes a lying `payload_len` a rejected datagram instead of an
//! out-of-bounds read.
//!
//! [`AEAD_OVERHEAD`]: crate::AEAD_OVERHEAD

use crate::{flags, SampleFormat, AEAD_OVERHEAD, HEADER_LEN, MAGIC, MAX_PAYLOAD_LEN, PROTOCOL_VERSION};
use std::ops::RangeInclusive;

/// Metadata for one audio datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// Bitfield of [`crate::flags`].
    pub flags: u8,
    /// Monotonic sequence number, incremented once per frame.
    pub seq: u32,
    /// Sender clock in milliseconds, for one-way delay and drift measurement.
    pub ts_ms: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Frames **per channel** in the payload.
    pub samples: u16,
    /// Interleaved channel count.
    pub channels: u8,
    /// Payload encoding.
    pub format: SampleFormat,
    /// Length of the payload following the header, in bytes.
    pub payload_len: u32,
}

impl AudioHeader {
    /// Builds a header, deriving `payload_len` from `samples`, `channels`, `format`
    /// and the `ENCRYPTED` flag so it cannot disagree with them.
    ///
    /// This is complete for a PCM format: the length follows from the sample count,
    /// and 28 bytes are added when the frame is to be encrypted. For `Opus` the
    /// length cannot be derived — a packet is as long as the encoder made it — so
    /// this leaves `payload_len` at zero, which a receiver rejects, and the caller
    /// must use [`AudioHeader::new_variable_len`]. Zero rather than a plausible
    /// guess: a plausible guess on the wire is a decoder reading past the end of a
    /// packet, and silence is a better failure than noise.
    pub fn new(
        seq: u32,
        ts_ms: u32,
        sample_rate: u32,
        samples: u16,
        channels: u8,
        format: SampleFormat,
        flags: u8,
    ) -> Self {
        let plain = match format.bytes_per_sample() {
            Some(bps) => samples as u32 * channels as u32 * bps as u32,
            None => 0,
        };
        let payload_len = plain + if flags & flags::ENCRYPTED != 0 { AEAD_OVERHEAD as u32 } else { 0 };
        Self {
            flags,
            seq,
            ts_ms,
            sample_rate,
            samples,
            channels,
            format,
            payload_len,
        }
    }

    /// Builds a header for a payload whose length does not follow from `samples`
    /// (Opus), returning `None` for a length a receiver would refuse.
    ///
    /// It checks the length against the same rule the parser applies, and refuses
    /// the degenerate field values the parser refuses, so a header built here is
    /// always one the far end accepts. A sender and a receiver holding two
    /// opinions about what a valid frame is happens to be the bug this function
    /// exists to make impossible.
    pub fn new_variable_len(
        seq: u32,
        ts_ms: u32,
        sample_rate: u32,
        samples: u16,
        channels: u8,
        format: SampleFormat,
        payload_len: usize,
        flags: u8,
    ) -> Option<Self> {
        if payload_len > u32::MAX as usize || channels == 0 || sample_rate == 0 {
            return None;
        }
        let header = Self {
            flags,
            seq,
            ts_ms,
            sample_rate,
            samples,
            channels,
            format,
            payload_len: payload_len as u32,
        };
        header.payload_len_bounds()?.contains(&payload_len).then_some(header)
    }

    /// Bytes the payload must contain, when the header determines that.
    ///
    /// `None` for a variable-length format: read `payload_len` itself.
    ///
    /// The value includes the AEAD overhead when `ENCRYPTED` is set, because this
    /// is the length of the bytes on the wire — the only length a receiver can
    /// check before it has decrypted anything.
    pub fn expected_payload_len(&self) -> Option<usize> {
        let bounds = self.payload_len_bounds()?;
        (bounds.start() == bounds.end()).then(|| *bounds.start())
    }

    /// The range of on-wire payload lengths this header permits.
    ///
    /// A single value for PCM, where the length is determined; a range for Opus,
    /// where the encoder chose it. `None` only for arithmetic overflow, which also
    /// stops a bogus `samples`/`channels` pair from driving a huge allocation.
    pub fn payload_len_bounds(&self) -> Option<RangeInclusive<usize>> {
        let overhead = if self.flags & flags::ENCRYPTED != 0 {
            AEAD_OVERHEAD
        } else {
            0
        };
        match self.format.bytes_per_sample() {
            Some(bytes_per_sample) => {
                let plain = (self.samples as usize)
                    .checked_mul(self.channels as usize)?
                    .checked_mul(bytes_per_sample)?;
                let exact = plain.checked_add(overhead)?;
                Some(exact..=exact)
            }
            None => {
                // Opus: any length the encoder produced, as long as at least one
                // byte of packet survives outside the nonce and tag.
                Some(1 + overhead..=MAX_PAYLOAD_LEN)
            }
        }
    }

    /// True when the payload is an AEAD envelope rather than plaintext.
    pub fn is_encrypted(&self) -> bool {
        self.flags & flags::ENCRYPTED != 0
    }

    /// Serializes into the fixed 24-byte little-endian layout.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..2].copy_from_slice(&MAGIC.to_le_bytes());
        out[2] = PROTOCOL_VERSION;
        out[3] = self.flags;
        out[4..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..12].copy_from_slice(&self.ts_ms.to_le_bytes());
        out[12..16].copy_from_slice(&self.sample_rate.to_le_bytes());
        out[16..18].copy_from_slice(&self.samples.to_le_bytes());
        out[18] = self.channels;
        out[19] = self.format as u8;
        out[20..24].copy_from_slice(&self.payload_len.to_le_bytes());
        out
    }

    /// Parses and validates a header from the front of `buf`.
    ///
    /// Returns [`ParseError`] instead of panicking: this is the boundary where
    /// untrusted network bytes enter the process.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.len() < HEADER_LEN {
            return Err(ParseError::Short);
        }
        let magic = u16::from_le_bytes([buf[0], buf[1]]);
        if magic != MAGIC {
            return Err(ParseError::BadMagic(magic));
        }
        let version = buf[2];
        if version != PROTOCOL_VERSION {
            return Err(ParseError::Version(version));
        }
        let format = SampleFormat::from_u8(buf[19]).ok_or(ParseError::Format(buf[19]))?;
        let header = Self {
            flags: buf[3],
            seq: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            ts_ms: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            sample_rate: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            samples: u16::from_le_bytes([buf[16], buf[17]]),
            channels: buf[18],
            format,
            payload_len: u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
        };
        if header.channels == 0 {
            return Err(ParseError::Channels(0));
        }
        if header.sample_rate == 0 {
            return Err(ParseError::SampleRate(0));
        }
        // Opus carries at most two channels in this protocol: the phone captures
        // one or two, and a packet claiming more is either a bug or a version that
        // should have said so.
        if header.format == SampleFormat::Opus && header.channels > 2 {
            return Err(ParseError::Channels(header.channels));
        }
        let payload_len = header.payload_len as usize;
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(ParseError::PayloadTooLarge(payload_len));
        }
        // A payload length that disagrees with the rest of the header is a bug or an
        // attack. Refuse it rather than indexing out of bounds later. For PCM there
        // is exactly one possible length (plus the AEAD overhead when encrypted);
        // for Opus the encoder picked it, so there is a range.
        let bounds = header
            .payload_len_bounds()
            .ok_or(ParseError::PayloadTooLarge(payload_len))?;
        if !bounds.contains(&payload_len) {
            let (min, max) = (*bounds.start(), *bounds.end());
            if min == max {
                return Err(ParseError::PayloadMismatch {
                    declared: payload_len,
                    expected: min,
                });
            }
            return Err(ParseError::PayloadOutOfRange {
                declared: payload_len,
                min,
                max,
            });
        }
        Ok(header)
    }

    /// Milliseconds of audio this frame carries.
    pub fn frame_duration_ms(&self) -> f32 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.samples as f32 * 1000.0 / self.sample_rate as f32
    }

    /// True if the sender marked this frame as carrying no voice.
    pub fn is_silence(&self) -> bool {
        self.flags & flags::SILENCE != 0
    }
}

impl AudioHeader {
    /// Decodes an interleaved PCM payload into mono `f32` appended to `out`.
    ///
    /// Multi-channel payloads are averaged down to mono. Averaging (rather than
    /// taking channel 0) is right for a microphone: if the phone hands us a
    /// stereo stream with one dead channel, we still hear the other one.
    ///
    /// Returns the number of mono frames written. A payload shorter than
    /// `samples` claims decodes what is actually there and stops, so a
    /// truncated datagram degrades instead of panicking.
    pub fn decode_pcm_mono(&self, payload: &[u8], out: &mut Vec<f32>) -> usize {
        let channels = self.channels as usize;
        let frames = self.samples as usize;
        if channels == 0 || frames == 0 {
            return 0;
        }
        // An Opus packet is not PCM, and decoding it as if it were would play
        // noise — heard as "the microphone is broken" rather than as a codec
        // mismatch. The codec path owns `format = 2`; this one refuses it.
        let Some(bytes_per_sample) = self.format.bytes_per_sample() else {
            return 0;
        };
        out.reserve(frames);
        let stride = bytes_per_sample * channels;
        for frame in 0..frames {
            let base = frame * stride;
            if base + stride > payload.len() {
                return frame;
            }
            let mut acc = 0.0f32;
            for channel in 0..channels {
                let idx = base + channel * bytes_per_sample;
                acc += match bytes_per_sample {
                    // 2 bytes per sample is I16 and 4 is F32; those are the only
                    // values `bytes_per_sample` can return for a PCM tag, and the
                    // stride check above is what makes these indexes safe.
                    2 => i16::from_le_bytes([payload[idx], payload[idx + 1]]) as f32 / 32768.0,
                    _ => f32::from_le_bytes([
                        payload[idx],
                        payload[idx + 1],
                        payload[idx + 2],
                        payload[idx + 3],
                    ]),
                };
            }
            out.push(acc / channels as f32);
        }
        frames
    }
}

/// Reasons a datagram header can be rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Datagram shorter than [`HEADER_LEN`].
    Short,
    /// First two bytes were not [`MAGIC`] — almost certainly not our traffic.
    BadMagic(u16),
    /// Peer speaks a different protocol version.
    Version(u8),
    /// Unknown `SampleFormat` tag.
    Format(u8),
    /// Channel count was zero.
    Channels(u8),
    /// Sample rate was zero.
    SampleRate(u32),
    /// `payload_len` exceeded [`MAX_PAYLOAD_LEN`].
    PayloadTooLarge(usize),
    /// `payload_len` disagreed with `samples * channels * bytes_per_sample` (plus
    /// [`AEAD_OVERHEAD`](crate::AEAD_OVERHEAD) when `ENCRYPTED` is set).
    PayloadMismatch {
        /// What the header claimed.
        declared: usize,
        /// What the other fields imply.
        expected: usize,
    },
    /// `payload_len` is outside the range the header permits, which happens only
    /// for a variable-length format: an Opus packet may be any size the encoder
    /// chose, within the cap.
    PayloadOutOfRange {
        /// What the header claimed.
        declared: usize,
        /// Smallest length that would have been accepted.
        min: usize,
        /// Largest length that would have been accepted.
        max: usize,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Short => write!(f, "datagram shorter than {HEADER_LEN}-byte header"),
            ParseError::BadMagic(m) => write!(f, "bad magic 0x{m:04X} (expected 0x{MAGIC:04X})"),
            ParseError::Version(v) => {
                write!(f, "peer speaks protocol version {v}, we speak {PROTOCOL_VERSION}")
            }
            ParseError::Format(v) => write!(f, "unknown sample format tag {v}"),
            ParseError::Channels(v) => write!(f, "unsupported channel count {v}"),
            ParseError::SampleRate(v) => write!(f, "unsupported sample rate {v}"),
            ParseError::PayloadTooLarge(n) => {
                write!(f, "payload of {n} bytes exceeds the {MAX_PAYLOAD_LEN}-byte cap")
            }
            ParseError::PayloadMismatch { declared, expected } => write!(
                f,
                "payload_len {declared} disagrees with the length the other header fields imply ({expected})"
            ),
            ParseError::PayloadOutOfRange { declared, min, max } => write!(
                f,
                "payload_len {declared} is outside the permitted range {min}..={max} for this header"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(header: AudioHeader) {
        let bytes = header.to_bytes();
        let parsed = AudioHeader::from_bytes(&bytes).expect("valid header must parse");
        assert_eq!(parsed, header);
    }

    #[test]
    fn header_is_exactly_24_bytes() {
        let h = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0);
        assert_eq!(h.to_bytes().len(), HEADER_LEN);
    }

    #[test]
    fn header_roundtrips() {
        roundtrip(AudioHeader::new(7, 1234, 48_000, 480, 1, SampleFormat::F32, flags::SILENCE));
        roundtrip(AudioHeader::new(u32::MAX, 0, 16_000, 160, 2, SampleFormat::I16, flags::FEC));
    }

    #[test]
    fn payload_len_is_derived_not_declared() {
        let h = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0);
        assert_eq!(h.payload_len, 480 * 4);
        let h = AudioHeader::new(0, 0, 48_000, 480, 2, SampleFormat::I16, 0);
        assert_eq!(h.payload_len, 480 * 2 * 2);
    }

    #[test]
    fn rejects_short_datagram() {
        assert_eq!(AudioHeader::from_bytes(&[0u8; 4]), Err(ParseError::Short));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0).to_bytes();
        bytes[0] ^= 0xFF;
        assert!(matches!(AudioHeader::from_bytes(&bytes), Err(ParseError::BadMagic(_))));
    }

    #[test]
    fn rejects_future_version() {
        let mut bytes = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0).to_bytes();
        bytes[2] = PROTOCOL_VERSION + 1;
        assert_eq!(
            AudioHeader::from_bytes(&bytes),
            Err(ParseError::Version(PROTOCOL_VERSION + 1))
        );
    }

    #[test]
    fn rejects_unknown_format_tag() {
        let mut bytes = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0).to_bytes();
        bytes[19] = 9;
        assert_eq!(AudioHeader::from_bytes(&bytes), Err(ParseError::Format(9)));
    }

    #[test]
    fn rejects_zero_channels() {
        let mut bytes = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0).to_bytes();
        bytes[18] = 0;
        assert!(matches!(AudioHeader::from_bytes(&bytes), Err(ParseError::Channels(0))));
    }

    #[test]
    fn rejects_lying_payload_len() {
        let mut bytes = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0).to_bytes();
        bytes[20..24].copy_from_slice(&4u32.to_le_bytes());
        assert!(matches!(
            AudioHeader::from_bytes(&bytes),
            Err(ParseError::PayloadMismatch { declared: 4, expected: 1920 })
        ));
    }

    #[test]
    fn rejects_oversized_payload() {
        let mut bytes = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0).to_bytes();
        bytes[16..18].copy_from_slice(&u16::MAX.to_le_bytes());
        bytes[20..24].copy_from_slice(&(MAX_PAYLOAD_LEN as u32 + 1).to_le_bytes());
        assert!(matches!(AudioHeader::from_bytes(&bytes), Err(ParseError::PayloadTooLarge(_))));
    }

    #[test]
    fn decodes_i16_mono() {
        let h = AudioHeader::new(0, 0, 48_000, 3, 1, SampleFormat::I16, 0);
        let mut payload = Vec::new();
        for s in [0i16, 16_384, -32_768] {
            payload.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = Vec::new();
        assert_eq!(h.decode_pcm_mono(&payload, &mut out), 3);
        assert!((out[0] - 0.0).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
        assert!((out[2] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn decodes_f32_stereo_as_mono_average() {
        let h = AudioHeader::new(0, 0, 48_000, 2, 2, SampleFormat::F32, 0);
        let mut payload = Vec::new();
        for s in [1.0f32, 0.0, -1.0, 1.0] {
            payload.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = Vec::new();
        assert_eq!(h.decode_pcm_mono(&payload, &mut out), 2);
        assert!((out[0] - 0.5).abs() < 1e-6, "got {}", out[0]);
        assert!((out[1] - 0.0).abs() < 1e-6, "got {}", out[1]);
    }

    #[test]
    fn decoding_truncated_payload_stops_cleanly() {
        let h = AudioHeader::new(0, 0, 48_000, 4, 1, SampleFormat::I16, 0);
        let payload = [0u8; 4]; // only 2 of the 4 declared frames are present
        let mut out = Vec::new();
        assert_eq!(h.decode_pcm_mono(&payload, &mut out), 2);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn frame_duration_and_silence_flag() {
        let h = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0);
        assert!((h.frame_duration_ms() - 10.0).abs() < 1e-6);
        let h = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, flags::SILENCE);
        assert!(h.is_silence());
    }

    #[test]
    fn the_opus_tag_is_frozen() {
        // Tag 2 is what the codec work and the phone client are written against.
        // Changing it now would silently reinterpret every Opus packet as PCM.
        assert_eq!(SampleFormat::Opus as u8, 2);
        assert_eq!(SampleFormat::from_u8(2), Some(SampleFormat::Opus));
        assert_eq!(SampleFormat::Opus.bytes_per_sample(), None);
        assert!(!SampleFormat::Opus.is_pcm());
        assert!(SampleFormat::I16.bytes_per_sample().is_some());
    }

    #[test]
    fn an_opus_packet_carries_its_own_length() {
        // 10 ms of 48 kHz mono, 80 bytes of Opus, which is what M1 targets.
        let header =
            AudioHeader::new_variable_len(1, 0, 48_000, 480, 1, SampleFormat::Opus, 80, 0)
                .expect("80 bytes is a plausible Opus packet");
        assert_eq!(header.payload_len, 80);
        assert_eq!(header.expected_payload_len(), None, "not derivable from `samples`");
        assert_eq!(header.payload_len_bounds(), Some(1..=MAX_PAYLOAD_LEN));
        let bytes = header.to_bytes();
        assert_eq!(AudioHeader::from_bytes(&bytes).unwrap(), header);
    }

    #[test]
    fn an_empty_opus_packet_is_refused() {
        let bytes = AudioHeader::new_variable_len(1, 0, 48_000, 480, 1, SampleFormat::Opus, 0, 0);
        assert!(bytes.is_none());
        // The same thing on the wire, so a receiver cannot be talked into it either.
        let mut raw = AudioHeader::new_variable_len(
            1, 0, 48_000, 480, 1, SampleFormat::Opus, 1, 0,
        )
        .unwrap()
        .to_bytes();
        raw[20..24].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            AudioHeader::from_bytes(&raw),
            Err(ParseError::PayloadOutOfRange { declared: 0, .. })
        ));
    }

    #[test]
    fn an_opus_frame_with_three_channels_is_refused() {
        let header =
            AudioHeader::new_variable_len(1, 0, 48_000, 480, 3, SampleFormat::Opus, 80, 0).unwrap();
        assert_eq!(AudioHeader::from_bytes(&header.to_bytes()), Err(ParseError::Channels(3)));
    }

    #[test]
    fn an_encrypted_header_reserves_room_for_the_nonce_and_the_tag() {
        // The 28 bytes are the point: a sender that forgot them would produce a
        // frame every receiver rejects, and the mistake would look like a dropped
        // stream rather than a length bug.
        let plain = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0);
        assert_eq!(plain.payload_len, 1_920);
        let sealed = AudioHeader::new(
            0,
            0,
            48_000,
            480,
            1,
            SampleFormat::F32,
            flags::ENCRYPTED,
        );
        assert_eq!(sealed.payload_len, 1_920 + AEAD_OVERHEAD as u32);
        assert!(sealed.is_encrypted());
        assert_eq!(sealed.expected_payload_len(), Some(1_920 + AEAD_OVERHEAD));
        assert_eq!(AudioHeader::from_bytes(&sealed.to_bytes()).unwrap(), sealed);
    }

    #[test]
    fn an_encrypted_header_declaring_a_plaintext_length_is_refused() {
        // This is the check that stops an encrypted frame from being read as if the
        // ciphertext were audio: 1920 bytes of GCM output is not 480 f32 samples.
        let mut raw = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, flags::ENCRYPTED)
            .to_bytes();
        raw[20..24].copy_from_slice(&1_920u32.to_le_bytes());
        assert_eq!(
            AudioHeader::from_bytes(&raw),
            Err(ParseError::PayloadMismatch {
                declared: 1_920,
                expected: 1_920 + AEAD_OVERHEAD,
            })
        );
    }

    #[test]
    fn an_encrypted_opus_frame_has_bounds_and_not_a_length() {
        let header = AudioHeader::new_variable_len(
            1,
            0,
            48_000,
            480,
            1,
            SampleFormat::Opus,
            80 + AEAD_OVERHEAD,
            flags::ENCRYPTED,
        )
        .expect("an encrypted Opus packet is its plaintext plus overhead");
        assert!(header.is_encrypted());
        assert_eq!(header.payload_len_bounds(), Some(1 + AEAD_OVERHEAD..=MAX_PAYLOAD_LEN));
        assert_eq!(AudioHeader::from_bytes(&header.to_bytes()).unwrap(), header);
        // One byte short of a nonce and a tag is not a sealed frame.
        let mut raw = header.to_bytes();
        raw[20..24].copy_from_slice(&(AEAD_OVERHEAD as u32).to_le_bytes());
        assert!(matches!(
            AudioHeader::from_bytes(&raw),
            Err(ParseError::PayloadOutOfRange { .. })
        ));
    }

    #[test]
    fn an_unknown_format_tag_is_still_rejected() {
        // Forward compatibility in the other direction: tag 3 is somebody else's,
        // and guessing is how a future codec becomes noise today.
        let mut bytes = AudioHeader::new(0, 0, 48_000, 480, 1, SampleFormat::F32, 0).to_bytes();
        bytes[19] = 3;
        assert_eq!(AudioHeader::from_bytes(&bytes), Err(ParseError::Format(3)));
    }
}


