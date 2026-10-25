//! Lock-free counters shared between the network thread, the audio callback and
//! the UI. Atomics only: the audio callback must never block.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Counters for one streaming session.
#[derive(Debug, Default)]
pub struct Stats {
    /// Datagrams that arrived and parsed cleanly.
    pub datagrams: AtomicU64,
    /// Datagrams rejected, with the reason counted separately.
    pub parse_errors: AtomicU64,
    /// Mono frames accepted from the sender, at the sender's rate.
    pub samples_in: AtomicU64,
    /// Mono frames handed to the output queue, at the device rate.
    pub samples_out: AtomicU64,
    /// Frames we never received, inferred from gaps in `seq`.
    pub frames_lost: AtomicU64,
    /// Frames that arrived too late or twice (duplicate / out-of-order).
    pub frames_late: AtomicU64,
    /// Frames deliberately discarded to stop the queue growing without bound.
    /// In M0 this is also our crude clock-drift correction.
    pub slip_drops: AtomicU64,
    /// Frames deliberately discarded by the drift controller, to shed a burst back
    /// down to the target depth.
    ///
    /// Distinct from [`slip_drops`](Self::slip_drops) on purpose. This one is the
    /// controller *choosing* to shed, which is normal and bounded; `slip_drops` fires
    /// when the queue has already reached capacity, which means the controller lost.
    pub trim_drops: AtomicU64,
    /// Audio callbacks that ran short of samples and padded with silence.
    pub underruns: AtomicU64,
    /// Callbacks that found the queue locked and emitted silence to stay safe.
    pub callback_busy: AtomicU64,
    /// Samples delivered as silence because the queue was empty at stream start.
    pub prefill_silence: AtomicU64,
    /// Peak absolute sample of the most recent frame accepted from the sender, as
    /// `f32` bits.
    ///
    /// This one exists for the *user*, not for diagnosis. A level meter is how someone
    /// answers "can it hear me?" without reading a log, and until now the desktop side
    /// has had no feedback of any kind.
    level_in: AtomicU32,
    /// Peak absolute sample of the most recent frame handed to the output device.
    ///
    /// Separate from [`level_in`](Self::level_in) because they fail apart: audio can be
    /// arriving perfectly and still not be leaving, which looks identical from a single
    /// number.
    level_out: AtomicU32,
}

/// An immutable copy of [`Stats`], for display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// See [`Stats::datagrams`].
    pub datagrams: u64,
    /// See [`Stats::parse_errors`].
    pub parse_errors: u64,
    /// See [`Stats::samples_in`].
    pub samples_in: u64,
    /// See [`Stats::samples_out`].
    pub samples_out: u64,
    /// See [`Stats::frames_lost`].
    pub frames_lost: u64,
    /// See [`Stats::frames_late`].
    pub frames_late: u64,
    /// See [`Stats::slip_drops`].
    pub slip_drops: u64,
    /// See [`Stats::trim_drops`].
    pub trim_drops: u64,
    /// See [`Stats::underruns`].
    pub underruns: u64,
    /// See [`Stats::callback_busy`].
    pub callback_busy: u64,
    /// See [`Stats::prefill_silence`].
    pub prefill_silence: u64,
}

impl Stats {
    /// Creates a zeroed set of counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads every counter once.
    ///
    /// Counters are read with `Relaxed`: they are independent diagnostic
    /// tallies, and nothing downstream makes a decision that needs them to be
    /// mutually consistent.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            datagrams: self.datagrams.load(Ordering::Relaxed),
            parse_errors: self.parse_errors.load(Ordering::Relaxed),
            samples_in: self.samples_in.load(Ordering::Relaxed),
            samples_out: self.samples_out.load(Ordering::Relaxed),
            frames_lost: self.frames_lost.load(Ordering::Relaxed),
            frames_late: self.frames_late.load(Ordering::Relaxed),
            slip_drops: self.slip_drops.load(Ordering::Relaxed),
            trim_drops: self.trim_drops.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            callback_busy: self.callback_busy.load(Ordering::Relaxed),
            prefill_silence: self.prefill_silence.load(Ordering::Relaxed),
        }
    }

    /// Records the peak of the most recent inbound frame, 0.0 to 1.0.
    pub fn set_input_level(&self, level: f32) {
        self.level_in.store(level.to_bits(), Ordering::Relaxed);
    }

    /// The most recent inbound peak, 0.0 to 1.0.
    pub fn input_level(&self) -> f32 {
        f32::from_bits(self.level_in.load(Ordering::Relaxed))
    }

    /// Records the peak of the most recent frame handed to the output device.
    pub fn set_output_level(&self, level: f32) {
        self.level_out.store(level.to_bits(), Ordering::Relaxed);
    }

    /// The most recent outbound peak, 0.0 to 1.0.
    pub fn output_level(&self) -> f32 {
        f32::from_bits(self.level_out.load(Ordering::Relaxed))
    }
}

impl StatsSnapshot {
    /// Frames lost as a fraction of everything the sender tried to deliver.
    ///
    /// Returns 0.0 when nothing has been expected yet, so a fresh session shows
    /// 0% loss rather than NaN.
    pub fn loss_ratio(&self) -> f64 {
        let total = self.datagrams + self.frames_lost;
        if total == 0 {
            return 0.0;
        }
        self.frames_lost as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_stats_are_zero() {
        let s = Stats::new().snapshot();
        assert_eq!(s, StatsSnapshot::default());
        assert_eq!(s.loss_ratio(), 0.0, "loss must not be NaN before any audio");
    }

    #[test]
    fn counters_survive_a_round_trip() {
        let s = Stats::new();
        s.datagrams.fetch_add(5, Ordering::Relaxed);
        s.frames_lost.fetch_add(3, Ordering::Relaxed);
        s.underruns.fetch_add(1, Ordering::Relaxed);
        let snap = s.snapshot();
        assert_eq!(snap.datagrams, 5);
        assert_eq!(snap.frames_lost, 3);
        assert_eq!(snap.underruns, 1);
        assert!(snap.loss_ratio() > 0.0);
    }

    #[test]
    fn levels_round_trip_and_start_at_silence() {
        // Stored as `f32` bits rather than as a scaled integer, so this round-trips
        // exactly; a truncating store would break the meter's small end, which is
        // exactly where a user looks when they are told to "speak up".
        let s = Stats::new();
        assert_eq!(s.input_level(), 0.0, "a fresh Stats must read as silence");
        assert_eq!(s.output_level(), 0.0);

        s.set_input_level(0.0042);
        s.set_output_level(0.87);
        assert_eq!(s.input_level(), 0.0042);
        assert_eq!(s.output_level(), 0.87);

        // The two are independent, because "arriving but not leaving" is a real
        // failure and looks like a single number if they are not.
        s.set_input_level(1.4);
        assert_eq!(s.input_level(), 1.4, "levels are not clamped here");
        assert_eq!(s.output_level(), 0.87);
    }
}
