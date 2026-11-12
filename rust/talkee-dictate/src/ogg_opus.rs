//! Ogg Opus: a dictation, about a tenth the size of WAV.
//!
//! Measured on 2026-11-12 from this PC: uploading to Groq ran at 70–210 KB/s, and transcription
//! time grew with the upload (a 15 s WAV of 488 KB took 2.4–3.3 s; the server itself answers in
//! well under a second). Speech at 24 kbps Opus is about 3 KB a second instead of WAV's 32, so the
//! upload stops being the slow part.
//!
//! The container is written by hand: two header packets and pages of audio packets, per RFC 7845
//! (Ogg Opus) and RFC 3533 (Ogg).

use anyhow::{Context, Result};
use opus::{Application, Bitrate, Channels, Encoder};

use crate::audio::TARGET_RATE;

/// Opus frame length: 20 ms at 16 kHz.
const FRAME: usize = 320;
/// Speech bitrate. Whisper's accuracy at this rate is indistinguishable from the WAV in testing.
const BITRATE: i32 = 24_000;
/// Opus granule positions always count 48 kHz samples, whatever the input rate.
const GRANULE_PER_SAMPLE: u64 = 48_000 / TARGET_RATE as u64;
/// Packets per Ogg page: one second of audio, so page headers stay a small overhead.
const PACKETS_PER_PAGE: usize = 50;

/// Encodes 16 kHz mono samples as an Ogg Opus file.
pub fn encode(samples: &[f32]) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new(TARGET_RATE, Channels::Mono, Application::Voip)
        .context("creating the Opus encoder")?;
    encoder
        .set_bitrate(Bitrate::Bits(BITRATE))
        .context("setting the Opus bitrate")?;
    // Lookahead is in input samples; the header's pre-skip is in 48 kHz samples.
    let lookahead = encoder
        .get_lookahead()
        .context("reading the Opus lookahead")?
        .max(0) as usize;
    let pre_skip = (lookahead as u64 * GRANULE_PER_SAMPLE) as u16;

    // Feed the encoder the lookahead's worth of extra silence so the last real samples come out.
    let total = samples.len() + lookahead;
    let frames = total.div_ceil(FRAME);
    let mut padded = samples.to_vec();
    padded.resize(frames * FRAME, 0.0);

    let mut ogg = OggWriter::new(serial());
    ogg.page(&[&opus_head(pre_skip)], 0, PageKind::First);
    ogg.page(&[&opus_tags()], 0, PageKind::Middle);

    let packets: Vec<Vec<u8>> = padded
        .chunks_exact(FRAME)
        .map(|frame| encoder.encode_vec_float(frame, 1_500))
        .collect::<Result<_, _>>()
        .context("encoding Opus")?;
    // The last page's granule tells a decoder where the real audio ends, trimming the padding.
    let end_granule = pre_skip as u64 + samples.len() as u64 * GRANULE_PER_SAMPLE;
    let pages = packets.chunks(PACKETS_PER_PAGE).count();
    for (index, chunk) in packets.chunks(PACKETS_PER_PAGE).enumerate() {
        let last = index + 1 == pages;
        let done = ((index * PACKETS_PER_PAGE + chunk.len()) * FRAME) as u64 * GRANULE_PER_SAMPLE;
        let granule = if last { end_granule } else { done };
        let refs: Vec<&[u8]> = chunk.iter().map(Vec::as_slice).collect();
        ogg.page(
            &refs,
            granule,
            if last {
                PageKind::Last
            } else {
                PageKind::Middle
            },
        );
    }
    Ok(ogg.finish())
}

fn serial() -> u32 {
    let mut bytes = [0u8; 4];
    let _ = getrandom::getrandom(&mut bytes);
    u32::from_le_bytes(bytes)
}

/// The identification header (RFC 7845 §5.1).
fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.push(1); // version
    head.push(1); // channels
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&TARGET_RATE.to_le_bytes()); // original input rate, informational
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family: mono/stereo
    head
}

/// The comment header (RFC 7845 §5.2), with no comments.
fn opus_tags() -> Vec<u8> {
    let vendor = b"talkee";
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());
    tags
}

#[derive(Clone, Copy, PartialEq)]
enum PageKind {
    First,
    Middle,
    Last,
}

struct OggWriter {
    serial: u32,
    sequence: u32,
    out: Vec<u8>,
}

impl OggWriter {
    fn new(serial: u32) -> Self {
        OggWriter {
            serial,
            sequence: 0,
            out: Vec::new(),
        }
    }

    /// Writes one page holding whole packets. Callers keep pages under 255 lacing values.
    fn page(&mut self, packets: &[&[u8]], granule: u64, kind: PageKind) {
        let mut lacing = Vec::new();
        for packet in packets {
            // A packet's length is a run of 255s and a final value below 255 (0 if it divides).
            lacing.extend(std::iter::repeat(255u8).take(packet.len() / 255));
            lacing.push((packet.len() % 255) as u8);
        }
        debug_assert!(lacing.len() <= 255, "too many packets for one Ogg page");

        let start = self.out.len();
        self.out.extend_from_slice(b"OggS");
        self.out.push(0); // version
        self.out.push(match kind {
            PageKind::First => 0x02, // beginning of stream
            PageKind::Middle => 0x00,
            PageKind::Last => 0x04, // end of stream
        });
        self.out.extend_from_slice(&granule.to_le_bytes());
        self.out.extend_from_slice(&self.serial.to_le_bytes());
        self.out.extend_from_slice(&self.sequence.to_le_bytes());
        self.out.extend_from_slice(&[0; 4]); // checksum, filled in below
        self.out.push(lacing.len() as u8);
        self.out.extend_from_slice(&lacing);
        for packet in packets {
            self.out.extend_from_slice(packet);
        }
        let crc = crc32(&self.out[start..]);
        self.out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
        self.sequence += 1;
    }

    fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Ogg's CRC-32: polynomial 0x04C11DB7, not reflected, zero initial value, no final XOR.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_ogg_check_value() {
        // Ogg's CRC is CRC-32/POSIX without the final XOR. POSIX's check value for "123456789" is
        // 0x765E7680, so Ogg's is that XOR 0xFFFFFFFF.
        assert_eq!(crc32(b"123456789"), 0x89A1_897F);
    }

    #[test]
    fn writes_valid_pages() {
        let samples: Vec<f32> = (0..TARGET_RATE as usize * 3)
            .map(|i| (i as f32 * 0.07).sin() * 0.3)
            .collect();
        let file = encode(&samples).unwrap();

        // Walk the pages: each starts with OggS, has a correct checksum, and sequence numbers count up.
        let mut pos = 0;
        let mut sequence = 0;
        let mut flags = Vec::new();
        let mut last_granule = 0;
        while pos < file.len() {
            assert_eq!(&file[pos..pos + 4], b"OggS");
            flags.push(file[pos + 5]);
            last_granule = u64::from_le_bytes(file[pos + 6..pos + 14].try_into().unwrap());
            assert_eq!(
                u32::from_le_bytes(file[pos + 18..pos + 22].try_into().unwrap()),
                sequence
            );
            let segments = file[pos + 26] as usize;
            let body: usize = file[pos + 27..pos + 27 + segments]
                .iter()
                .map(|&b| b as usize)
                .sum();
            let end = pos + 27 + segments + body;
            let mut page = file[pos..end].to_vec();
            let stored = u32::from_le_bytes(page[22..26].try_into().unwrap());
            page[22..26].fill(0);
            assert_eq!(crc32(&page), stored);
            pos = end;
            sequence += 1;
        }
        assert_eq!(pos, file.len());
        assert_eq!(flags.first(), Some(&0x02));
        assert_eq!(flags.last(), Some(&0x04));
        assert_eq!(&file[28..36], b"OpusHead");

        let pre_skip = u16::from_le_bytes([file[28 + 10], file[28 + 11]]) as u64;
        assert_eq!(last_granule, pre_skip + samples.len() as u64 * 3);

        // 3 s of speech-like audio at 24 kbps: about 9 KB, against 96 KB of WAV.
        assert!(file.len() < 15_000, "{} bytes", file.len());
    }
}
