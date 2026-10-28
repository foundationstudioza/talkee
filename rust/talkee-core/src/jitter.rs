//! How much audio to hold back, judged by how unevenly it arrives.
//!
//! # The problem this solves
//!
//! A phone sends a packet every 10 or 20 ms, but Wi-Fi does not deliver them that evenly, and the
//! phone itself may be late reading its microphone: they arrive in small bursts with gaps between. A buffer that is deep enough for a quiet network runs dry in the gap
//! and then overflows with the burst, so the cable gets a click, then the controller throws audio away
//! to get back to its depth, and the person hears both. A buffer deep enough for the worst network
//! adds delay on a good one for no reason.
//!
//! The answer is to measure. Packet number *n* holds the audio from `n * frame` after the stream began,
//! so it ought to arrive that long after the first one, give or take a constant delay. How much later
//! than that it actually arrives, `arrival - n * frame`, is its transit. The packet's own timestamp is
//! deliberately not used: it says when the phone *sent* it, so a phone that stalled and then sent six
//! packets at once would look perfectly punctual. The *smallest* transit in the last few seconds is the
//! network at its best; how far the others stand above it is how much the network wandered, and that
//! wander is exactly how much audio has to be waiting to ride it out.
//!
//! # Fast up, slow down
//!
//! Being too shallow costs a click and being too deep costs a few milliseconds, so the target rises as
//! soon as the network gets worse and falls back only gradually once it has been calm for a while.

use std::collections::VecDeque;
use std::time::Duration;

/// How far back the estimate looks.
const WINDOW: Duration = Duration::from_secs(10);

/// Fewest packets before an estimate is offered. A second of audio.
const MIN_SAMPLES: usize = 60;

/// The share of packets the buffer should be deep enough for. The worst few are given up: chasing
/// every outlier would keep the buffer deep for a delay that happens once a minute.
const PERCENTILE: f64 = 0.98;

/// Extra depth on top of what was measured, in ms, for the burst that has not happened yet.
const HEADROOM_MS: f64 = 20.0;

/// Shallowest the buffer is allowed to get, in ms.
pub const MIN_TARGET_MS: u32 = 50;

/// Deepest, in ms. Past this the delay is worse than the dropouts.
pub const MAX_TARGET_MS: u32 = 200;

/// How fast the target may fall, in ms per second.
const FALL_MS_PER_SEC: f64 = 5.0;

/// Measures how much the delay of packets varies.
#[derive(Debug, Default)]
pub struct JitterEstimator {
    /// (arrival in µs on the receiver's clock, transit in µs: arrival minus where in the stream the
    /// packet belongs).
    transits: VecDeque<(u64, i64)>,
    /// The first sequence number seen, so a stream that does not start at zero is treated as if it had.
    first_seq: Option<u32>,
}

impl JitterEstimator {
    /// An estimator that has seen nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets everything, for a stream that has started over (its clock restarts at zero).
    pub fn reset(&mut self) {
        self.transits.clear();
        self.first_seq = None;
    }

    /// Records one packet: when it arrived (time since any fixed start), its sequence number, and how
    /// long one packet of audio is.
    pub fn observe(&mut self, arrived: Duration, seq: u32, frame: Duration) {
        let first = *self.first_seq.get_or_insert(seq);
        // Wrapping, so a sequence number that rolls over costs nothing.
        let sent_us = seq.wrapping_sub(first) as u64 * frame.as_micros() as u64;
        let arrived_us = arrived.as_micros() as u64;
        self.transits.push_back((arrived_us, arrived_us as i64 - sent_us as i64));
        let horizon = arrived_us.saturating_sub(WINDOW.as_micros() as u64);
        while self.transits.front().is_some_and(|&(at, _)| at < horizon) {
            self.transits.pop_front();
        }
    }

    /// How much audio, in ms, has needed to be waiting to ride out the network, or `None` before there
    /// is enough to say.
    pub fn needed_ms(&self) -> Option<f64> {
        if self.transits.len() < MIN_SAMPLES {
            return None;
        }
        let best = self.transits.iter().map(|&(_, t)| t).min()?;
        let mut waits: Vec<i64> = self.transits.iter().map(|&(_, t)| t - best).collect();
        waits.sort_unstable();
        let index = ((waits.len() - 1) as f64 * PERCENTILE).round() as usize;
        Some(waits[index] as f64 / 1000.0)
    }
}

/// The least depth that can work at all, in ms, for a device that takes `callback_ms` at a time from
/// packets that bring `packet_ms` at a time.
///
/// The queue is drained a callback at a time and refilled a packet at a time, so even on a perfect
/// network it swings by about the sum of the two; a target below that underruns however steadily the
/// audio arrives. The extra 15 ms is for the scheduler.
pub fn floor_ms(callback_ms: f64, packet_ms: f64) -> f64 {
    callback_ms + packet_ms + 15.0
}

/// The depth to aim for next, in ms, given the one in force, what the network needs now and the
/// [`floor_ms`] the device sets.
///
/// Rises at once to what is needed; falls by at most [`FALL_MS_PER_SEC`] per second of `elapsed`.
pub fn next_target_ms(current_ms: f64, needed_ms: f64, floor_ms: f64, elapsed: Duration) -> f64 {
    let lowest = floor_ms.max(MIN_TARGET_MS as f64).min(MAX_TARGET_MS as f64);
    let wanted = (needed_ms + HEADROOM_MS).clamp(lowest, MAX_TARGET_MS as f64);
    if wanted >= current_ms {
        wanted
    } else {
        (current_ms - FALL_MS_PER_SEC * elapsed.as_secs_f64()).max(wanted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `seconds` of packets every `every_ms`, with `delay(i)` ms of extra transit each. `start_ms`
    /// is where in time the feed begins, and the packet numbers carry on from `start_ms / every_ms`.
    fn feed(est: &mut JitterEstimator, start_ms: u64, seconds: u64, every_ms: u64, delay: impl Fn(u64) -> u64) -> u64 {
        let mut i = 0;
        let mut at = start_ms;
        while at < start_ms + seconds * 1000 {
            let seq = (at / every_ms) as u32;
            est.observe(Duration::from_millis(at + delay(i)), seq, Duration::from_millis(every_ms));
            i += 1;
            at += every_ms;
        }
        at
    }

    #[test]
    fn nothing_is_said_before_there_is_enough_to_say_it() {
        let mut est = JitterEstimator::new();
        feed(&mut est, 0, 0, 10, |_| 0);
        assert_eq!(est.needed_ms(), None);
    }

    #[test]
    fn a_steady_network_needs_nothing_held_back() {
        let mut est = JitterEstimator::new();
        feed(&mut est, 0, 5, 10, |_| 3);
        assert!(est.needed_ms().unwrap() < 1.0);
    }

    #[test]
    fn bursts_need_a_buffer_as_long_as_the_gap_between_them() {
        let mut est = JitterEstimator::new();
        // Ten packets (100 ms of audio) arrive together at the end of every 100 ms.
        feed(&mut est, 0, 8, 10, |i| 90 - (i % 10) * 10);
        let needed = est.needed_ms().unwrap();
        assert!((80.0..=95.0).contains(&needed), "{needed}");
    }

    #[test]
    fn one_rare_stall_is_not_chased() {
        let mut est = JitterEstimator::new();
        feed(&mut est, 0, 8, 10, |i| if i == 400 { 300 } else { 2 });
        assert!(est.needed_ms().unwrap() < 10.0);
    }

    #[test]
    fn old_trouble_is_forgotten() {
        let mut est = JitterEstimator::new();
        let end = feed(&mut est, 0, 5, 10, |i| (i % 10) * 12);
        assert!(est.needed_ms().unwrap() > 80.0);
        feed(&mut est, end, 12, 10, |_| 1);
        assert!(est.needed_ms().unwrap() < 2.0);
    }

    #[test]
    fn a_stream_that_starts_at_any_sequence_number_gives_the_same_answer() {
        let mut a = JitterEstimator::new();
        let mut b = JitterEstimator::new();
        let frame = Duration::from_millis(10);
        for i in 0..400u64 {
            let wait = (i % 10) * 9;
            a.observe(Duration::from_millis(i * 10 + wait), i as u32, frame);
            // Close enough to the top of the range to roll over mid-feed.
            b.observe(Duration::from_millis(i * 10 + wait), (u32::MAX - 100).wrapping_add(i as u32), frame);
        }
        assert_eq!(a.needed_ms(), b.needed_ms());
    }

    #[test]
    fn a_phone_that_stalls_and_then_sends_a_burst_is_seen_even_though_its_stamps_look_punctual() {
        // Packets 0..49 on time; then nothing for 100 ms; then packets 50..59 all at once. Each is sent
        // (and would be stamped) the moment it leaves, so only its place in the stream shows the stall.
        let mut est = JitterEstimator::new();
        let frame = Duration::from_millis(10);
        for n in 0..50u32 {
            est.observe(Duration::from_millis(n as u64 * 10), n, frame);
        }
        for n in 50..60u32 {
            est.observe(Duration::from_millis(600), n, frame);
        }
        for n in 60..400u32 {
            est.observe(Duration::from_millis(n as u64 * 10 + 100), n, frame);
        }
        assert!(est.needed_ms().unwrap() >= 90.0);
    }

    #[test]
    fn the_target_rises_at_once_and_falls_slowly() {
        let up = next_target_ms(60.0, 120.0, 0.0, Duration::from_millis(200));
        assert_eq!(up, 140.0);
        let down = next_target_ms(140.0, 0.0, 0.0, Duration::from_secs(1));
        assert_eq!(down, 135.0);
        // It never goes below the floor, nor above the ceiling.
        assert_eq!(next_target_ms(52.0, 0.0, 0.0, Duration::from_secs(10)), MIN_TARGET_MS as f64);
        assert_eq!(next_target_ms(60.0, 900.0, 0.0, Duration::from_secs(1)), MAX_TARGET_MS as f64);
    }

    #[test]
    fn a_device_that_takes_big_chunks_raises_the_floor() {
        // 22 ms callbacks fed 20 ms packets cannot work below 57 ms, however calm the network.
        let floor = floor_ms(22.0, 20.0);
        assert_eq!(floor, 57.0);
        assert_eq!(next_target_ms(80.0, 0.0, floor, Duration::from_secs(60)), 57.0);
        // Chunks too big for the ceiling are capped by it rather than aimed beyond it.
        assert_eq!(next_target_ms(80.0, 0.0, 400.0, Duration::from_secs(1)), MAX_TARGET_MS as f64);
    }
}
