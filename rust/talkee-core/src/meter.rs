//! Turning arrival timing into a number that can be trusted.
//!
//! # Why the session average was not good enough
//!
//! M0's first instrument divided *everything received so far* by *all the time
//! since the first frame*. On the first real phone run that produced
//! `drift -137784.8 ppm`, which reads as "the phone's clock is 14 % slow". It was
//! not. The last two and a half minutes of that same run were delivering a clean
//! `100 dgram/s`, and the figure never recovered, because an average over a whole
//! session cannot forget the stalls at the start of it.
//!
//! Two things were wrong with it:
//!
//! 1. **It conflates three different failures.** Clock error, packet loss and
//!    scheduling stalls all lower the same number, and they have completely
//!    different fixes.
//! 2. **It is unreadable as a health signal.** A run whose tail is healthy looks
//!    broken, so the number gets ignored — which is worse than not having it.
//!
//! [`ArrivalWindow`] measures one reporting window instead, and reports the three
//! numbers needed to tell the failures apart: how many datagrams arrived per
//! second, how many frames arrived per second, and how many frames each datagram
//! carried.
//!
//! | datagrams/s | frames/s | frames/datagram | Reading |
//! |---|---|---|---|
//! | normal | low | normal | Frames are being lost, or the sender stalls between bursts |
//! | low | low | normal | The same failure seen from the datagram side |
//! | normal | low | low | The sender is emitting short frames |
//! | normal | normal | normal | Healthy now — any `slip`/`under` totals are historical |
//! | normal | normal | normal, but `ppm` pinned | A genuine sample-clock difference |
//!
//! # Why the window is measured from the first datagram
//!
//! The clock starts when the window's **first datagram arrives**, not when the
//! window opens, and that datagram's frames are excluded from the numerator. Both
//! choices shrink the uncertainty from roughly one reporting period to one frame
//! period. Without them, a window whose first datagram happened to arrive late —
//! which is exactly what a stall looks like — reports a rate lowered by a reason
//! that has nothing to do with the rate.
//!
//! What remains is LAN jitter, of the order of ±1 %. That is why this instrument
//! reports `ppm` in whole percents rather than in the hundreds: a 12 % shortfall is
//! unambiguous, a 200 ppm clock difference is not measurable this way, and the
//! queue-depth trend in `docs/LATENCY.md` remains the better tool for small
//! numbers.

use std::time::Instant;

/// Shortest window that may be reported, in seconds.
///
/// The health line reports every 5 s, so this only rejects a window in which
/// almost nothing arrived. It exists because a rate computed over a fraction of a
/// second is dominated by when the first datagram happened to arrive: an earlier
/// 8-second window reported **+17 800 ppm**, some 90x any plausible crystal
/// difference, for exactly that reason.
pub const MIN_RATE_WINDOW_SECS: f64 = 1.0;

/// What arrived during one window of wall-clock time.
///
/// All rates here are measured at the **sender's** rate, on the input side, before
/// resampling: these describe the phone, not the output device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputRate {
    /// Length of the interval the numbers below were measured over, in seconds.
    pub seconds: f64,
    /// Datagrams that carried frames during the window.
    pub datagrams: u64,
    /// Frames those datagrams carried, at the sender's declared rate.
    pub frames: u64,
    /// Datagrams per second.
    pub datagrams_per_second: f64,
    /// Frames per second.
    pub frames_per_second: f64,
    /// Mean frames per datagram. Falls when the sender emits short frames.
    pub frames_per_datagram: f64,
    /// [`frames_per_second`](Self::frames_per_second) against the rate the sender
    /// *claims* in its header, in parts per million.
    ///
    /// A few hundred is a plausible crystal difference; thousands mean the sender
    /// is not keeping up; tens of thousands mean the sample clock differs from what
    /// the header says — or that the capture loop is being starved and losing audio
    /// it never gets to send.
    ///
    /// Telling those two apart is exactly why [`frames_per_datagram`] sits next to
    /// this figure. One real-phone run averaged **-138 000 ppm**, which reads like a
    /// 41.4 kHz capture path advertised as 48 kHz. It was not: a repeat run of the
    /// same build measured `480 frames/datagram` and within **0.15 %** of nominal, so
    /// the framing was never the problem and the shortfall came from somewhere else
    /// entirely. The same evidence fits two different fixes, and only one is right.
    ///
    /// [`frames_per_datagram`]: Self::frames_per_datagram
    pub ppm: f64,
}

impl InputRate {
    /// Measures a window from explicit numbers.
    ///
    /// Returns `None` rather than a made-up figure when there is nothing to
    /// measure: no audio arrived, the window was too short, or the nominal rate is
    /// unusable. A health line that prints `--` is more useful than one that prints
    /// `0 ppm` because the sender has not started yet — the second reads like a
    /// clean stream and is not a measurement at all.
    pub fn between(seconds: f64, datagrams: u64, frames: u64, nominal_rate: u32) -> Option<Self> {
        if !seconds.is_finite() || seconds < MIN_RATE_WINDOW_SECS {
            return None;
        }
        if datagrams == 0 || nominal_rate == 0 {
            return None;
        }
        let frames_per_second = frames as f64 / seconds;
        Some(Self {
            seconds,
            datagrams,
            frames,
            datagrams_per_second: datagrams as f64 / seconds,
            frames_per_second,
            frames_per_datagram: frames as f64 / datagrams as f64,
            ppm: (frames_per_second / nominal_rate as f64 - 1.0) * 1e6,
        })
    }
}

/// Accumulates the arrival pattern of the current reporting window.
///
/// See the module docs for why the clock starts at the first datagram rather than
/// when the window opens.
#[derive(Debug)]
pub struct ArrivalWindow {
    /// Arrival time of the first datagram, i.e. the start of the clock.
    first: Option<Instant>,
    /// Arrival time of the most recent datagram.
    last: Instant,
    /// Datagrams carrying frames in this window, including the first.
    datagrams: u64,
    /// Frames carried by those datagrams, including the first.
    frames: u64,
    /// Frames carried by the first datagram, which the rate excludes because that
    /// datagram also starts the interval being measured.
    first_datagram_frames: u64,
}

impl ArrivalWindow {
    /// Opens a window. Nothing has arrived yet, so the clock is not running.
    pub fn new() -> Self {
        Self {
            first: None,
            last: Instant::now(),
            datagrams: 0,
            frames: 0,
            first_datagram_frames: 0,
        }
    }

    /// Records one datagram carrying `frames` mono frames.
    ///
    /// A datagram carrying nothing is ignored rather than counted, so it cannot
    /// start the clock and dilute the rate.
    pub fn observe(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        let now = Instant::now();
        if self.first.is_none() {
            self.first = Some(now);
            self.first_datagram_frames = frames as u64;
        }
        self.last = now;
        self.datagrams += 1;
        self.frames += frames as u64;
    }

    /// True when no datagram carrying audio has arrived in this window.
    pub fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    /// The counts [`close`](Self::close) will measure, with the first datagram
    /// already excluded.
    ///
    /// Public so the accounting can be asserted without waiting on a clock — the
    /// same reason `Receiver::ingest` is public.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.datagrams.saturating_sub(1),
            self.frames.saturating_sub(self.first_datagram_frames),
        )
    }

    /// Measures the window against `nominal_rate` and starts a new one.
    ///
    /// `nominal_rate` is the rate the sender claims in its header, not the rate we
    /// resample to. This is an input-side measurement.
    ///
    /// Always resets, even when there is nothing to report, so a window cannot leak
    /// counts into its successor. Returns `None` when the window holds no
    /// measurement: nothing arrived, only one datagram did, or the interval was too
    /// short.
    pub fn close(&mut self, nominal_rate: u32) -> Option<InputRate> {
        let opened = self.first.take();
        let seconds = opened.map(|first| self.last.saturating_duration_since(first).as_secs_f64());
        let (datagrams, frames) = self.counts();
        self.datagrams = 0;
        self.frames = 0;
        self.first_datagram_frames = 0;
        // `unwrap_or(0.0)` on an unopened window is below the minimum window, so
        // `between` rejects it for the same reason it rejects a short one.
        InputRate::between(seconds.unwrap_or(0.0), datagrams, frames, nominal_rate)
    }
}

impl Default for ArrivalWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_stream_measures_as_no_drift() {
        // 500 datagrams of 480 frames in 5 s: exactly 100 dgram/s and 48 kHz.
        let rate = InputRate::between(5.0, 500, 240_000, 48_000).unwrap();
        assert!((rate.datagrams_per_second - 100.0).abs() < 1e-9);
        assert!((rate.frames_per_second - 48_000.0).abs() < 1e-9);
        assert!((rate.frames_per_datagram - 480.0).abs() < 1e-9);
        assert_eq!(rate.ppm, 0.0);
    }

    #[test]
    fn a_stalling_sender_shows_as_fewer_datagrams_not_smaller_ones() {
        // The real-phone shape: 440 datagrams of 480 frames in 5 s is 42.24 kHz,
        // with the frame size untouched. That is a delivery problem, not a codec
        // or a capture-format problem, and the two have different fixes.
        let rate = InputRate::between(5.0, 440, 211_200, 48_000).unwrap();
        assert!((rate.frames_per_datagram - 480.0).abs() < 1e-9);
        assert!((rate.frames_per_second - 42_240.0).abs() < 1e-9);
        assert!(
            (rate.ppm + 120_000.0).abs() < 1.0,
            "expected -120 000 ppm, got {}",
            rate.ppm
        );
    }

    #[test]
    fn short_frames_are_visible_as_a_small_frame_size() {
        // A healthy 100 dgram/s, but each datagram carries 441 frames because the
        // capture path is really running at 44.1 kHz while claiming 48 kHz. Only
        // the frames/datagram column distinguishes this from the case above.
        let rate = InputRate::between(5.0, 500, 220_500, 48_000).unwrap();
        assert!((rate.datagrams_per_second - 100.0).abs() < 1e-9);
        assert!((rate.frames_per_datagram - 441.0).abs() < 1e-9);
        assert!(rate.ppm < -80_000.0, "got {} ppm", rate.ppm);
    }

    #[test]
    fn nothing_arrived_is_not_a_rate() {
        // The sender has not started. Reporting 0 ppm would claim a healthy
        // stream; the honest answer is that there is no measurement.
        assert!(InputRate::between(5.0, 0, 0, 48_000).is_none());
    }

    #[test]
    fn a_short_window_refuses_to_report() {
        assert!(InputRate::between(MIN_RATE_WINDOW_SECS - 0.01, 10, 4_800, 48_000).is_none());
        assert!(InputRate::between(0.0, 10, 4_800, 48_000).is_none());
        assert!(InputRate::between(f64::NAN, 10, 4_800, 48_000).is_none());
        assert!(InputRate::between(f64::INFINITY, 10, 4_800, 48_000).is_none());
    }

    #[test]
    fn an_unusable_nominal_rate_refuses_to_report() {
        // `nominal_rate` comes from a parsed network header, so zero is reachable.
        assert!(InputRate::between(5.0, 500, 240_000, 0).is_none());
    }

    #[test]
    fn a_fresh_window_has_no_measurement() {
        let mut window = ArrivalWindow::new();
        assert!(window.is_empty());
        assert_eq!(window.counts(), (0, 0));
        assert!(window.close(48_000).is_none());
    }

    #[test]
    fn the_clock_starts_at_the_first_datagram() {
        // The -388 085 ppm bug: the socket is bound long before the sender starts,
        // and idle time must not be billed as a slow stream.
        let mut window = ArrivalWindow::new();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(window.is_empty(), "waiting is not receiving");
        assert!(window.close(48_000).is_none());

        for _ in 0..500 {
            window.observe(480);
        }
        // 500 datagrams arrived, but the first only started the clock, so the
        // interval being measured covers the other 499 — and the same frames the
        // rate will divide.
        assert_eq!(window.counts(), (499, 499 * 480));
        // Those 499 arrived within microseconds, so the window is still too short
        // to report: a rate needs a real interval behind it.
        assert!(window.close(48_000).is_none());
    }

    #[test]
    fn an_empty_datagram_neither_starts_the_clock_nor_dilutes_the_rate() {
        let mut window = ArrivalWindow::new();
        window.observe(0);
        assert!(window.is_empty());
        window.observe(480);
        window.observe(480);
        assert_eq!(window.counts(), (1, 480));
    }

    #[test]
    fn closing_a_window_resets_it() {
        let mut window = ArrivalWindow::new();
        window.observe(480);
        window.observe(480);
        let _ = window.close(48_000);
        assert!(window.is_empty());
        assert_eq!(window.counts(), (0, 0));
    }
}
