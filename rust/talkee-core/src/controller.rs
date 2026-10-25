//! Holding the output queue at the depth we asked for.
//!
//! # The problem this solves
//!
//! M0's answer to two devices running at slightly different rates was to let the
//! queue fill up and then throw away the oldest samples when it overflowed. That
//! bounds latency, but it does not *control* it. Measured against a real phone, a
//! burst of early frames pushed a 60 ms target to **130 ms**, and nothing ever
//! brought it back down: the depth wandered between 20 ms and 150 ms for the rest
//! of the run, and the 20 ms dip came within one callback of starving the audio
//! thread. That is ~70 ms of latency the user pays, permanently, plus a standing
//! risk of dropouts.
//!
//! The fix is the standard one: make the **clock** the actuator. If the queue is too
//! full, resample the incoming audio very slightly faster, so the output device
//! drains it faster than the sender fills it. If the queue is too empty, resample
//! slightly slower. Absorbing a 300 ppm clock difference means stretching audio by
//! 0.03 %, which is a pitch change of about a hundredth of a semitone — inaudible,
//! and it is not even a permanent shift, it is exactly the offset needed to make the
//! two clocks agree.
//!
//! # Why the ratio alone is not enough
//!
//! The ratio is a *slow* actuator, by design: it can only move the depth at the rate
//! the correction allows. Draining 70 ms of excess at 1 % takes seven seconds of
//! real time, and stretching that far for that long is the audible limit. So a
//! burst — which is not a clock difference but a late arrival — is handled by
//! shedding whole frames down to the target, and the ratio is left to hold what
//! is left. Two mechanisms, two different failure modes, two different counters
//! (`slip` for "we ran out of room", `trim` for "we chose to shed").
//!
//! # Why proportional *and* integral
//!
//! Proportional action alone settles at a non-zero error: generating a standing
//! correction requires a standing error, so the queue would rest at the wrong depth
//! forever. The integral term supplies a persistent correction for a persistent
//! rate difference, which is exactly what a clock mismatch is. It is clamped,
//! because without anti-windup a long burst would drive it to the rail and it would
//! take as long to come back.
//!
//! # Why this is testable without a clock
//!
//! [`DepthController::update`] takes the elapsed time as an argument instead of
//! reading a clock. The entire control loop — producer, resampler, queue, device —
//! can therefore be simulated in a unit test against a synthetic mismatch, in
//! microseconds of real time, with the convergence asserted exactly. See the tests
//! at the bottom of this file.

use std::time::Duration;

/// How often the caller feeds the controller a depth measurement.
///
/// The audio callback only reveals the depth when it runs, quantised to one
/// callback, so sampling faster than this buys nothing but noise. It must stay
/// several times faster than the loop's own response time or the controller is
/// steering with a stale reading.
pub const UPDATE_INTERVAL_MS: u32 = 200;

/// Largest correction the loop will apply, as a fraction.
///
/// 1 % is the same order as the largest plausible crystal mismatch. Held
/// indefinitely it is about 17 cents of pitch; held for the few seconds it takes to
/// drain a head of steam it is inaudible. There is no reason for it to be held:
/// the integral removes the error that called for it.
pub const MAX_CORRECTION: f64 = 0.01;

/// Error below this is not worth acting on, in milliseconds.
///
/// One callback of depth is ~10 ms at this frame size, so a smaller threshold than
/// this would spend the whole time chasing quantisation.
const DEADBAND_MS: f64 = 1.0;

/// Time constant of the depth filter, in seconds.
///
/// The depth is only knowable in whole frames: the producer pushes 480 frames at a
/// time and the callback takes 480 at a time, so an instantaneous reading is a
/// sawtooth of one frame — **10 ms** — whose phase slowly beats against the
/// controller's own sampling. Feeding those steps straight to the proportional term
/// made the ratio jump by thousands of ppm, which is a warble you can hear on a
/// steady tone, and it excited a limit cycle that never decayed. Smoothing the
/// *measurement* is what keeps the loop quiet.
///
/// This is the single most important number in the file. See the module docs for
/// what it costs.
const FILTER_TAU_SECS: f64 = 2.0;

/// Proportional gain: correction per millisecond of error.
///
/// Bounded by the measurement, not by taste: one frame of unseen error is 10 ms, so
/// a gain above ~1e-4 would swing the ratio by more than 1000 ppm on quantisation
/// alone.
const KP: f64 = 1.0e-4;

/// Integral gain: correction per millisecond of error, per second.
///
/// Slow on purpose. Together with `KP` it puts the loop's natural period at ~100 s,
/// which is a fair price: a clock difference is a slow phenomenon, and the fast
/// transients are handled by shedding rather than by steering.
const KI: f64 = 4.0e-6;

/// How far above the target the depth may drift before frames are shed.
///
/// Set wider than the loop's own overshoot on purpose. The margin is for *bursts* —
/// a late arrival that is not a clock difference and cannot be waited out — and it
/// must not fire on the slow overshoot the drift loop itself produces, or shedding
/// would become the primary mechanism instead of the exception. Measured overshoot
/// on a 3000 ppm mismatch is ~30 ms, and a real Wi-Fi burst is 70 ms or more.
const HIGH_WATER_MARGIN_MS: u32 = 50;

/// What the controller wants done about the current depth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Correction {
    /// Multiplier for the resampler's base ratio. Exactly 1.0 means "leave it".
    pub ratio_scale: f64,
    /// Oldest frames to discard, to bring the depth back inside the band.
    pub drop_oldest: usize,
}

impl Correction {
    /// The correction to apply when there is nothing to correct.
    pub const NONE: Self = Self {
        ratio_scale: 1.0,
        drop_oldest: 0,
    };

    /// True when this asks for no change at all, so the caller can skip the work.
    pub fn is_none(&self) -> bool {
        self.drop_oldest == 0 && self.ratio_scale == 1.0
    }
}

impl Default for Correction {
    fn default() -> Self {
        Self::NONE
    }
}

/// A PI controller that holds the output queue at a target depth by trimming the
/// resampler's ratio.
#[derive(Debug)]
pub struct DepthController {
    target_frames: usize,
    device_rate: u32,
    /// Smoothed depth, in frames. `None` until the first measurement, so the loop
    /// does not spend its first second crawling up from zero.
    filtered: Option<f64>,
    /// Accumulated error, in millisecond-seconds.
    integral: f64,
    /// The correction this controller is currently asking for.
    correction: f64,
}

impl DepthController {
    /// Builds a controller that aims to hold `target_frames` at `device_rate`.
    ///
    /// A target of zero is treated as one frame. Callers that want *no* control
    /// should not build a controller at all.
    pub fn new(target_frames: usize, device_rate: u32) -> Self {
        let mut controller = Self {
            target_frames: 1,
            device_rate: device_rate.max(1),
            filtered: None,
            integral: 0.0,
            correction: 0.0,
        };
        controller.set_target(target_frames);
        controller
    }

    /// Frames the controller is aiming to hold.
    pub fn target_frames(&self) -> usize {
        self.target_frames
    }

    /// Changes the target depth.
    ///
    /// Deliberately does not reset the loop: the correction already accumulated is
    /// still the right answer for the clock mismatch, and throwing it away would
    /// make the queue lurch every time the setting changed.
    pub fn set_target(&mut self, frames: usize) {
        self.target_frames = frames.max(1);
    }

    /// The correction currently applied, as a fraction of 1.0.
    ///
    /// This is the number worth watching: it converges to whatever the two clocks
    /// actually disagree by.
    pub fn correction(&self) -> f64 {
        self.correction
    }

    /// Depth above which the controller sheds frames rather than waiting.
    pub fn high_water_frames(&self) -> usize {
        self.target_frames + self.frames_for_ms(HIGH_WATER_MARGIN_MS)
    }

    fn frames_for_ms(&self, ms: u32) -> usize {
        ((self.device_rate as u64 * ms as u64) / 1000).max(1) as usize
    }

    /// Feeds one depth measurement covering `elapsed`, and returns what to do.
    ///
    /// `depth_frames` is the queue depth as the audio callback last saw it, in
    /// frames at `device_rate`. Returns [`Correction::NONE`] for a non-positive or
    /// non-finite interval, so a clock that jumps backwards cannot drive the loop.
    pub fn update(&mut self, depth_frames: usize, elapsed: Duration) -> Correction {
        let dt = elapsed.as_secs_f64();
        if !dt.is_finite() || dt <= 0.0 {
            return Correction::NONE;
        }

        let target = self.target_frames as f64;
        let depth = depth_frames as f64;
        let rate = self.device_rate.max(1) as f64;

        // Shed before measuring. A burst is a late arrival, not a clock difference,
        // and no ratio short of the pitch-shift ceiling undoes 70 ms of it in
        // reasonable time.
        let drop_oldest = if depth_frames > self.high_water_frames() {
            depth_frames.saturating_sub(self.target_frames)
        } else {
            0
        };
        let shedding = drop_oldest > 0;

        if shedding {
            // Re-seed the filter at the depth we are about to enforce, and leave the
            // integral alone. The integral holds the *clock* correction, which a
            // burst says nothing about; feeding it the burst error would leave a
            // standing correction behind long after the burst is gone.
            self.filtered = Some(target);
        } else {
            // Seed the filter with the first real reading. Starting from zero would
            // tell the loop the queue is empty and make it ask for a large, wrong
            // correction.
            let filtered = match self.filtered {
                Some(previous) => {
                    let alpha = 1.0 - (-dt / FILTER_TAU_SECS).exp();
                    previous + alpha * (depth - previous)
                }
                None => depth,
            };
            self.filtered = Some(filtered);

            // The error is expressed as a *deficit* — how far the queue is below the
            // target — because that is the direction that needs a higher ratio. A
            // shallow queue is refilled by producing more output frames than the
            // input nominally holds, which plays the audio fractionally slow; a full
            // queue is drained by producing fewer, which plays it fractionally fast.
            // Getting this backwards is positive feedback, so it is spelled out.
            let deficit_ms = (target - filtered) * 1000.0 / rate;
            if deficit_ms.abs() > DEADBAND_MS {
                self.integral += deficit_ms * dt;
            }

            // Anti-windup: hold the integral to what the actuator can actually use,
            // so it stops growing while saturated and can unwind promptly after.
            let limit = MAX_CORRECTION / KI;
            self.integral = self.integral.clamp(-limit, limit);
        }

        let deficit_ms = (target - self.filtered.unwrap_or(target)) * 1000.0 / rate;
        self.correction =
            (KP * deficit_ms + KI * self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);

        Correction {
            ratio_scale: 1.0 + self.correction,
            drop_oldest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::SharedQueue;
    use crate::resample::LinearResampler;
    use crate::stats::Stats;
    use std::sync::Arc;

    const RATE: u32 = 48_000;
    /// 10 ms frames, the cadence the phone sends.
    const FRAME: u32 = 480;
    const TARGET_MS: u32 = 60;
    const UPDATE: f64 = UPDATE_INTERVAL_MS as f64 / 1000.0;

    fn target_frames() -> usize {
        (RATE as u64 / 1000 * TARGET_MS as u64) as usize
    }

    fn ms(frames: usize) -> f64 {
        frames as f64 * 1000.0 / RATE as f64
    }

    /// What the simulated loop did.
    ///
    /// # About the tolerances the tests use
    ///
    /// The depth is only knowable in whole frames: the producer pushes 480 at a time
    /// and the callback takes 480 at a time, so an instantaneous reading is a
    /// sawtooth whose amplitude is **one frame — 10 ms**. The phase of that sawtooth
    /// beats slowly against the mismatch, which is why a simulated depth trace steps
    /// by ~10 ms every few seconds even when the loop is behaving. No assertion here
    /// can be tighter than that, and pretending otherwise would produce a test that
    /// passes only on the phase it was written against.
    #[derive(Debug)]
    struct Sim {
        /// Depth at the end.
        final_ms: f64,
        /// Extremes over the second half of the run, so the startup transient is not
        /// counted as a failure to hold the depth.
        settled_min_ms: f64,
        settled_max_ms: f64,
        /// Correction the controller had arrived at.
        final_correction: f64,
        /// Largest change in the applied ratio between two consecutive updates: the
        /// measure of whether the correction would be audible as a warble.
        max_step: f64,
        slip: u64,
        trim: u64,
        underruns: u64,
        /// Updates that had to shed frames.
        shed_updates: u32,
        /// One entry per controller update: `(seconds, depth_ms, correction_ppm,
        /// frames_pushed, frames_consumed)`.
        ///
        /// Kept so a failing test can show *how* the loop misbehaved rather than only
        /// that it did.
        trace: Vec<(f64, f64, f64, u64, u64)>,
    }

    impl Sim {
        fn settled_band_ms(&self) -> f64 {
            self.settled_max_ms - self.settled_min_ms
        }
    }

    /// Runs producer -> resampler -> queue -> device -> controller for `seconds`.
    ///
    /// `mismatch` is the sender's rate error as a fraction: `+0.003` means it
    /// delivers 0.3 % more frames per second than the output device consumes, which
    /// is what a real crystal difference looks like. `burst_ms` is injected into the
    /// queue before the loop starts, standing in for a Wi-Fi retransmission.
    ///
    /// This is the whole M1 drift story in one function: if the loop converges here
    /// it converges on a phone, and if it does not, nothing downstream can rescue it.
    fn simulate(mismatch: f64, burst_ms: u32, seconds: f64) -> Sim {
        let stats = Arc::new(Stats::new());
        let target = target_frames();
        let queue = Arc::new(SharedQueue::new(target * 4, Arc::clone(&stats)));
        queue.set_prefill(target);
        let mut controller = DepthController::new(target, RATE);
        // Nominal rates agree, so the base ratio is 1.0 and any ratio work is the
        // controller's doing.
        let mut resampler = LinearResampler::new(RATE, RATE);

        // A constant signal, so no rate error can be blamed on the data itself.
        let input = vec![0.25f32; FRAME as usize];
        let mut resampled = Vec::with_capacity(FRAME as usize * 2);
        let mut device_out = vec![0.0f32; FRAME as usize];

        let burst = (RATE as u64 / 1000 * burst_ms as u64) as usize;
        if burst > 0 {
            queue.push(&vec![0.25f32; burst]);
        }

        let dt = 0.001; // 1 ms per step
        let frame_secs = FRAME as f64 / RATE as f64;
        // The sender's clock runs `1 + mismatch` times faster than ours, so its frame
        // period is correspondingly shorter.
        let sender_frame_secs = frame_secs / (1.0 + mismatch);

        let mut now = 0.0f64;
        let mut sent = 0.0f64;
        let mut consumed = 0.0f64;
        let mut next_update = UPDATE;
        let mut applied = 1.0f64;
        let mut max_step = 0.0f64;
        let mut shed_updates = 0u32;
        let mut trace = Vec::new();
        let mut pushed = 0u64;
        let mut consumed_frames = 0u64;
        let mut last_pushed = 0u64;
        let mut last_consumed = 0u64;
        let mut settled_min = f64::MAX;
        let mut settled_max = 0.0f64;

        while now < seconds {
            now += dt;

            sent += dt / sender_frame_secs;
            while sent >= 1.0 {
                sent -= 1.0;
                resampled.clear();
                resampler.process(&input, &mut resampled);
                pushed += resampled.len() as u64;
                queue.push(&resampled);
            }

            consumed += dt / frame_secs;
            while consumed >= 1.0 {
                consumed -= 1.0;
                let depth = queue.len();
                let outcome = queue.fill(&mut device_out);
                consumed_frames += outcome.provided as u64;
                if now >= seconds / 2.0 {
                    let now_ms = ms(depth);
                    settled_min = settled_min.min(now_ms);
                    settled_max = settled_max.max(now_ms);
                }
            }

            if now >= next_update {
                next_update += UPDATE;
                let depth = queue.len();
                let correction = controller.update(depth, Duration::from_secs_f64(UPDATE));
                trace.push((
                    now,
                    ms(depth),
                    controller.correction() * 1e6,
                    pushed - last_pushed,
                    consumed_frames - last_consumed,
                ));
                last_pushed = pushed;
                last_consumed = consumed_frames;
                if correction.drop_oldest > 0 {
                    queue.drop_oldest(correction.drop_oldest);
                    shed_updates += 1;
                }
                max_step = max_step.max((correction.ratio_scale - applied).abs());
                applied = correction.ratio_scale;
                resampler.set_ratio(applied);
            }
        }

        let snapshot = stats.snapshot();
        Sim {
            final_ms: ms(queue.len()),
            settled_min_ms: settled_min,
            settled_max_ms: settled_max,
            final_correction: controller.correction(),
            max_step,
            slip: snapshot.slip_drops,
            trim: snapshot.trim_drops,
            underruns: snapshot.underruns,
            shed_updates,
            trace,
        }
    }

    #[test]
    fn a_fast_sender_is_absorbed_by_the_ratio_alone() {
        // 3000 ppm fast — a phone crystal, not a burst. The loop must find -3000 ppm
        // and hold the depth with nothing shed and nothing lost.
        let sim = simulate(0.003, 0, 60.0);
        assert!(
            (sim.final_correction + 0.003).abs() < 5e-4,
            "expected the loop to settle near -3000 ppm, got {:+.0} ppm: {sim:?}",
            sim.final_correction * 1e6
        );
        assert!(
            sim.settled_band_ms() < 35.0,
            "depth must be held steady, band was {:.1} ms: {sim:?}",
            sim.settled_band_ms()
        );
        assert!(
            (sim.final_ms - TARGET_MS as f64).abs() < 20.0,
            "expected to end near the 60 ms target, got {:.1} ms: {sim:?}",
            sim.final_ms
        );
        assert_eq!(sim.shed_updates, 0, "a clock difference is not a burst: {sim:?}");
        assert_eq!(sim.slip, 0, "{sim:?}");
        assert_eq!(sim.underruns, 0, "{sim:?}");
    }

    #[test]
    fn a_slow_sender_is_absorbed_by_the_ratio_alone() {
        // The other sign, which is where an inverted loop shows up as a queue that
        // empties and then clicks instead of one that overflows.
        let sim = simulate(-0.003, 0, 60.0);
        assert!(
            (sim.final_correction - 0.003).abs() < 5e-4,
            "expected the loop to settle near +3000 ppm, got {:+.0} ppm: {sim:?}",
            sim.final_correction * 1e6
        );
        assert!(sim.settled_band_ms() < 35.0, "{sim:?}");
        assert!((sim.final_ms - TARGET_MS as f64).abs() < 20.0, "{sim:?}");
        assert_eq!(sim.underruns, 0, "the queue must not be allowed to run dry: {sim:?}");
        assert_eq!(sim.slip, 0, "{sim:?}");
    }

    #[test]
    fn a_matched_clock_needs_no_correction() {
        let sim = simulate(0.0, 0, 30.0);
        assert!(
            sim.final_correction.abs() < 1e-3,
            "nothing to correct, so the loop must not invent a correction: {sim:?}"
        );
        assert!(sim.settled_band_ms() < 20.0, "{sim:?}");
        assert_eq!(sim.trim, 0, "{sim:?}");
        assert_eq!(sim.slip, 0, "{sim:?}");
    }

    #[test]
    fn a_burst_is_shed_and_the_depth_comes_back() {
        // 150 ms of audio arrives while the queue holds 60 ms. Nothing about that is
        // a rate difference, so the loop must shed it rather than stretch it, and
        // must not leave a standing correction behind as a souvenir.
        let sim = simulate(0.0, 150, 30.0);
        assert!(sim.shed_updates >= 1, "a 150 ms burst must be shed: {sim:?}");
        assert!(sim.trim > 0, "the shed frames must be counted: {sim:?}");
        assert_eq!(sim.slip, 0, "the burst must be shed before the queue fills: {sim:?}");
        assert!(
            (sim.final_ms - TARGET_MS as f64).abs() < 15.0,
            "depth must return to target, got {:.1} ms: {sim:?}",
            sim.final_ms
        );
        assert!(
            sim.final_correction.abs() < 5e-4,
            "a one-off burst leaves no standing clock correction: {sim:?}"
        );
    }

    #[test]
    fn an_unabsorbable_mismatch_is_bounded_rather_than_fatal() {
        // 5 % is past anything a crystal does and past what the ratio may correct, so
        // the queue *must* grow. What matters is that it stays bounded: the ratio works
        // against the excess, the surplus is shed, and the queue never reaches the
        // capacity where `slip` — the uncontrolled failure — becomes the mechanism.
        //
        // The loop deliberately does not walk its integral to the clamp here. Repeated
        // shedding suppresses the integrator on purpose, because shedding means "a
        // burst", and a burst says nothing about the clock. A mismatch this large is
        // not a clock difference; holding latency by shedding is the honest response.
        let sim = simulate(0.05, 0, 30.0);
        assert!(sim.shed_updates > 0, "what the ratio cannot absorb must be shed: {sim:?}");
        assert_eq!(sim.slip, 0, "the queue must never reach capacity: {sim:?}");
        assert!(
            sim.settled_max_ms < TARGET_MS as f64 + 80.0,
            "latency must stay bounded near the target, peaked at {:.1} ms: {sim:?}",
            sim.settled_max_ms
        );
        assert!(
            sim.final_correction <= -1e-3,
            "the ratio must be working against the excess, got {:+.0} ppm: {sim:?}",
            sim.final_correction * 1e6
        );
    }

    #[test]
    fn the_loop_settles_in_tens_of_seconds_not_minutes() {
        // A drift loop that converges *eventually* is no use to anyone, but it cannot be
        // fast either: the depth reading is quantised to one frame (10 ms), so a loop
        // quick enough to settle in two seconds is a loop that chases its own
        // measurement noise. The gains are set for a natural period of ~100 s, and this
        // test pins the shape of that: most of the clock difference found within 30 s,
        // all of it within 60.
        let sim = simulate(0.003, 0, 60.0);
        let reading_at = |at: f64| {
            *sim.trace
                .iter()
                .find(|(t, ..)| *t >= at)
                .unwrap_or_else(|| panic!("the trace does not cover {at} s"))
        };

        let at_30 = reading_at(30.0);
        assert!(
            (at_30.2 / 1e6 + 0.003).abs() < 1.2e-3,
            "by 30 s the loop should have found most of a 3000 ppm difference, \
             it was at {:+.0} ppm: {sim:?}",
            at_30.2
        );
        assert!(
            (at_30.1 - TARGET_MS as f64).abs() < 35.0,
            "at 30 s the depth was {:.1} ms: {sim:?}",
            at_30.1
        );

        let at_60 = reading_at(60.0);
        assert!(
            (at_60.2 / 1e6 + 0.003).abs() < 5e-4,
            "by 60 s the loop should have found the whole difference, \
             it was at {:+.0} ppm: {sim:?}",
            at_60.2
        );
    }

    #[test]
    fn the_correction_moves_smoothly_enough_not_to_warble() {
        // A ratio that jumps between updates is a pitch warble, audible on a steady
        // tone even when the average correction is tiny. The depth filter is what
        // keeps this small.
        let sim = simulate(0.001, 0, 30.0);
        assert!(
            sim.max_step < 2e-3,
            "ratio moved {:.0} ppm in one {UPDATE_INTERVAL_MS} ms update: {sim:?}",
            sim.max_step * 1e6
        );
    }

    #[test]
    fn shedding_does_not_wind_up_the_integral() {
        // The bug this guards: a burst is fed to the integrator, which then holds a
        // correction for a clock difference that does not exist, and the depth dips
        // for tens of seconds after the burst is over.
        let mut controller = DepthController::new(2_880, RATE);
        let elapsed = Duration::from_secs_f64(UPDATE);
        for _ in 0..10 {
            let c = controller.update(11_520, elapsed); // 240 ms deep
            assert!(c.drop_oldest > 0);
        }
        // Back at target: the loop must be at rest.
        let c = controller.update(2_880, elapsed);
        assert_eq!(c.drop_oldest, 0);
        assert!(
            c.ratio_scale == 1.0,
            "a burst left a standing correction of {:+.0} ppm",
            (c.ratio_scale - 1.0) * 1e6
        );
    }

    #[test]
    fn the_first_measurement_does_not_ask_for_a_wild_correction() {
        // Seeding the depth filter from zero instead of from the first reading makes
        // the loop believe the queue is empty and demand a large correction.
        let mut controller = DepthController::new(2_880, RATE);
        let c = controller.update(2_880, Duration::from_secs_f64(UPDATE));
        assert_eq!(c.ratio_scale, 1.0, "an on-target queue needs no correction");
        assert_eq!(c.drop_oldest, 0);
    }

    #[test]
    fn a_backwards_or_zero_interval_is_ignored() {
        // `Duration` cannot be negative or non-finite, so zero is the only way a
        // caller can express "no time passed" — and it must not be treated as a
        // measurement, because a rate needs an interval behind it.
        let mut controller = DepthController::new(2_880, RATE);
        assert_eq!(
            controller.update(9_999, Duration::ZERO),
            Correction::NONE
        );
        assert_eq!(controller.correction(), 0.0, "no interval, no correction");
    }

    #[test]
    fn a_measurement_inside_the_deadband_is_not_integrated() {
        // One frame is 10 ms, so a sub-millisecond error is quantisation. Integrating
        // it would accumulate drift of the loop's own making. The proportional term
        // may still act on it; the *integral* must not move.
        let mut controller = DepthController::new(2_880, RATE);
        let elapsed = Duration::from_secs_f64(UPDATE);
        let first = controller.update(2_904, elapsed).ratio_scale; // 0.5 ms shallow
        for _ in 0..200 {
            assert_eq!(
                controller.update(2_904, elapsed).ratio_scale,
                first,
                "the deadband leaked into the integral"
            );
        }
        assert!(
            (first - 1.0).abs() < 1e-3,
            "0.5 ms of error must not be worth {:.0} ppm",
            (first - 1.0) * 1e6
        );
    }

    #[test]
    fn the_high_water_mark_follows_the_target() {
        let mut controller = DepthController::new(2_880, RATE);
        assert_eq!(controller.high_water_frames(), 2_880 + 2_400); // target + 50 ms
        controller.set_target(4_800);
        assert_eq!(controller.target_frames(), 4_800);
        assert_eq!(controller.high_water_frames(), 4_800 + 2_400);
    }

    #[test]
    fn a_target_of_zero_is_treated_as_one_frame() {
        // Zero is reachable from a parsed `--buffer-ms 0`. It must not produce a
        // controller that cannot be satisfied.
        let controller = DepthController::new(0, RATE);
        assert_eq!(controller.target_frames(), 1);
        assert!(controller.high_water_frames() > 1);
    }

    #[test]
    fn the_correction_never_exceeds_its_clamp() {
        let mut controller = DepthController::new(2_880, RATE);
        let elapsed = Duration::from_secs_f64(UPDATE);
        for _ in 0..1_000 {
            let c = controller.update(0, elapsed); // permanently starving
            assert!(c.ratio_scale <= 1.0 + MAX_CORRECTION);
            assert!(c.ratio_scale >= 1.0 - MAX_CORRECTION);
        }
    }
}
