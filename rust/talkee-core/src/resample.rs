//! Sample-rate conversion.
//!
//! # Why this exists at all
//!
//! Two devices are never at exactly the same rate. A phone's 48 kHz oscillator
//! might really be 48 009 Hz, and the desktop's virtual device might really be
//! 47 995 Hz. The mismatch is a few hundred parts per million — which sounds
//! negligible until you do the arithmetic: 200 ppm over one hour is **0.72
//! seconds** of accumulated audio. Uncorrected, that shows up as a periodic
//! click then a growing delay into the hundreds of milliseconds.
//!
//! M0 does the crude thing: [`LinearResampler`] handles the *nominal* rate
//! difference (e.g. phone at 48 kHz, virtual cable at 44.1 kHz), while
//! [`crate::queue::SharedQueue`] absorbs residual drift by dropping the oldest
//! samples when it overflows. M1 replaces the crude part with a PI controller
//! driving `LinearResampler::set_ratio` — the resampler is already built to
//! accept that, which is why it is not hard-coded to a fixed ratio.
//!
//! Measuring *how far* the two rates actually are apart is a separate concern and
//! lives in [`crate::meter`]. It used to live here, and moving it out is part of
//! the same change that stopped the health line reporting a session average.

/// Streaming linear-interpolation resampler.
///
/// Not audiophile-grade: linear interpolation has a gentle high-frequency roll
/// off and some aliasing. It is the right M0 choice because it is stateless
/// across frames in the ways that matter, trivially predictable, and cheap
/// enough to run on the network thread. M1 swaps the interpolation kernel for
/// `rubato`'s band-limited sinc resampler without changing this API.
///
/// Works in chunks. State carries across chunks so that feeding 4800 samples as
/// ten chunks of 480 gives the same output as one call.
#[derive(Debug, Clone)]
pub struct LinearResampler {
    /// Output rate divided by input rate.
    ratio: f64,
    /// Position of the next output sample, in input samples, relative to the
    /// start of the chunk currently being processed. Because it can land
    /// fractionally *before* the first input sample of a chunk, its valid range
    /// is `(-1, step]` where `step = 1 / ratio`.
    phase: f64,
    /// Last input sample of the previous chunk, for interpolation across the
    /// chunk boundary.
    last: f32,
    /// Whether `last` holds a real sample yet.
    has_last: bool,
    /// True when the ratio is exactly 1.0, enabling a memcpy fast path.
    identity: bool,
}

impl LinearResampler {
    /// Creates a resampler going from `in_rate` to `out_rate` Hz.
    ///
    /// Zero rates are treated as 1 Hz rather than panicking, because these
    /// values come from a parsed network header.
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let in_rate = in_rate.max(1) as f64;
        let out_rate = out_rate.max(1) as f64;
        let ratio = out_rate / in_rate;
        Self {
            ratio,
            phase: 0.0,
            last: 0.0,
            has_last: false,
            identity: (ratio - 1.0).abs() < 1e-12,
        }
    }

    /// Current output/input ratio.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// True if this resampler is a passthrough.
    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// Adjusts the ratio while streaming.
    ///
    /// This is the hook M1's drift controller drives. Non-finite or
    /// non-positive values are ignored rather than poisoning the phase
    /// accumulator.
    pub fn set_ratio(&mut self, ratio: f64) {
        if !ratio.is_finite() || ratio <= 0.0 {
            return;
        }
        self.ratio = ratio;
        self.identity = (ratio - 1.0).abs() < 1e-12;
    }

    /// Forgets interpolation state. Call on reconnect so we do not interpolate
    /// across a discontinuity.
    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.last = 0.0;
        self.has_last = false;
    }

    /// Resamples `input`, appending to `out`.
    ///
    /// `out` is a `&mut Vec` rather than a pre-sized slice because the number of
    /// output frames is not known in advance at fractional ratios. The caller
    /// (network thread) is allowed to allocate.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let n = input.len();
        if self.identity {
            out.extend_from_slice(input);
            self.last = input[n - 1];
            self.has_last = true;
            return;
        }

        let step = 1.0 / self.ratio;
        let previous = if self.has_last { self.last } else { input[0] };
        let last_index = (n - 1) as f64;

        // Returns input[i], clamped, falling back to the carried sample for
        // i == -1. Clamping the upper end means a phase exactly at the final
        // sample interpolates to itself instead of reading out of bounds.
        let sample_at = |i: isize| -> f32 {
            if i < 0 {
                previous
            } else if (i as usize) < n {
                input[i as usize]
            } else {
                input[n - 1]
            }
        };

        let mut phase = self.phase;
        while phase <= last_index {
            let index = phase.floor();
            let frac = (phase - index) as f32;
            let index = index as isize;
            let a = sample_at(index);
            let b = sample_at(index + 1);
            out.push(a + (b - a) * frac);
            phase += step;
        }

        // Rebase the leftover phase onto the next chunk. `phase` left the loop
        // in `(last_index, last_index + step]`, so after subtracting `n` it is
        // in `(-1, step - 1]` — bounded, so it cannot run away over time.
        self.phase = phase - n as f64;
        self.last = input[n - 1];
        self.has_last = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32 / n as f32).collect()
    }

    #[test]
    fn matching_rates_are_a_passthrough() {
        let mut r = LinearResampler::new(48_000, 48_000);
        assert!(r.is_identity());
        let input = ramp(4800);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out, input, "an identity resampler must not touch samples");
    }

    #[test]
    fn upsampling_hits_the_right_average_rate() {
        let mut r = LinearResampler::new(48_000, 96_000);
        assert!((r.ratio() - 2.0).abs() < 1e-12);
        let small = ramp(480);
        let mut out = Vec::new();
        // The first chunk is short by one output frame, because the final output
        // sample needs an input sample that has not arrived yet. That is a
        // ramp-up artifact, not drift, so measure across many chunks.
        for _ in 0..101 {
            r.process(&small, &mut out);
        }
        let produced = out.len() as f64;
        assert!(
            (produced - 2.0 * 480.0 * 101.0).abs() < 4.0,
            "expected ~96960 output frames, got {produced}"
        );
    }

    #[test]
    fn downsampling_hits_the_right_average_rate() {
        let mut r = LinearResampler::new(96_000, 48_000);
        let small = ramp(480);
        let mut out = Vec::new();
        for _ in 0..101 {
            r.process(&small, &mut out);
        }
        let produced = out.len() as f64;
        assert!(
            (produced - 0.5 * 480.0 * 101.0).abs() < 4.0,
            "expected ~24240 output frames, got {produced}"
        );
    }

    #[test]
    fn dc_signal_survives_resampling_unchanged() {
        // Gain errors and off-by-one phase bugs show up here immediately.
        for (a, b) in [(48_000, 44_100), (44_100, 48_000), (48_000, 16_000)] {
            let mut r = LinearResampler::new(a, b);
            let input = vec![0.25f32; 4800];
            let mut out = Vec::new();
            r.process(&input, &mut out);
            assert!(!out.is_empty(), "{a} -> {b} produced nothing");
            let worst = out.iter().fold(0.0f32, |acc, &s| acc.max((s - 0.25).abs()));
            assert!(worst < 1e-6, "{a} -> {b} distorted DC by {worst}");
        }
    }

    #[test]
    fn chunked_and_whole_buffer_processing_agree() {
        // A linear ramp is reproduced exactly by linear interpolation, so any
        // difference here is a state-carrying bug across chunk boundaries.
        let input: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let mut whole = Vec::new();
        LinearResampler::new(48_000, 44_100).process(&input, &mut whole);

        let mut chunked = Vec::new();
        let mut r = LinearResampler::new(48_000, 44_100);
        for chunk in input.chunks(100) {
            r.process(chunk, &mut chunked);
        }

        assert!(
            whole.len().abs_diff(chunked.len()) <= 1,
            "lengths diverged: {} vs {}",
            whole.len(),
            chunked.len()
        );
        let n = whole.len().min(chunked.len());
        for i in 0..n {
            assert!(
                (whole[i] - chunked[i]).abs() < 1e-3,
                "sample {i} differs: {} vs {}",
                whole[i],
                chunked[i]
            );
        }
    }

    #[test]
    fn phase_stays_bounded_over_a_long_stream() {
        // Guards the invariant documented on `phase`: if rebasing were wrong it
        // would run away and the output would eventually stall or explode.
        let mut r = LinearResampler::new(48_000, 44_100);
        let input = vec![1.0f32; 480];
        let mut total = 0usize;
        for _ in 0..10_000 {
            let mut out = Vec::new();
            r.process(&input, &mut out);
            total += out.len();
            assert!(r.phase > -1.0 && r.phase <= 1.0 / r.ratio, "phase escaped: {}", r.phase);
        }
        let expected = 4_800_000.0 * 44_100.0 / 48_000.0;
        assert!(
            (total as f64 - expected).abs() < 100.0,
            "got {total} frames, expected ~{expected}"
        );
    }

    #[test]
    fn set_ratio_rejects_garbage() {
        let mut r = LinearResampler::new(48_000, 48_000);
        r.set_ratio(f64::NAN);
        assert!((r.ratio() - 1.0).abs() < 1e-12);
        r.set_ratio(0.0);
        assert!((r.ratio() - 1.0).abs() < 1e-12);
        r.set_ratio(-2.0);
        assert!((r.ratio() - 1.0).abs() < 1e-12);
        r.set_ratio(0.919);
        assert!((r.ratio() - 0.919).abs() < 1e-12);
        assert!(!r.is_identity());
    }

    #[test]
    fn empty_input_is_ignored() {
        let mut r = LinearResampler::new(48_000, 44_100);
        let mut out = Vec::new();
        r.process(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn reset_forgets_carried_state() {
        let mut r = LinearResampler::new(48_000, 44_100);
        let mut out = Vec::new();
        r.process(&[1.0; 480], &mut out);
        assert!(r.has_last);
        r.reset();
        assert!(!r.has_last);
        assert_eq!(r.phase, 0.0);
    }
}


