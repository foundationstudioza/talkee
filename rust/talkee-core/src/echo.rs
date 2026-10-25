//! Acoustic echo cancellation: taking the PC's own speaker output back out of the phone's microphone.
//!
//! # The problem this solves
//!
//! The phone's microphone hears the user *and* whatever the PC is playing. The PC knows exactly what
//! it is playing, so that sound can be subtracted from the microphone signal before it reaches the
//! virtual cable. That is echo cancellation, as opposed to noise suppression, which guesses.
//!
//! # How it is fed
//!
//! Two signals, both mono at [`RATE`]:
//!
//! * the **reference** — what the PC is sending to its speakers, captured elsewhere (WASAPI
//!   loopback) and pushed in through a [`RenderFeed`] from whatever thread captured it;
//! * the **microphone** — the phone's audio, passed to [`EchoCanceller::process`] on the receive
//!   thread, in place.
//!
//! The canceller is WebRTC's AEC3 (through the pure-Rust `sonora` port). It estimates the delay
//! between the two on its own, which matters because the microphone path includes Wi-Fi and the
//! reference path does not.
//!
//! # What it does not promise
//!
//! The two signals come from different clocks and the microphone path has jitter the reference does
//! not. AEC3 tolerates both, but this is not a phone's echo canceller, where microphone and speaker
//! share a clock. Measure with [`EchoCanceller::stats`] rather than assuming.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use sonora::config::{EchoCanceller as EchoConfig, HighPassFilter, NoiseSuppression};
use sonora::{AudioProcessing, Config, StreamConfig};

/// The only sample rate the canceller works at. The phone sends 48 kHz and the reference is
/// resampled to match by whoever feeds it.
pub const RATE: u32 = 48_000;

/// One AEC3 block: 10 ms at [`RATE`].
const FRAME: usize = 480;

/// How much reference audio may wait to be consumed, in samples (one second).
///
/// A bound rather than a convenience: if the receive side stalls — no phone connected, for
/// instance — the loopback thread keeps producing, and an unbounded queue would grow for as long as
/// the app runs.
const MAX_PENDING: usize = RATE as usize;

/// The way reference audio gets to the canceller. Cheap to clone; safe to push to from any thread.
#[derive(Clone, Default)]
pub struct RenderFeed(Arc<Mutex<VecDeque<f32>>>);

impl RenderFeed {
    /// Adds mono samples at [`RATE`]. Drops the oldest if the canceller is not keeping up.
    pub fn push(&self, samples: &[f32]) {
        let mut queue = self.lock();
        queue.extend(samples.iter().copied());
        let excess = queue.len().saturating_sub(MAX_PENDING);
        if excess > 0 {
            queue.drain(..excess);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<f32>> {
        // The guarded value is a plain queue with no invariant a panic could half-update.
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Turns captured speaker audio at any rate into reference audio at [`RATE`].
///
/// Lives here rather than with the capture code because it is plain arithmetic, and because the
/// canceller is only correct if the reference is at its rate: a loopback at 44.1 kHz fed in as if it
/// were 48 kHz would be a reference that drifts out of alignment by 8% and cancels nothing.
pub struct ReferenceInput {
    feed: RenderFeed,
    resampler: crate::resample::LinearResampler,
    rate: u32,
    out: Vec<f32>,
}

impl ReferenceInput {
    /// Wraps `feed`; the capture rate is learned from the first call to [`ReferenceInput::push`].
    pub fn new(feed: RenderFeed) -> Self {
        Self {
            feed,
            resampler: crate::resample::LinearResampler::new(RATE, RATE),
            rate: RATE,
            out: Vec::with_capacity(4096),
        }
    }

    /// Adds mono speaker audio captured at `rate`.
    pub fn push(&mut self, samples: &[f32], rate: u32) {
        if rate == RATE {
            self.feed.push(samples);
            return;
        }
        if rate != self.rate {
            self.resampler = crate::resample::LinearResampler::new(rate, RATE);
            self.rate = rate;
        }
        self.out.clear();
        self.resampler.process(samples, &mut self.out);
        self.feed.push(&self.out);
    }
}

/// What the canceller has measured about itself.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EchoStats {
    /// Echo return loss enhancement, in dB: how much quieter the echo got. `None` until there has
    /// been echo to measure.
    pub erle_db: Option<f64>,
    /// Net reduction since the last reading, in dB: the energy that went in over the energy that
    /// came out, after echo cancellation *and* suppression. Measured on the signal itself, so it
    /// cannot be flattered by the level stage after it. `None` when nothing has passed.
    pub net_db: Option<f64>,
    /// Estimated delay between reference and microphone, in ms, when known.
    pub delay_ms: Option<i32>,
}

/// Removes the reference signal from the microphone signal.
pub struct EchoCanceller {
    apm: AudioProcessing,
    feed: RenderFeed,
    src: [f32; FRAME],
    dest: [f32; FRAME],
    /// Which stages are on: (echo cancellation, noise suppression).
    stages: (bool, bool),
    /// Energy in and out since the last [`EchoCanceller::stats`] reading.
    energy_in: Cell<f64>,
    energy_out: Cell<f64>,
}

impl EchoCanceller {
    /// Builds a canceller and the feed that drives its reference input.
    ///
    /// `suppress_noise` adds WebRTC's noise suppression after the canceller. Off by default in the
    /// app when music is loud: measured on a real voice over music it gated whole words (gaps of
    /// 0.15 to 1.4 s inside speech), because it cannot tell a quiet voice from background sound. In a
    /// quiet room it is the difference between a phone microphone and a clean one.
    pub fn new(suppress_noise: bool) -> (Self, RenderFeed) {
        let config = Config {
            echo_canceller: Some(EchoConfig::default()),
            high_pass_filter: Some(HighPassFilter::default()),
            // After the canceller, on its output. What echo cancellation cannot remove — the tail of
            // the music, the part the filter has not caught up with — is exactly the steady,
            // low-level residue that suppression is good at hiding. Moderate rather than higher:
            // the harder it works, the more it chews at the user's own voice.
            noise_suppression: suppress_noise.then(NoiseSuppression::default),
            ..Default::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(RATE, 1))
            .render_config(StreamConfig::new(RATE, 1))
            .build();
        let feed = RenderFeed::default();
        (
            Self {
                apm,
                feed: feed.clone(),
                src: [0.0; FRAME],
                dest: [0.0; FRAME],
                stages: (true, suppress_noise),
                energy_in: Cell::new(0.0),
                energy_out: Cell::new(0.0),
            },
            feed,
        )
    }

    /// Turns the stages on or off while running. Cheap when nothing changed.
    ///
    /// Echo cancellation off with suppression on is a legitimate combination (a headset in a noisy
    /// room), so they are independent. With both off, [`EchoCanceller::process`] leaves the audio
    /// untouched.
    pub fn configure(&mut self, echo_cancel: bool, suppress_noise: bool) {
        if (echo_cancel, suppress_noise) == self.stages {
            return;
        }
        self.stages = (echo_cancel, suppress_noise);
        self.apm.apply_config(Config {
            echo_canceller: echo_cancel.then(EchoConfig::default),
            high_pass_filter: Some(HighPassFilter::default()),
            noise_suppression: suppress_noise.then(NoiseSuppression::default),
            ..Default::default()
        });
    }

    /// Cancels echo in `samples`, in place.
    ///
    /// Whole 10 ms blocks are processed; a trailing partial block is left as it arrived, which in
    /// practice never happens because the phone sends exactly one block per datagram.
    pub fn process(&mut self, samples: &mut [f32]) {
        self.feed_reference();
        if self.stages == (false, false) {
            return;
        }
        for block in samples.chunks_exact_mut(FRAME) {
            self.src.copy_from_slice(block);
            if self
                .apm
                .process_capture_f32(&[&self.src], &mut [&mut self.dest])
                .is_ok()
            {
                self.energy_in
                    .set(self.energy_in.get() + self.src.iter().map(|s| (*s as f64).powi(2)).sum::<f64>());
                self.energy_out
                    .set(self.energy_out.get() + self.dest.iter().map(|s| (*s as f64).powi(2)).sum::<f64>());
                block.copy_from_slice(&self.dest);
            }
        }
    }

    /// Hands every complete block of waiting reference audio to the canceller.
    ///
    /// Done before the microphone block, every time, because AEC3 needs the far end first: an echo
    /// cannot be predicted from audio the canceller has not seen yet.
    fn feed_reference(&mut self) {
        loop {
            {
                let mut queue = self.feed.lock();
                if queue.len() < FRAME {
                    return;
                }
                for slot in self.src.iter_mut() {
                    *slot = queue.pop_front().unwrap_or(0.0);
                }
            }
            // A render failure is a configuration error, not a runtime one; the capture path keeps
            // working without the reference, and the stats will show no enhancement.
            let _ = self
                .apm
                .process_render_f32(&[&self.src], &mut [&mut self.dest]);
        }
    }

    /// The canceller's own measurements.
    pub fn stats(&self) -> EchoStats {
        let stats = self.apm.statistics();
        let (e_in, e_out) = (self.energy_in.replace(0.0), self.energy_out.replace(0.0));
        let net_db = (e_in > 1e-9).then(|| 10.0 * (e_in / e_out.max(1e-12)).log10());
        EchoStats {
            net_db,
            erle_db: stats.echo_return_loss_enhancement,
            delay_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise in [-0.5, 0.5], so a failure reproduces.
    struct Noise(u32);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.0 >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        }
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    /// Music through a room and onto a phone is the reference, delayed and quieter. A few seconds
    /// of it should be mostly gone by the end.
    #[test]
    fn a_delayed_quieter_copy_of_the_reference_is_removed() {
        let (mut aec, feed) = EchoCanceller::new(false);
        let mut noise = Noise(7);
        // 60 ms of delay: Wi-Fi, the phone's capture buffer and a little air.
        let delay = 2_880;
        let total = RATE as usize * 6;
        let reference: Vec<f32> = (0..total + delay).map(|_| noise.next()).collect();

        let mut late = 0.0f32;
        let mut late_n = 0.0f32;
        for (i, block) in (0..total).step_by(FRAME).enumerate() {
            // The reference leads the microphone by `delay` samples.
            feed.push(&reference[block + delay..block + delay + FRAME]);
            let mut mic: Vec<f32> = (0..FRAME).map(|k| reference[block + k] * 0.4).collect();
            let before = rms(&mic);
            aec.process(&mut mic);
            if i >= 500 {
                late += rms(&mic) / before;
                late_n += 1.0;
            }
        }
        let late_ratio = late / late_n;
        assert!(
            late_ratio < 0.1,
            "after the filter converged the echo should be at least 20 dB down; remaining ratio {late_ratio}"
        );
    }

    /// A stand-in for a voice: a 140 Hz buzz with harmonics, switched on and off at syllable rate.
    ///
    /// Not white noise, deliberately. With suppression on, steady noise is exactly what gets
    /// removed, so noise cannot stand in for a voice any more.
    fn voice(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let syllable = (0.5 + 0.5 * (2.0 * std::f32::consts::PI * 3.0 * t).sin()).powi(2);
                let tone: f32 = (1..=8)
                    .map(|h| (2.0 * std::f32::consts::PI * 140.0 * h as f32 * t).sin() / h as f32)
                    .sum();
                0.3 * syllable * tone
            })
            .collect()
    }

    #[test]
    fn a_voice_with_no_reference_passes_through() {
        let (mut aec, _feed) = EchoCanceller::new(false);
        let signal = voice(RATE as usize * 4);
        let mut before = 0.0f32;
        let mut after = 0.0f32;
        for (i, block) in signal.chunks_exact(FRAME).enumerate() {
            let mut mic = block.to_vec();
            let b = rms(&mic);
            aec.process(&mut mic);
            if i >= 100 {
                before += b;
                after += rms(&mic);
            }
        }
        assert!(
            after / before > 0.5,
            "with nothing to cancel the voice must survive; kept {}",
            after / before
        );
    }

    #[test]
    fn the_feed_is_bounded() {
        let feed = RenderFeed::default();
        feed.push(&vec![0.0; MAX_PENDING * 3]);
        assert_eq!(feed.lock().len(), MAX_PENDING);
    }
}
