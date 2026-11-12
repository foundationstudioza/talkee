//! Turning captured samples into the file the speech-to-text API receives.
//!
//! Whisper works at 16 kHz mono, and resamples anything else itself. Sending it 16 kHz 16-bit WAV
//! costs 32 KB per second (about 2 MB a minute), needs no encoder, and loses nothing the model
//! would have used.

use anyhow::{bail, Context, Result};

/// The rate Whisper-family models work at.
pub const TARGET_RATE: u32 = 16_000;

/// Recordings shorter than this are treated as a slip of the finger, not a dictation.
pub const MIN_SECONDS: f32 = 0.3;

/// Below this peak level a recording holds no speech.
///
/// This matters more than it looks: given silence, Whisper models invent text ("Thank you.",
/// "Subtitles by …"), and that invented text would be typed into whatever app has focus.
pub const SILENCE_PEAK: f32 = 0.01;

/// Why a recording was not worth sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Shorter than [`MIN_SECONDS`].
    TooShort,
    /// No sample louder than [`SILENCE_PEAK`].
    Silent,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::TooShort => write!(f, "the recording was too short"),
            Rejection::Silent => write!(f, "no speech was heard (is the microphone muted?)"),
        }
    }
}

/// Checks a 16 kHz mono recording before it is sent anywhere.
pub fn check(samples: &[f32]) -> Result<(), Rejection> {
    if (samples.len() as f32) < MIN_SECONDS * TARGET_RATE as f32 {
        return Err(Rejection::TooShort);
    }
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    if peak < SILENCE_PEAK {
        return Err(Rejection::Silent);
    }
    Ok(())
}

/// Silence kept before the first and after the last sound, so the edges of words survive.
const TRIM_MARGIN: usize = TARGET_RATE as usize / 4;
/// Window used to find where sound starts and ends: 20 ms.
const TRIM_WINDOW: usize = TARGET_RATE as usize / 50;

/// Cuts leading and trailing silence from 16 kHz mono audio, keeping a quarter-second margin.
///
/// The moments between pressing the key and speaking, and between finishing and releasing it, are
/// often a second or more. Not sending them shortens the upload, and gives the model less silence
/// to invent words in.
pub fn trim_silence(samples: &[f32]) -> &[f32] {
    let loud = |w: &[f32]| w.iter().any(|s| s.abs() >= SILENCE_PEAK);
    let windows: Vec<bool> = samples.chunks(TRIM_WINDOW).map(loud).collect();
    let (Some(first), Some(last)) = (
        windows.iter().position(|&l| l),
        windows.iter().rposition(|&l| l),
    ) else {
        return samples;
    };
    let start = (first * TRIM_WINDOW).saturating_sub(TRIM_MARGIN);
    let end = ((last + 1) * TRIM_WINDOW + TRIM_MARGIN).min(samples.len());
    &samples[start..end]
}

/// Downmixes interleaved frames to mono by averaging the channels.
pub fn to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    if channels == 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Resamples mono audio to 16 kHz.
///
/// Each output sample averages the input samples that fall in its period (a box filter), then the
/// result is read off at the exact output time. The box filter is a crude low-pass, but it is
/// enough to keep content above 8 kHz from folding down into the speech band, which plain
/// interpolation would do. For 48 kHz, the common case, it is exactly "average every 3 samples".
pub fn to_16k(mono: &[f32], rate: u32) -> Vec<f32> {
    if rate == TARGET_RATE || mono.is_empty() {
        return mono.to_vec();
    }
    let step = rate as f64 / TARGET_RATE as f64;
    if step < 1.0 {
        // Upsampling (an 8 kHz source): there is nothing above 4 kHz to alias, so interpolate.
        let out_len = (mono.len() as f64 / step).floor() as usize;
        return (0..out_len)
            .map(|i| {
                let pos = i as f64 * step;
                let idx = pos.floor() as usize;
                let frac = (pos - idx as f64) as f32;
                let a = mono[idx];
                let b = *mono.get(idx + 1).unwrap_or(&a);
                a + (b - a) * frac
            })
            .collect();
    }
    let out_len = (mono.len() as f64 / step).floor() as usize;
    (0..out_len)
        .map(|i| {
            let start = (i as f64 * step).floor() as usize;
            let end = (((i + 1) as f64 * step).floor() as usize).clamp(start + 1, mono.len());
            let window = &mono[start..end];
            window.iter().sum::<f32>() / window.len() as f32
        })
        .collect()
}

/// Encodes 16 kHz mono samples as a 16-bit PCM WAV file.
pub fn wav_16k(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&TARGET_RATE.to_le_bytes());
    out.extend_from_slice(&(TARGET_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// A decoded WAV file: mono samples and their rate.
#[derive(Debug)]
pub struct Decoded {
    /// Mono samples in -1.0..=1.0.
    pub samples: Vec<f32>,
    /// Sample rate in Hz.
    pub rate: u32,
}

/// Reads a 16-bit PCM WAV file (any rate, any channel count), for testing with recordings.
pub fn read_wav(bytes: &[u8]) -> Result<Decoded> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let mut pos = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = bytes
            .get(pos + 8..pos + 8 + len)
            .context("WAV chunk runs past the end of the file")?;
        if id == b"fmt " {
            if body.len() < 16 {
                bail!("WAV format chunk is too short");
            }
            let le16 = |o: usize| u16::from_le_bytes([body[o], body[o + 1]]);
            let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            format = Some((le16(0), le16(2), rate, le16(14)));
        } else if id == b"data" {
            let (tag, channels, rate, bits) = format.context("WAV data comes before its format")?;
            if tag != 1 || bits != 16 {
                bail!("only 16-bit PCM WAV is supported (format {tag}, {bits} bits)");
            }
            let interleaved: Vec<f32> = body
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                .collect();
            return Ok(Decoded {
                samples: to_mono(&interleaved, channels),
                rate,
            });
        }
        pos += 8 + len + (len & 1); // chunks are padded to an even length
    }
    bail!("WAV file has no data chunk")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trips() {
        let samples: Vec<f32> = (0..1600).map(|i| ((i as f32) * 0.05).sin() * 0.5).collect();
        let decoded = read_wav(&wav_16k(&samples)).unwrap();
        assert_eq!(decoded.rate, 16_000);
        assert_eq!(decoded.samples.len(), samples.len());
        for (a, b) in samples.iter().zip(&decoded.samples) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn downsamples_48k_by_averaging_threes() {
        let input = [0.0, 0.3, 0.6, 1.0, 1.0, 1.0];
        assert_eq!(to_16k(&input, 48_000), vec![0.3, 1.0]);
    }

    #[test]
    fn downsampling_suppresses_content_above_8khz() {
        // 12 kHz at 48 kHz: a 4-sample period. Averaging 3 of them leaves a third of the amplitude
        // rather than folding it down at full strength.
        let input: Vec<f32> = (0..4800)
            .map(|i| (i as f32 * std::f32::consts::TAU * 12_000.0 / 48_000.0).sin())
            .collect();
        let out = to_16k(&input, 48_000);
        assert_eq!(out.len(), 1600);
        assert!(out.iter().all(|s| s.abs() <= 0.34));
    }

    #[test]
    fn length_follows_the_rate() {
        assert_eq!(to_16k(&vec![0.0; 44_100], 44_100).len(), 16_000);
        assert_eq!(to_16k(&vec![0.0; 8_000], 8_000).len(), 16_000);
    }

    #[test]
    fn rejects_short_and_silent_recordings() {
        assert_eq!(check(&[0.5; 1000]), Err(Rejection::TooShort));
        assert_eq!(check(&[0.001; 16_000]), Err(Rejection::Silent));
        assert_eq!(check(&[0.2; 16_000]), Ok(()));
    }

    #[test]
    fn trims_silence_but_keeps_a_margin() {
        let mut samples = vec![0.0; 16_000];
        samples.extend(vec![0.5; 8_000]);
        samples.extend(vec![0.0; 16_000]);
        let trimmed = trim_silence(&samples);
        assert_eq!(trimmed.len(), 8_000 + 2 * TRIM_MARGIN);
        assert!(
            trim_silence(&[0.0; 1000]).len() == 1000,
            "all-silent audio is left for check()"
        );
    }

    #[test]
    fn mixes_stereo_down() {
        assert_eq!(to_mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
    }
}
