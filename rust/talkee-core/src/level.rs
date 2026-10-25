//! Lifting a quiet sender to a usable level.
//!
//! # Why this exists
//!
//! Measured with the real phone and the real microphone, over 318 frames of a person
//! speaking:
//!
//! ```text
//! median  0.0013   -57.7 dBFS
//! p90     0.0102   -39.8 dBFS
//! peak    0.0533   -25.5 dBFS     <- and nothing at all above -20
//! ```
//!
//! The capture path was not broken — the chain was calibrated first, and a tone sent at
//! amplitude 0.25 arrives as exactly 0.25. The signal is simply *quiet*: about 25 dB below
//! where a microphone feed normally sits, which is what `AudioSource.UNPROCESSED` means (it
//! bypasses the platform's automatic gain control, deliberately — that is what makes the
//! signal unprocessed). What made this worth acting on is the level meter: it answers "can it
//! hear me?", and a bar that sits at one cell out of twelve answers "no" to a working
//! microphone.
//!
//! The noise floor was −57.7 dBFS against a −25.5 dBFS speech peak, so the signal has about
//! **32 dB of signal-to-noise**. That is a respectable microphone, and it is why lifting it by
//! roughly the shortfall is safe rather than merely loud: the gain moves the whole signal, and
//! a gap that wide stays a gap.
//!
//! # What it will not do
//!
//! * **It never attenuates.** Gain starts at 1.0 and can only rise. A sender that is already
//!   at a good level is passed through untouched, because the failure mode of automatic gain —
//!   pumping, and flattening the dynamics of something that was already correct — is worse than
//!   the problem being solved here.
//! * **It never raises the gain on silence.** The gain only moves while frames are actually
//!   arriving; during a pause it is frozen at whatever it had. The alternative, letting the
//!   held peak leak away, means the gain creeps to its ceiling during a quiet minute and then
//!   blasts the next word. That is the classic way this feature ruins a recording.
//! * **It never lets a sample leave above 1.0.** The output is clamped, so the worst case is a
//!   clipped transient rather than a driver receiving a number it cannot represent.

use std::time::Duration;

/// The peak this aims for, in linear samples. −12 dBFS, which is where speech normally sits.
///
/// Deliberately not closer to full scale: a peak limiter that aims at 0.9 has nowhere to go when
/// someone raises their voice, and the consumer application on the other end has its own make-up
/// gain anyway.
pub const TARGET_PEAK: f32 = 0.25;

/// The most it will lift by. 32× is +30 dB, which covers the measured shortfall with room to
/// spare and still leaves the result recognisably the same signal.
pub const MAX_GAIN: f32 = 32.0;

/// Frames quieter than this do not count as signal, and do not move the gain.
///
/// −66 dBFS. Below it is either a muted microphone or a digital zero, and lifting those would
/// only amplify a noise floor.
const ACTIVE_FLOOR: f32 = 0.0005;

/// How fast the held peak forgets, per second of audio.
///
/// Only ever applied while signal is arriving, so a pause holds the gain rather than winding it
/// up. A tenth per second is slow enough that ordinary speech — which stops and starts
/// constantly — does not look like silence.
const PEAK_DECAY_PER_SEC: f32 = 0.9;

/// How fast the gain may move, per second: it may at most double or halve in one second.
///
/// A gain that jumps is a click, and a gain that moves faster than the peak it is chasing
/// oscillates.
const GAIN_SLEW_PER_SEC: f32 = 2.0;

/// Automatic gain that lifts a quiet sender and leaves anything else alone.
#[derive(Debug, Clone)]
pub struct AutoLevel {
    /// Current multiplier, never below 1.0.
    gain: f32,
    /// The loudest recent frame, which is what the gain is chosen from.
    held_peak: f32,
}

impl AutoLevel {
    /// Starts at unity gain, having heard nothing.
    pub fn new() -> Self {
        Self {
            gain: 1.0,
            held_peak: 0.0,
        }
    }

    /// The multiplier currently being applied. For the health line and the level meter.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Scales `frames` in place, having updated the gain from what it just heard.
    ///
    /// `elapsed` is the audio duration the frames represent, which is what makes the attack and
    /// release rates independent of the frame size.
    pub fn process(&mut self, frames: &mut [f32], elapsed: Duration) {
        if frames.is_empty() {
            return;
        }
        let seconds = elapsed.as_secs_f32();
        if !seconds.is_finite() || seconds <= 0.0 {
            return;
        }

        let frame_peak = frames
            .iter()
            .fold(0.0f32, |loudest, s| loudest.max(s.abs()));

        // Only signal moves the gain. Digital silence and a muted microphone both look like this,
        // and neither is a reason to turn the volume up.
        if frame_peak > ACTIVE_FLOOR {
            self.held_peak = frame_peak.max(self.held_peak * PEAK_DECAY_PER_SEC.powf(seconds));

            let desired = (TARGET_PEAK / self.held_peak).clamp(1.0, MAX_GAIN);
            // Slew toward it rather than jumping: at most a doubling or a halving per second.
            let step = GAIN_SLEW_PER_SEC.powf(seconds);
            self.gain = desired.clamp(self.gain / step, self.gain * step).clamp(1.0, MAX_GAIN);
        }

        if self.gain != 1.0 {
            for sample in frames.iter_mut() {
                *sample = (*sample * self.gain).clamp(-1.0, 1.0);
            }
        }
    }
}

impl Default for AutoLevel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `seconds` of a steady tone at `amplitude` and returns the loudest output sample.
    ///
    /// The buffer is rebuilt every iteration on purpose: `process` scales in place, so reusing one
    /// buffer would compound the gain and measure something that cannot happen.
    fn feed(level: &mut AutoLevel, amplitude: f32, seconds: f32) -> f32 {
        let steps = (seconds / 0.01) as usize;
        let mut loudest = 0.0f32;
        for _ in 0..steps {
            let mut frames = vec![amplitude; 480];
            level.process(&mut frames, Duration::from_millis(10));
            loudest = loudest.max(frames.iter().fold(0.0f32, |m, s| m.max(s.abs())));
        }
        loudest
    }

    #[test]
    fn a_quiet_sender_is_lifted_to_a_working_level() {
        // The measured case: speech peaking at 0.0533. Asked for 0.01 here so the gain has to
        // travel, which is the part that can go wrong.
        let mut level = AutoLevel::new();
        let out = feed(&mut level, 0.01, 8.0);
        assert!(
            (out - TARGET_PEAK).abs() < 0.01,
            "expected the output to land on the {TARGET_PEAK} target, got {out}"
        );
        assert!(
            (level.gain() - 25.0).abs() < 1.0,
            "0.01 needs 25x to reach 0.25; got {}",
            level.gain()
        );
    }

    #[test]
    fn a_sender_that_is_already_right_is_not_touched() {
        // The whole reason the gain can only rise. Anything at or above the target passes through
        // exactly as it arrived, with its dynamics intact.
        let mut level = AutoLevel::new();
        let out = feed(&mut level, TARGET_PEAK, 5.0);
        assert_eq!(level.gain(), 1.0, "a correct level must not be adjusted");
        assert_eq!(out, TARGET_PEAK, "and the samples must be untouched");
    }

    #[test]
    fn a_loud_sender_is_never_attenuated() {
        // The consumer application has its own make-up gain and its own limiter. Quietening a hot
        // signal here would only fight them.
        let mut level = AutoLevel::new();
        let out = feed(&mut level, 0.9, 5.0);
        assert_eq!(level.gain(), 1.0);
        assert!((out - 0.9).abs() < 1e-6, "got {out}");
    }

    #[test]
    fn silence_does_not_wind_the_gain_up() {
        // The classic way this feature ruins a recording: the gain creeps to its ceiling during a
        // quiet minute, and the first word afterwards is a shout. Nothing here moves on silence,
        // whether it is digital zero or the noise floor of a muted microphone.
        for quiet in [0.0, ACTIVE_FLOOR / 2.0] {
            let mut level = AutoLevel::new();
            let out = feed(&mut level, quiet, 30.0);
            assert_eq!(out, quiet, "silence must pass through untouched");
            assert_eq!(level.gain(), 1.0, "silence must not raise the gain");
        }
    }

    #[test]
    fn a_pause_holds_the_gain_it_had() {
        // Speech stops and starts constantly. A pause must not be read as a reason to lift harder.
        let mut level = AutoLevel::new();
        feed(&mut level, 0.01, 8.0);
        let settled = level.gain();
        feed(&mut level, 0.0, 3.0);
        assert_eq!(level.gain(), settled, "a pause changed the gain");
    }

    #[test]
    fn the_gain_stops_at_its_ceiling() {
        // A near-silent but technically active frame should not drive the gain to infinity, and the
        // ceiling is what keeps a hiss from becoming a roar.
        let mut level = AutoLevel::new();
        feed(&mut level, ACTIVE_FLOOR * 1.1, 15.0);
        assert_eq!(level.gain(), MAX_GAIN, "the ceiling should bind here");
    }

    #[test]
    fn a_sample_never_leaves_above_full_scale() {
        // A burst while the gain is high is the one case that can clip, and a driver handed a
        // number it cannot represent is worse than a clipped transient. This is what the clamp in
        // `process` is for.
        let mut level = AutoLevel::new();
        feed(&mut level, 0.01, 8.0); // gain climbs to ~25x
        let mut frames = vec![0.9f32; 480];
        level.process(&mut frames, Duration::from_millis(10));
        assert!(
            frames.iter().all(|s| s.abs() <= 1.0),
            "a burst at the top of the gain range must be clamped, not passed on"
        );
    }

    #[test]
    fn the_gain_cannot_jump() {
        // A gain that steps is a click. One 10 ms frame may move it by at most 2^(0.01), which is
        // about 0.7 %.
        let mut level = AutoLevel::new();
        let quiet = vec![0.001f32; 480];
        let mut frames = quiet.clone();
        level.process(&mut frames, Duration::from_millis(10));
        let after_one = level.gain();
        assert!(
            after_one <= 1.0 * 1.01,
            "the gain jumped to {after_one} in a single frame"
        );
    }

    #[test]
    fn an_empty_frame_changes_nothing() {
        let mut level = AutoLevel::new();
        level.process(&mut [], Duration::from_millis(10));
        assert_eq!(level.gain(), 1.0);
    }
}
