//! Opus decoding for the receive path.
//!
//! # Why the phone compresses
//!
//! Raw 48 kHz samples are 1.5 Mbit/s: nothing on a good network, a lot of airtime on a crowded 2.4 GHz
//! one or a phone hotspot. Opus carries the same voice in about 40 times less, so a few lost datagrams
//! cost a few lost packets instead of a stuttering stream. It also gives the receiver something raw
//! samples never could: when a packet is missing, the decoder can *guess* the missing 20 ms from the
//! ones either side of it, which sounds far better than a hole of silence.
//!
//! # What this module owns
//!
//! One decoder per sender, mono at 48 kHz. [`OpusStream::decode`] appends the audio of a packet to a
//! buffer, after the audio it concealed for any packets known to be missing in front of it. The
//! decoder keeps state between packets, so it has to be [reset](OpusStream::reset) when the stream
//! restarts (a new pairing, a jump in sequence numbers, a mute) or the first packet after the gap
//! would be decoded as though it continued the last.

use anyhow::{Context, Result};
use opus::{Channels, Decoder};

/// Most samples one packet can hold: 120 ms at 48 kHz.
const MAX_PACKET_SAMPLES: usize = 5_760;

/// Most missing packets concealed in front of one that arrived.
///
/// Concealment is a guess, and a long guess is a droning noise. Past this the gap is left to the
/// buffer's silence, which is at least honest.
pub const MAX_CONCEALED: usize = 3;

/// The state of one Opus stream.
pub struct OpusStream {
    decoder: Decoder,
    /// Samples in the last packet decoded, which is how long a concealed packet is taken to be.
    last_packet_samples: usize,
    scratch: Vec<f32>,
}

impl OpusStream {
    /// A decoder for mono 48 kHz audio.
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: Decoder::new(48_000, Channels::Mono).context("creating the Opus decoder")?,
            last_packet_samples: 0,
            scratch: vec![0.0; MAX_PACKET_SAMPLES],
        })
    }

    /// Forgets everything about the stream so far.
    pub fn reset(&mut self) {
        let _ = self.decoder.reset_state();
        self.last_packet_samples = 0;
    }

    /// Appends the audio of `packet` to `out`, preceded by a guess at each of `lost` packets that
    /// should have come before it. Returns how many samples were appended in all.
    ///
    /// A packet that is not valid Opus appends nothing: a damaged datagram is dropped, not played.
    pub fn decode(&mut self, packet: &[u8], lost: usize, out: &mut Vec<f32>) -> usize {
        let Ok(samples) = self.decoder.get_nb_samples(packet) else {
            return 0;
        };
        if samples == 0 || samples > MAX_PACKET_SAMPLES {
            return 0;
        }
        let before = out.len();
        if self.last_packet_samples > 0 {
            for _ in 0..lost.min(MAX_CONCEALED) {
                let n = self.last_packet_samples;
                // An empty input is how libopus is asked to conceal a packet it never received.
                if let Ok(written) = self.decoder.decode_float(&[], &mut self.scratch[..n], false) {
                    out.extend_from_slice(&self.scratch[..written]);
                }
            }
        }
        match self.decoder.decode_float(packet, &mut self.scratch[..samples], false) {
            Ok(written) => {
                out.extend_from_slice(&self.scratch[..written]);
                self.last_packet_samples = written;
            }
            Err(_) => {
                out.truncate(before);
                return 0;
            }
        }
        out.len() - before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus::{Application, Bitrate, Encoder};

    /// A voice-like test signal: a buzz with harmonics, so the codec has something to chew on.
    fn tone(n: usize, offset: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = (i + offset) as f32 / 48_000.0;
                (1..=6)
                    .map(|h| (2.0 * std::f32::consts::PI * 180.0 * h as f32 * t).sin() / h as f32)
                    .sum::<f32>()
                    * 0.25
            })
            .collect()
    }

    fn encode(frames: usize, frame: usize) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::Voip).unwrap();
        encoder.set_bitrate(Bitrate::Bits(48_000)).unwrap();
        let signal = tone(frames * frame, 0);
        signal
            .chunks_exact(frame)
            .map(|chunk| encoder.encode_vec_float(chunk, 1_500).unwrap())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn a_stream_of_packets_comes_back_as_audio_of_the_same_length_and_level() {
        let packets = encode(50, 960);
        let mut stream = OpusStream::new().unwrap();
        let mut out = Vec::new();
        for packet in &packets {
            assert_eq!(stream.decode(packet, 0, &mut out), 960);
        }
        assert_eq!(out.len(), 50 * 960);
        let want = rms(&tone(50 * 960, 0)[24_000..]);
        let got = rms(&out[24_000..]);
        assert!((got / want - 1.0).abs() < 0.25, "level {got} against {want}");
    }

    #[test]
    fn a_missing_packet_is_filled_with_a_guess_rather_than_left_as_a_hole() {
        let packets = encode(40, 960);
        let mut stream = OpusStream::new().unwrap();
        let mut out = Vec::new();
        for packet in &packets[..30] {
            stream.decode(packet, 0, &mut out);
        }
        let before = out.len();
        // Packet 30 never arrived; 31 did.
        let appended = stream.decode(&packets[31], 1, &mut out);
        assert_eq!(appended, 2 * 960, "one concealed packet plus the real one");
        let concealed = &out[before..before + 960];
        assert!(rms(concealed) > 0.05, "the guess should carry sound, not silence");
    }

    #[test]
    fn concealment_stops_after_a_few_packets() {
        let packets = encode(20, 960);
        let mut stream = OpusStream::new().unwrap();
        let mut out = Vec::new();
        stream.decode(&packets[0], 0, &mut out);
        out.clear();
        let appended = stream.decode(&packets[15], 14, &mut out);
        assert_eq!(appended, (MAX_CONCEALED + 1) * 960);
    }

    #[test]
    fn bytes_that_are_not_opus_add_nothing() {
        let mut stream = OpusStream::new().unwrap();
        let mut out = vec![0.5];
        assert_eq!(stream.decode(&[], 0, &mut out), 0);
        assert_eq!(out, vec![0.5], "nothing was appended");
        // A reserved frame-count code with an impossible length.
        assert_eq!(stream.decode(&[0xFF], 0, &mut out), 0);
    }

    #[test]
    fn a_reset_forgets_the_last_packet_so_nothing_is_concealed_after_it() {
        let packets = encode(5, 960);
        let mut stream = OpusStream::new().unwrap();
        let mut out = Vec::new();
        stream.decode(&packets[0], 0, &mut out);
        stream.reset();
        out.clear();
        assert_eq!(stream.decode(&packets[2], 3, &mut out), 960);
    }

    #[test]
    fn packets_are_a_small_fraction_of_raw_samples() {
        let packets = encode(20, 960);
        let mean = packets.iter().map(Vec::len).sum::<usize>() / packets.len();
        assert!(mean < 960 * 4 / 20, "{mean} bytes against {} raw", 960 * 4);
    }
}
