//! The handoff between the network thread and the audio callback.
//!
//! Two different worlds meet here:
//!
//! * **Producer** (network thread): may block, allocate and resample freely.
//! * **Consumer** (audio callback): must never block, allocate, or take a lock
//!   that might be held. A missed deadline is an audible click.
//!
//! So the consumer uses `try_lock` and emits silence if it loses the race. With
//! 10 ms frames and a producer that only holds the lock while memcpy-ing, the
//! race is rare — and `Stats::callback_busy` counts it, so "rare" is measured
//! rather than assumed.

use crate::stats::Stats;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

/// What one audio callback managed to get.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FillOutcome {
    /// Frames the callback asked for.
    pub requested: usize,
    /// Frames actually copied out of the queue.
    pub provided: usize,
    /// The callback could not take the lock and emitted silence instead.
    pub busy: bool,
}

impl FillOutcome {
    /// True if the callback had to pad with silence.
    pub fn underran(&self) -> bool {
        self.provided < self.requested
    }

    /// Buffer occupancy as a fraction in `[0, 1]`.
    pub fn fill_ratio(&self) -> f32 {
        if self.requested == 0 {
            return 0.0;
        }
        self.provided as f32 / self.requested as f32
    }
}

/// A bounded ring of mono `f32` frames at the **output device's** rate.
pub struct SharedQueue {
    inner: Mutex<VecDeque<f32>>,
    capacity: usize,
    /// Frames to accumulate before output starts. See [`SharedQueue::set_prefill`].
    prefill: AtomicUsize,
    /// Whether the prefill gate has opened.
    started: AtomicBool,
    stats: Arc<Stats>,
}

impl SharedQueue {
    /// Creates a queue holding at most `capacity` mono frames, with no prefill.
    ///
    /// `capacity` should be a few multiples of the largest buffer size the
    /// device will ask for; 200 ms-worth is a sane starting point.
    ///
    /// Prefill defaults to zero, meaning output starts immediately. That is the
    /// right default for a library — the caller decides how much latency to
    /// trade for stability — and it is what [`set_prefill`](Self::set_prefill)
    /// exists to change.
    pub fn new(capacity: usize, stats: Arc<Stats>) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity.min(1 << 20))),
            capacity: capacity.max(1),
            prefill: AtomicUsize::new(0),
            started: AtomicBool::new(false),
            stats,
        }
    }

    /// The shared counters.
    pub fn stats(&self) -> &Arc<Stats> {
        &self.stats
    }

    /// Target capacity in frames.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Sets how many frames must accumulate before the consumer is fed.
    ///
    /// # Why this is not optional
    ///
    /// A queue with no prefill and a producer that runs at the same average rate
    /// as the consumer sits permanently near empty. Anything that delays the
    /// producer for a moment — a scheduler preemption, a GC pause on the other
    /// side, a Wi-Fi retransmission — STARVES the callback, which then pads with
    /// silence. Measured on this machine, that was **65% of callbacks** padding
    /// with silence at 0 prefill: an audible mess that still reports "0% loss".
    ///
    /// Buffering `prefill` frames first converts those dropouts into a fixed,
    /// predictable delay. This is the latency-versus-reliability dial, and there
    /// is no setting of it that gives you both.
    pub fn set_prefill(&self, frames: usize) {
        self.prefill.store(frames.min(self.capacity), Ordering::Relaxed);
    }

    /// Frames required before output starts.
    pub fn prefill(&self) -> usize {
        self.prefill.load(Ordering::Relaxed)
    }

    /// True once the prefill gate has opened and real audio is flowing.
    pub fn is_running(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// Producer side: append frames, discarding the oldest if that would
    /// overflow.
    ///
    /// Discarding the *oldest* is deliberate. Keeping the newest audio and
    /// accepting a small discontinuity keeps latency bounded; keeping the
    /// oldest would let latency grow without limit, which is the failure mode
    /// that makes these apps unusable after ten minutes. In M0 this is also our
    /// entire clock-drift strategy — see `docs/LATENCY.md`.
    pub fn push(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let capacity = self.capacity;
        let dropped = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let existing = guard.len();
            if samples.len() >= capacity {
                // A single push that would overflow on its own. Keep the newest
                // `capacity` frames and discard everything already queued; the
                // alternative is a queue that silently grows past its bound.
                guard.clear();
                let skipped = samples.len() - capacity;
                guard.extend(samples[skipped..].iter().copied());
                existing + skipped
            } else {
                // Drop as many of the oldest queued frames as needed to make
                // room, but never more than are actually there.
                let droppable = (existing + samples.len())
                    .saturating_sub(capacity)
                    .min(existing);
                for _ in 0..droppable {
                    guard.pop_front();
                }
                guard.extend(samples.iter().copied());
                droppable
            }
        };
        self.stats.samples_out.fetch_add(samples.len() as u64, Ordering::Relaxed);
        if dropped > 0 {
            self.stats.slip_drops.fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }

    /// Consumer side: fill `out`, padding with silence if we run short.
    ///
    /// Never blocks. Called from the audio callback.
    pub fn fill(&self, out: &mut [f32]) -> FillOutcome {
        let requested = out.len();
        self.stats
            .callback_frames_max
            .fetch_max(requested as u64, Ordering::Relaxed);
        let mut guard = match self.inner.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                out.fill(0.0);
                self.stats.callback_busy.fetch_add(1, Ordering::Relaxed);
                return FillOutcome {
                    requested,
                    provided: 0,
                    busy: true,
                };
            }
        };
        let mut provided = 0usize;
        // Hold output silent until the prefill target is met. This silence is
        // deliberate, so it is counted separately and is NOT an underrun.
        if !self.started.load(Ordering::Relaxed) {
            if guard.len() < self.prefill.load(Ordering::Relaxed) {
                out.fill(0.0);
                drop(guard);
                self.stats
                    .prefill_silence
                    .fetch_add(requested as u64, Ordering::Relaxed);
                return FillOutcome {
                    requested,
                    provided: 0,
                    busy: false,
                };
            }
            self.started.store(true, Ordering::Relaxed);
        }
        for slot in out.iter_mut() {
            match guard.pop_front() {
                Some(sample) => {
                    *slot = sample;
                    provided += 1;
                }
                None => *slot = 0.0,
            }
        }
        drop(guard);
        if provided < requested {
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
        }
        FillOutcome {
            requested,
            provided,
            busy: false,
        }
    }

    /// Frames currently buffered.
    pub fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }

    /// True if nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Discards up to `frames` of the oldest buffered audio, returning how many
    /// actually went.
    ///
    /// This is the drift controller shedding a burst back down to its target depth.
    /// It is deliberately not the same code path as the overflow handling in
    /// [`push`](Self::push): that one keeps the queue inside its bound when the
    /// controller has already lost, and the two are counted separately so the health
    /// line can say which happened.
    pub fn drop_oldest(&self, frames: usize) -> usize {
        if frames == 0 {
            return 0;
        }
        let dropped = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let count = frames.min(guard.len());
            for _ in 0..count {
                guard.pop_front();
            }
            count
        };
        if dropped > 0 {
            self.stats.trim_drops.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        dropped
    }

    /// Discards everything buffered and re-arms the prefill gate.
    ///
    /// Called on resync. Re-arming means a restarted stream gets one brief,
    /// intentional silence rather than an immediate underrun.
    pub fn clear(&self) {
        match self.inner.lock() {
            Ok(mut g) => g.clear(),
            Err(p) => p.into_inner().clear(),
        }
        self.started.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(capacity: usize) -> (SharedQueue, Arc<Stats>) {
        let stats = Arc::new(Stats::new());
        (SharedQueue::new(capacity, Arc::clone(&stats)), stats)
    }

    #[test]
    fn trimming_drops_the_oldest_and_is_counted_apart_from_overflow() {
        // The controller sheds; the capacity guard spills. A health line that could
        // not tell those apart could not say whether the loop was working.
        let (q, stats) = queue(1000);
        q.push(&[1.0f32; 100]);
        q.push(&[2.0f32; 100]);

        assert_eq!(q.drop_oldest(60), 60);
        assert_eq!(q.len(), 140, "60 of the oldest must be gone");
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.trim_drops, 60);
        assert_eq!(snapshot.slip_drops, 0, "this was not an overflow");

        // The newest audio survives, in order.
        let mut out = [0.0f32; 140];
        q.fill(&mut out);
        assert_eq!(out[39], 1.0, "the tail of the first push is still there");
        assert_eq!(out[40], 2.0, "the second push follows it");
    }

    #[test]
    fn trimming_more_than_is_buffered_empties_the_queue_without_lying() {
        let (q, stats) = queue(1000);
        q.push(&[1.0f32; 100]);
        assert_eq!(q.drop_oldest(500), 100, "returns what it actually dropped");
        assert!(q.is_empty());
        assert_eq!(stats.snapshot().trim_drops, 100);
        // A no-op trim is not a counter increment.
        assert_eq!(q.drop_oldest(0), 0);
        assert_eq!(stats.snapshot().trim_drops, 100);
    }

    #[test]
    fn empty_queue_fills_with_silence_and_counts_underrun() {
        let (q, stats) = queue(1000);
        let mut out = [1.0f32; 64];
        let outcome = q.fill(&mut out);
        assert_eq!(outcome.provided, 0);
        assert!(outcome.underran());
        assert!(!outcome.busy);
        assert_eq!(out, [0.0f32; 64], "must be silence, not stale memory");
        assert_eq!(stats.snapshot().underruns, 1);
    }

    #[test]
    fn push_then_fill_reproduces_samples_in_order() {
        let (q, _stats) = queue(1000);
        let frames: Vec<f32> = (0..100).map(|i| i as f32).collect();
        q.push(&frames);
        let mut out = [0.0f32; 40];
        let outcome = q.fill(&mut out);
        assert_eq!(outcome.provided, 40);
        assert_eq!(outcome.fill_ratio(), 1.0);
        assert!((0..40).all(|i| out[i] == i as f32), "got {out:?}");
        assert_eq!(q.len(), 60);
    }

    #[test]
    fn partial_fill_pads_the_tail_with_silence() {
        let (q, _stats) = queue(1000);
        q.push(&[0.5; 10]);
        let mut out = [1.0f32; 20];
        let outcome = q.fill(&mut out);
        assert_eq!(outcome.provided, 10);
        assert!(outcome.underran());
        assert!(out[..10].iter().all(|&s| s == 0.5));
        assert!(out[10..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn overflowing_push_drops_the_oldest_and_counts_slip() {
        let (q, stats) = queue(100);
        q.push(&(0..80).map(|i| i as f32).collect::<Vec<_>>());
        q.push(&(80..180).map(|i| i as f32).collect::<Vec<_>>());
        // Capacity is 100 and we offered 180 total, so the first 80 are gone.
        assert_eq!(q.len(), 100);
        let mut out = [0.0f32; 4];
        q.fill(&mut out);
        assert_eq!(out, [80.0, 81.0, 82.0, 83.0]);
        assert_eq!(stats.snapshot().slip_drops, 80);
    }

    #[test]
    fn capacity_is_enforced_exactly() {
        let (q, _stats) = queue(10);
        q.push(&[7.0; 5000]);
        assert_eq!(q.len(), 10);
    }

    #[test]
    fn push_of_empty_slice_is_a_no_op() {
        let (q, stats) = queue(10);
        q.push(&[]);
        assert!(q.is_empty());
        assert_eq!(stats.snapshot().samples_out, 0);
    }

    #[test]
    fn clear_empties_the_queue() {
        let (q, _stats) = queue(100);
        q.push(&[1.0; 50]);
        assert_eq!(q.len(), 50);
        q.clear();
        assert!(q.is_empty());
    }

    #[test]
    fn a_single_oversized_push_keeps_the_newest_frames() {
        // The contract is "oldest data is the data we sacrifice", and it has to
        // hold even when one push alone overflows the queue.
        let (q, stats) = queue(10);
        let frames: Vec<f32> = (0..5000).map(|i| i as f32).collect();
        q.push(&frames);
        assert_eq!(q.len(), 10);

        let mut out = [0.0f32; 10];
        q.fill(&mut out);
        let expected: Vec<f32> = (4990..5000).map(|i| i as f32).collect();
        assert_eq!(out.to_vec(), expected);
        assert_eq!(stats.snapshot().slip_drops, 4990);
    }

    #[test]
    fn prefill_holds_output_silent_until_the_target_is_reached() {
        let (q, stats) = queue(1000);
        q.set_prefill(100);

        let mut out = [1.0f32; 32];
        q.push(&[0.5; 64]); // below the 100-frame target
        let outcome = q.fill(&mut out);
        assert_eq!(outcome.provided, 0);
        assert_eq!(out, [0.0f32; 32], "must be silent while prefilling");
        assert!(!q.is_running());
        let s = stats.snapshot();
        assert_eq!(s.underruns, 0, "prefill silence is intentional, not starvation");
        assert_eq!(s.prefill_silence, 32);

        // Crossing the target opens the gate and audio starts flowing.
        q.push(&[0.5; 40]);
        let outcome = q.fill(&mut out);
        assert_eq!(outcome.provided, 32);
        assert!(q.is_running());
        assert!(out.iter().all(|&s| s == 0.5), "got {out:?}");
    }

    #[test]
    fn clear_re_arms_the_prefill_gate() {
        let (q, _stats) = queue(1000);
        q.set_prefill(10);
        q.push(&[0.5; 100]);
        let mut out = [0.0f32; 4];
        q.fill(&mut out);
        assert!(q.is_running(), "the gate should have opened");

        q.clear();
        assert!(!q.is_running(), "clear must re-arm the gate for a restart");

        let mut after = [1.0f32; 4];
        q.push(&[0.5; 5]); // below prefill again
        q.fill(&mut after);
        assert_eq!(after, [0.0f32; 4], "output must wait for the new prefill");
    }

    #[test]
    fn set_prefill_is_clamped_to_capacity() {
        // A prefill above capacity could never be satisfied, which would mute
        // the stream forever. Clamp rather than hang.
        let (q, _stats) = queue(100);
        q.set_prefill(10_000);
        assert_eq!(q.prefill(), 100);
    }

    #[test]
    fn zero_prefill_passes_audio_straight_through() {
        // The default must stay "no buffering" so the library does not impose
        // latency on callers who have not asked for it.
        let (q, _stats) = queue(100);
        assert_eq!(q.prefill(), 0);
        q.push(&[0.25; 8]);
        let mut out = [0.0f32; 4];
        assert_eq!(q.fill(&mut out).provided, 4);
        assert!(out.iter().all(|&s| s == 0.25));
    }

    #[test]
    fn samples_out_counts_everything_offered_not_just_what_was_kept() {
        let (q, stats) = queue(10);
        q.push(&[0.0; 100]);
        assert_eq!(stats.snapshot().samples_out, 100);
        assert_eq!(stats.snapshot().slip_drops, 90);
    }
}

