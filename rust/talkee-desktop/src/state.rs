//! The four states, and the pure rules that choose between them.
//!
//! # Why the states *are* the product
//!
//! The desktop side has never had a way to say "I am connected and I cannot hear
//! anything". A microphone that has gone quiet without saying so is the worst
//! failure this app can have: the user keeps talking, the other end keeps hearing
//! silence, and nothing in front of them contradicts it. Every rule in this file
//! exists to make that one condition unambiguous, and to make the app *stop*
//! claiming it the moment the situation changes.
//!
//! # Exactly one, and nothing else
//!
//! [`AppState`] has exactly four variants, and [`decide`] returns exactly one of
//! them for every [`Observation`]. The one thing those four cannot express is "the
//! user pressed Stop", because that is not a condition of the receiver — it is the
//! absence of a session. It is carried separately by the caller, which dims the icon
//! and prefixes the tooltip rather than inventing a fifth state that would claim a
//! condition which is not true.
//!
//! # Everything here is pure
//!
//! The inputs are durations, levels and counters. No socket, no device, no clock, so
//! every rule below is testable — which matters most for the two rules that are easy
//! to get wrong: a *stale* level must never be displayed, and an all-zero output
//! level must not be read as "the device is silent" until we know that the audio
//! callback publishes that counter at all.

use std::time::Duration;

/// How long audio has to be missing before `live` becomes `connected, but silent`.
///
/// The sender emits one datagram every 10 ms (README: ~100 dgram/s, 480 frames
/// each), so this needs 75 consecutive datagrams to go missing. That is far outside
/// LAN jitter — the output queue is only 60 ms deep, so 750 ms is more than twelve
/// times the buffering that could possibly hide a hiccup — and it is comfortably
/// inside "a person notices the silence and starts wondering".
pub const SILENT_AFTER: Duration = Duration::from_millis(750);

/// How long the last peak is shown for after the frame that carried it.
///
/// A peak belongs to a frame, not to the app. Without a hold the meter would drop to
/// zero in the gap between datagrams and flicker at 100 Hz; without a *deadline* it
/// would keep showing a loud level long after the audio stopped, which is exactly the
/// lie this app exists to remove. 250 ms is 25 frames of slack.
pub const LEVEL_HOLD: Duration = Duration::from_millis(250);

/// Number of cells in the two level bars.
///
/// Twelve fits both the tooltip (Windows truncates a tray tooltip at 128 characters)
/// and the 32x32 tray icon, where one cell is two pixels wide.
pub const METER_CELLS: usize = 12;

/// Level below which a peak counts as nothing, in dBFS.
///
/// -66 dBFS is a raw sample of 0.0005. A phone microphone in a quiet room idles
/// around -60 dBFS, so this floor sits under anything it will ever report and above
/// zero: a whisper still lights a cell instead of reading as silence.
pub const LEVEL_FLOOR_DB: f32 = -66.0;

/// The condition the app is showing. Exactly one of these is current at any moment.
/// Where a user who has no virtual microphone has to go.
///
/// There is deliberately **no installer bundled with this app**. VB-Audio's licence does
/// permit redistributing VB-CABLE, but silently installing a kernel audio driver is a change
/// to someone's machine that they should agree to, and the download page is what explains
/// what the thing is. Handing over the address and getting out of the way is the whole
/// feature — and `engine` already re-probes for the cable every couple of seconds, so
/// nothing has to be restarted once it appears.
pub const VB_CABLE_URL: &str = "https://vb-audio.com/Cable/";

/// The address as it should be *shown* to a person.
///
/// Not the same string as the one to open: a full URL in a tray tooltip is noise, and the
/// host is the part anyone would recognise or retype. A test keeps the two from drifting.
pub const VB_CABLE_HOST: &str = "vb-audio.com/Cable";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    /// No virtual cable to render into, so there is nothing to hear even if a phone
    /// is sending perfectly.
    NoCable,
    /// A cable is open and listening, but no audio has arrived in this session.
    Searching,
    /// Audio is arriving and being played.
    Live,
    /// Audio was arriving in this session and has stopped. The state this app was
    /// built for.
    Silent,
}

impl AppState {
    /// Every state, in the order a user meets them.
    ///
    /// Only the tests need the whole list in one place, and they are the only place it can
    /// be checked that all four are distinguishable — the app itself always matches on one
    /// state at a time.
    #[cfg(test)]
    pub const ALL: [AppState; 4] = [
        AppState::NoCable,
        AppState::Searching,
        AppState::Live,
        AppState::Silent,
    ];

    /// Stable identifier, for logs and tests. Not shown to users.
    pub fn as_str(self) -> &'static str {
        match self {
            AppState::NoCable => "no-cable",
            AppState::Searching => "searching",
            AppState::Live => "live",
            AppState::Silent => "silent",
        }
    }

    /// Short title, for the tooltip and the menu status line.
    pub fn title(self) -> &'static str {
        match self {
            AppState::NoCable => "no virtual microphone",
            AppState::Searching => "looking for your phone",
            AppState::Live => "live",
            AppState::Silent => "connected, but silent",
        }
    }

    /// One sentence of what to do about it, for stdout.
    ///
    /// The `Silent` line is the most important string in this crate: it is the only
    /// place in the product where the user is told that their microphone is not
    /// carrying their voice.
    pub fn detail(self) -> &'static str {
        match self {
            AppState::NoCable => {
                "no virtual microphone yet — the menu installs VB-CABLE, a donationware audio \
                 driver by VB-Audio (vb-audio.com/Cable); Windows needs a restart afterwards"
            }
            AppState::Searching => "listening and announcing this PC; open Talkee on your phone",
            AppState::Live => "audio is arriving from your phone and playing into the cable",
            AppState::Silent => {
                "your phone connected but nothing is coming through: check that its microphone \
                 is not muted and that it is still on this network"
            }
        }
    }
}


/// Cells lit by a peak level, `0..=METER_CELLS`.
///
/// A linear bar is useless for speech: the difference between "too quiet" and "good"
/// is 0.01 to 0.3, which is one cell out of twelve. So the mapping is in dBFS — every
/// halving of amplitude moves the bar by a fixed amount — with the bottom of the
/// scale at [`LEVEL_FLOOR_DB`].
///
/// Deliberately not clamped above 1.0, matching `talkee-core`: a meter that cannot
/// show clipping is missing the one thing it is for.
pub fn level_cells(level: f32) -> usize {
    if !level.is_finite() || level <= 0.0 {
        return 0;
    }
    let db = 20.0 * level.log10();
    if db <= LEVEL_FLOOR_DB {
        return 0;
    }
    let fraction = (db - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB;
    let cells = (fraction * METER_CELLS as f32).ceil();
    cells.clamp(1.0, METER_CELLS as f32) as usize
}

/// One bar, from an already-computed cell count. Always [`METER_CELLS`] long.
pub fn cells_bar(cells: usize) -> String {
    let lit = cells.min(METER_CELLS);
    let mut out = String::with_capacity(METER_CELLS);
    for index in 0..METER_CELLS {
        out.push(if index < lit { '#' } else { '.' });
    }
    out
}

/// One bar, from a peak level. `#` for lit cells, `.` for dark ones.
pub fn bar(level: f32) -> String {
    cells_bar(level_cells(level))
}

/// A level that is still worth showing, or zero.
///
/// This is the rule that stops the app lying in the other direction. `Stats` keeps
/// the peak of the *last frame it saw* forever, so once the phone falls silent its
/// final reading sits there at full scale and a naive meter would keep painting a
/// healthy bar over a dead stream. After [`LEVEL_HOLD`] with no new frame, the honest
/// reading is zero.
pub fn held_level(raw: f32, since_last_datagram: Duration) -> f32 {
    if since_last_datagram <= LEVEL_HOLD {
        raw
    } else {
        0.0
    }
}

/// What the app knows about the *output* half of the pipeline.
///
/// Split out from a plain `f32` because the three cases below are genuinely
/// different, and conflating them is how a meter ends up claiming that a working
/// device is silent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutMeter {
    /// The audio callback published a peak. This is the real number, zero included.
    Measured(f32),
    /// Audio has not started leaving yet — normal for the first prefill (~60 ms), so
    /// silence here means nothing.
    Waiting,
    /// Frames are arriving and being played, yet nothing has ever published a
    /// non-zero `Stats::output_level`. The counter is not being written by this
    /// build's audio path, so an empty bar would be an assertion the app cannot
    /// support.
    ///
    /// This is the documented seam: `talkee-vmic` owns the audio callback and is
    /// being extended right now. The desktop side wants exactly one thing from it —
    /// a `set_output_level` call per callback — and until that exists this variant
    /// reports the unknown honestly instead of drawing a bar that means nothing.
    Unpublished,
}

impl OutMeter {
    /// The number to draw, if there is one.
    pub fn level(self) -> Option<f32> {
        match self {
            OutMeter::Measured(level) => Some(level),
            OutMeter::Waiting => Some(0.0),
            OutMeter::Unpublished => None,
        }
    }

    /// The bar to draw, with `?` where the app cannot measure the output side.
    pub fn bar(self) -> String {
        match self.level() {
            Some(level) => bar(level),
            None => "?".repeat(METER_CELLS),
        }
    }

    /// Cells lit, or `None` when the output side is not being measured.
    pub fn cells(self) -> Option<usize> {
        self.level().map(level_cells)
    }
}

/// How long audible audio has to flow before an all-zero output level is read as
/// "nothing publishes this counter" rather than "the device has not started".
///
/// The output queue prefills for 60 ms, so the first half-second of a session is
/// legitimately silent; 1.5 s is 25 times that, and short enough that a user who
/// starts speaking is told the truth about the other bar before they finish a word.
pub const OUTPUT_PUBLISH_GRACE: Duration = Duration::from_millis(1500);

/// Watches `Stats::output_level` to find out whether anything writes it.
///
/// # Why this exists
///
/// `Stats::output_level()` was added for this meter, but at the time of writing nothing in
/// the pipeline publishes it, so it reads `0.0` whether the device is silent or merely
/// unmeasured. Those two are indistinguishable from a single reading, and the only honest
/// way to tell them apart is time: once audio is *demonstrably* flowing (frames arriving
/// above the floor) and the counter has still never been non-zero after a grace period, the
/// counter is not being written.
///
/// Once a non-zero value has been seen the counter is trusted absolutely — a later zero is
/// then a real zero, and reporting it as "unknown" would be its own lie.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OutTracker {
    seen_nonzero: bool,
    flowing_for: Duration,
}

impl OutTracker {
    /// Feeds one tick. `input_is_flowing` means *audible* audio is arriving now.
    pub fn observe(&mut self, raw_out: f32, input_is_flowing: bool, elapsed: Duration) {
        if raw_out.is_finite() && raw_out > 0.0 {
            self.seen_nonzero = true;
        }
        if input_is_flowing {
            self.flowing_for = self.flowing_for.saturating_add(elapsed);
        }
    }

    /// How the output side should be described right now.
    pub fn meter(&self, raw_out: f32) -> OutMeter {
        if self.seen_nonzero {
            return OutMeter::Measured(raw_out);
        }
        if self.flowing_for < OUTPUT_PUBLISH_GRACE {
            return OutMeter::Waiting;
        }
        OutMeter::Unpublished
    }
}

/// Everything the state machine is allowed to look at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    /// A virtual cable endpoint exists on this machine.
    pub cable_present: bool,
    /// Datagrams accepted from the sender in this session.
    pub datagrams: u64,
    /// Since the most recent accepted datagram.
    pub since_last_datagram: Duration,
    /// How long the input has been *below the floor while frames kept arriving*.
    pub quiet_for: Duration,
    /// Peak of the most recent frame, straight from `Stats::input_level`.
    pub level_in: f32,
    /// Peak of the most recent frame handed to the device, from
    /// `Stats::output_level`.
    pub level_out: f32,
    /// Datagrams that arrived and were thrown away, for any reason.
    ///
    /// Read for one specific case: the security gate refusing unencrypted audio means
    /// datagrams *are* arriving while `datagrams` stays at zero, and "looking for your
    /// phone" would then be a lie — the phone is right there and being turned away.
    pub refused: u64,
}

/// The observation for a session that has not started: a cable, and nothing else.
pub fn idle_observation(cable_present: bool) -> Observation {
    Observation {
        cable_present,
        datagrams: 0,
        since_last_datagram: Duration::ZERO,
        quiet_for: Duration::ZERO,
        level_in: 0.0,
        level_out: 0.0,
        refused: 0,
    }
}

/// How long the audio has been missing, from whichever side noticed first.
///
/// Two different failures produce the same silence and both have to raise it: the
/// datagrams stopping (phone out of range, Wi-Fi gone) and the datagrams continuing
/// with a muted microphone. Taking the larger of the two means the state cannot be
/// held off by a stream that is technically alive and carrying nothing.
pub fn stall(o: &Observation) -> Duration {
    o.since_last_datagram.max(o.quiet_for)
}

/// The one state this observation calls for.
pub fn decide(o: &Observation) -> AppState {
    if !o.cable_present {
        return AppState::NoCable;
    }
    if o.datagrams == 0 {
        return AppState::Searching;
    }
    if stall(o) >= SILENT_AFTER {
        AppState::Silent
    } else {
        AppState::Live
    }
}

/// Advances the "how long has the input been quiet" clock by one tick.
///
/// Pure, because the rule is subtle enough to deserve a test rather than a
/// comment-only guarantee:
///
/// * a fresh datagram above the floor resets it — the stream is alive and loud;
/// * a fresh datagram below the floor extends it — frames are flowing, sound is not,
///   which is what a muted phone microphone looks like;
/// * *no* datagram freezes it. The last level reading is then stale, and letting a
///   stale reading reset or extend the clock would make the state depend on data that
///   is no longer being produced. The stall timer covers that case instead.
pub fn advance_quiet(
    quiet_for: Duration,
    datagram_arrived: bool,
    level_in: f32,
    elapsed: Duration,
) -> Duration {
    if !datagram_arrived {
        return quiet_for;
    }
    if level_cells(level_in) == 0 {
        quiet_for.saturating_add(elapsed)
    } else {
        Duration::ZERO
    }
}

/// The app's display state, derived once per tick.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The current state. Exactly one of four.
    pub state: AppState,
    /// Input level to draw, already passed through [`held_level`].
    pub level_in: f32,
    /// Output meter to draw.
    pub out: OutMeter,
    /// How long the audio has been missing.
    pub stall: Duration,
    /// Datagrams accepted this session.
    pub datagrams: u64,
    /// Frames inferred lost, from gaps in the sequence numbers.
    pub lost: u64,
    /// Audio callbacks that ran short and padded with silence.
    pub underruns: u64,
    /// Frames buffered for the device.
    pub queue_frames: usize,
    /// Datagrams thrown away this session, whatever the reason.
    pub refused: u64,
}

impl Reading {
    /// Derives the display state from one observation.
    pub fn sample(o: &Observation, out: OutMeter) -> Self {
        Self {
            state: decide(o),
            level_in: held_level(o.level_in, o.since_last_datagram),
            out,
            stall: stall(o),
            datagrams: o.datagrams,
            lost: 0,
            underruns: 0,
            queue_frames: 0,
            refused: o.refused,
        }
    }

    /// Input bar, as text.
    pub fn in_bar(&self) -> String {
        bar(self.level_in)
    }

    /// Output bar, as text.
    pub fn out_bar(&self) -> String {
        self.out.bar()
    }

    /// True when audio is arriving but demonstrably not leaving.
    ///
    /// Worth its own sentence because it is a failure with a completely different fix
    /// from "the phone is quiet": the phone is fine and the device is not being fed.
    /// Only claimed once the output counter is trusted, so a missing counter can
    /// never produce it.
    pub fn output_stalled(&self) -> bool {
        match self.out {
            OutMeter::Measured(level) => level_cells(self.level_in) >= 3 && level_cells(level) == 0,
            _ => false,
        }
    }

    /// Tooltip for the tray icon. Carries the state, which is the requirement.
    ///
    /// Kept under 120 characters because that is all a Windows tooltip holds; `silent`
    /// is deliberately spelled out in full rather than abbreviated, since it is the
    /// state that must be impossible to misread.
    pub fn tooltip(&self, hint: Option<&str>) -> String {
        let mut text = match self.state {
            // The action is in the tray menu now, so this names where to press rather than sending
            // anyone to a website. Whose driver it is lives in the menu's footnote and in the
            // notice printed at install time — this line is about the action.
            AppState::NoCable => {
                "Talkee — no virtual microphone: choose \"Install VB-CABLE\" in this menu".to_string()
            }
            AppState::Searching => {
                if self.refused > 0 {
                    // Not "looking for your phone": the phone is here and being turned
                    // away. Without this line the user would reinstall the app.
                    "Talkee — your phone is connecting but its audio is refused: this \
                     build will not play unencrypted audio"
                        .to_string()
                } else {
                    let why = hint.unwrap_or("listening and announcing this PC");
                    format!("Talkee — looking for your phone ({why})")
                }
            }
            AppState::Live => {
                format!("Talkee — live · in [{}] out [{}]", self.in_bar(), self.out_bar())
            }
            // Spelled the same way as `AppState::title` apart from the upper case, which
            // is deliberate emphasis: this is the one string in the product that tells a
            // user their microphone is not carrying their voice.
            AppState::Silent => format!(
                "Talkee — connected, but SILENT for {:.1}s · in [{}] out [{}]",
                self.stall.as_secs_f64(),
                self.in_bar(),
                self.out_bar()
            ),
        };
        if self.output_stalled() {
            text.push_str(" · audio arriving, not leaving");
        }
        truncate(text, 120)
    }

    /// The line the app prints when the state changes.
    pub fn state_line(&self) -> String {
        format!("state -> {}: {}", self.state.title(), self.state.detail())
    }

    /// The line the app prints while a level is moving.
    pub fn level_line(&self) -> String {
        let out = match self.out {
            OutMeter::Measured(level) => format!("{level:.4}"),
            OutMeter::Waiting => "prefill".to_string(),
            OutMeter::Unpublished => "not-published".to_string(),
        };
        format!(
            "level    in [{}] out [{}]  in={:.4} out={} lost={} under={} buffer={}f",
            self.in_bar(),
            self.out_bar(),
            self.level_in,
            out,
            self.lost,
            self.underruns,
            self.queue_frames,
        )
    }
}

/// Shortens `text` to at most `limit` characters, marking the cut with an ellipsis.
pub fn truncate(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let keep = limit.saturating_sub(3);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("...");
    out
}

/// The tooltip the tray icon shows for a reading.
///
/// Lives here rather than in the tray module so the app can print exactly the string the
/// user would see on hover. With no window, that log line is the only way to verify the
/// tooltip without a taskbar — and the tooltip is where the state is stated in words.
///
/// When nothing is listening, none of the four states is true, so the honest tooltip says
/// "stopped" and then what the user would get if they pressed Start.
pub fn tray_tooltip(reading: &Reading, stopped: bool, hint: Option<&str>) -> String {
    if stopped {
        return truncate(
            format!("Talkee — stopped · if started: {}", reading.state.title()),
            120,
        );
    }
    reading.tooltip(hint)
}

/// Formats a duration since the app started, as `mm:ss.mmm`.
///
/// Every line the app prints is prefixed with this, so a state transition can be
/// lined up against whatever was happening outside the app at that moment — which is
/// the only way to verify a tray app that nobody can look at.
pub fn format_elapsed(elapsed: Duration) -> String {
    let total_ms = elapsed.as_millis() as u64;
    let minutes = total_ms / 60_000;
    let seconds = (total_ms % 60_000) / 1000;
    let millis = total_ms % 1000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A healthy session: a cable, 100 datagrams, audio at `level_in` that arrived
    /// just now.
    fn observing(level_in: f32, since: Duration) -> Observation {
        Observation {
            cable_present: true,
            datagrams: 100,
            since_last_datagram: since,
            quiet_for: Duration::ZERO,
            level_in,
            level_out: 0.0,
            refused: 0,
        }
    }

    #[test]
    fn the_missing_cable_message_says_where_to_get_one() {
        // The user can install it from the menu, so this address is the fallback rather than the
        // only route. It still has to be there: the terms that let us ship the package require
        // the origin to be visible, and a search engine would offer a dozen installers of unknown
        // provenance for something that has to become a system audio device.
        let o = Observation {
            cable_present: false,
            ..observing(0.0, Duration::ZERO)
        };
        let reading = Reading::sample(&o, OutMeter::Measured(0.0));
        assert_eq!(reading.state, AppState::NoCable);

        let tooltip = reading.tooltip(None);
        assert!(
            tooltip.contains("VB-CABLE"),
            "the tooltip must name what it is asking the user to install: {tooltip}"
        );
        // The origin lives in the log line and the menu footnote, not in the tooltip: the terms
        // ask that the user can *see and identify* the driver, not that it be shouted on hover.
        assert!(
            AppState::NoCable.detail().contains(VB_CABLE_HOST),
            "the log line names no source: {}",
            AppState::NoCable.detail()
        );

        // The link that gets opened and the address that gets shown must not drift apart:
        // a tooltip telling a user one thing while the menu opens another is worse than no
        // link at all.
        assert!(
            VB_CABLE_URL.contains(VB_CABLE_HOST),
            "the address shown ({VB_CABLE_HOST}) is not part of the address opened ({VB_CABLE_URL})"
        );
        assert!(
            VB_CABLE_URL.starts_with("https://"),
            "a download page for a system audio driver must be https"
        );
    }

    #[test]
    fn a_missing_cable_beats_everything_else() {
        // Even with audio arriving, nothing can be heard without a cable. Reporting
        // "live" here would be the single most misleading thing the app could say.
        let o = Observation {
            cable_present: false,
            level_in: 0.9,
            ..observing(0.0, Duration::ZERO)
        };
        assert_eq!(decide(&o), AppState::NoCable);
    }

    #[test]
    fn before_any_audio_the_state_is_searching() {
        let o = Observation {
            datagrams: 0,
            since_last_datagram: Duration::from_secs(30),
            quiet_for: Duration::from_secs(30),
            ..observing(0.0, Duration::ZERO)
        };
        assert_eq!(
            decide(&o),
            AppState::Searching,
            "a quiet cable with no session is searching, not silent: nothing has \
             connected yet, and calling that silent would train the user to ignore the \
             one state that must never be ignored"
        );
    }

    #[test]
    fn the_silent_threshold_is_a_boundary_not_a_range() {
        let mut o = observing(0.2, SILENT_AFTER - Duration::from_millis(1));
        assert_eq!(decide(&o), AppState::Live);
        o.since_last_datagram = SILENT_AFTER;
        assert_eq!(decide(&o), AppState::Silent);
    }

    #[test]
    fn a_muted_microphone_is_silent_even_though_datagrams_keep_arriving() {
        // The failure this app exists for: the phone is connected, the stream is
        // healthy, and there is no sound in it. Only the quiet clock can see this, so
        // the state must not rest on the datagram timer alone.
        let mut o = observing(0.0, Duration::ZERO);
        o.quiet_for = SILENT_AFTER - Duration::from_millis(1);
        assert_eq!(decide(&o), AppState::Live);
        o.quiet_for = SILENT_AFTER;
        assert_eq!(decide(&o), AppState::Silent);
    }

    #[test]
    fn a_stale_level_is_not_displayed() {
        // Stats keeps the last frame's peak forever. Painting that peak over a dead
        // stream is the lie in the other direction, and it is why this hold exists.
        assert_eq!(held_level(0.9, Duration::ZERO), 0.9);
        assert_eq!(held_level(0.9, LEVEL_HOLD), 0.9);
        assert_eq!(held_level(0.9, LEVEL_HOLD + Duration::from_millis(1)), 0.0);
    }

    #[test]
    fn a_quiet_session_shows_the_real_reason_it_is_quiet() {
        let o = observing(0.7, Duration::from_millis(1200));
        let reading = Reading::sample(&o, OutMeter::Measured(0.0));
        assert_eq!(reading.state, AppState::Silent);
        assert_eq!(
            reading.level_in, 0.0,
            "the level belongs to a frame that is 1.2 s old"
        );
        assert_eq!(reading.in_bar(), cells_bar(0));
    }

    #[test]
    fn the_quiet_clock_freezes_when_frames_stop_arriving() {
        let running = Duration::from_millis(200);
        // Fresh frames, loud: reset.
        assert_eq!(
            advance_quiet(running, true, 0.3, Duration::from_millis(50)),
            Duration::ZERO
        );
        // Fresh frames, quiet: accumulate.
        assert_eq!(
            advance_quiet(running, true, 0.0, Duration::from_millis(50)),
            Duration::from_millis(250)
        );
        // No frames: frozen, because the level we are holding is stale.
        assert_eq!(
            advance_quiet(running, false, 0.0, Duration::from_millis(50)),
            running
        );
        assert_eq!(
            advance_quiet(running, false, 0.9, Duration::from_millis(50)),
            running,
            "a stale loud reading must not reset a real quiet streak"
        );
    }

    #[test]
    fn the_meter_is_monotonic_and_survives_bad_input() {
        let mut previous = 0;
        for step in 0..=100 {
            let level = step as f32 / 100.0;
            let cells = level_cells(level);
            assert!(cells >= previous, "{level} went backwards");
            previous = cells;
        }
        assert_eq!(level_cells(0.0), 0);
        assert_eq!(level_cells(1.0), METER_CELLS, "full scale must be a full bar");
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
            assert_eq!(level_cells(bad), 0, "{bad} is not a level");
        }
        assert_eq!(level_cells(50.0), METER_CELLS, "clipping reports full, not more");
    }

    #[test]
    fn the_meter_resolves_speech_rather_than_hiding_it() {
        // A linear meter puts 0.001 and 0.1 in the same cell. That is the difference
        // between "nobody can hear you" and "fine", so the scale is logarithmic and
        // the small end has to stay visible.
        assert!(level_cells(0.001) >= 1, "a quiet voice must light something");
        assert!(level_cells(0.01) > level_cells(0.001));
        assert!(level_cells(0.3) > level_cells(0.03));
        assert!(
            level_cells(0.001) < level_cells(0.1),
            "the floor must not squeeze the useful range into one cell"
        );
    }

    #[test]
    fn bars_are_always_the_same_width() {
        for cells in 0..=METER_CELLS + 3 {
            let bar = cells_bar(cells);
            assert_eq!(bar.chars().count(), METER_CELLS);
            assert_eq!(
                bar.chars().filter(|c| *c == '#').count(),
                cells.min(METER_CELLS)
            );
        }
    }

    #[test]
    fn an_unpublished_output_counter_is_not_reported_as_silence() {
        // The trap: output_level reads 0.0 both when the device is silent and when
        // nobody writes the counter. Claiming the former is a lie the user cannot
        // detect, so the meter has to be able to say "I do not know".
        let mut tracker = OutTracker::default();
        tracker.observe(0.0, true, Duration::from_millis(500));
        assert_eq!(
            tracker.meter(0.0),
            OutMeter::Waiting,
            "inside the prefill grace period, silence is expected"
        );

        tracker.observe(0.0, true, Duration::from_secs(1));
        assert_eq!(tracker.meter(0.0), OutMeter::Unpublished);
        assert_eq!(OutMeter::Unpublished.bar(), "?".repeat(METER_CELLS));
        assert_eq!(OutMeter::Unpublished.cells(), None);
    }

    #[test]
    fn a_published_output_counter_is_trusted_including_its_zeros() {
        let mut tracker = OutTracker::default();
        tracker.observe(0.4, true, Duration::from_millis(10));
        assert_eq!(
            tracker.meter(0.4),
            OutMeter::Measured(0.4),
            "a real peak is reported as it is"
        );
        // From here on a zero is a real zero, and calling it "unknown" would be its own kind
        // of lie: the meter knows the counter is alive.
        tracker.observe(0.0, true, Duration::from_secs(5));
        assert_eq!(tracker.meter(0.0), OutMeter::Measured(0.0));
        assert_eq!(tracker.meter(0.0).bar(), cells_bar(0));
    }

    #[test]
    fn nothing_flowing_leaves_the_output_unjudged() {
        // A quiet phone is not evidence about the output counter, so the meter must
        // stay in "waiting" no matter how long the app has been open.
        let mut tracker = OutTracker::default();
        tracker.observe(0.0, false, Duration::from_secs(3600));
        assert_eq!(tracker.meter(0.0), OutMeter::Waiting);
    }

    #[test]
    fn arriving_but_not_leaving_is_called_out() {
        let o = observing(0.25, Duration::ZERO);
        assert!(
            Reading::sample(&o, OutMeter::Measured(0.0)).output_stalled(),
            "loud input and a dead output is a different failure from a quiet phone"
        );
        assert!(!Reading::sample(&o, OutMeter::Measured(0.24)).output_stalled());
        assert!(
            !Reading::sample(&o, OutMeter::Unpublished).output_stalled(),
            "an unmeasured output can never be called stalled"
        );
    }

    #[test]
    fn every_state_has_a_distinct_tooltip_that_names_it() {
        let mut seen = Vec::new();
        for state in AppState::ALL {
            let o = match state {
                AppState::NoCable => Observation {
                    cable_present: false,
                    ..observing(0.0, Duration::ZERO)
                },
                AppState::Searching => Observation {
                    datagrams: 0,
                    ..observing(0.0, Duration::ZERO)
                },
                AppState::Live => observing(0.2, Duration::ZERO),
                AppState::Silent => observing(0.0, SILENT_AFTER),
            };
            let reading = Reading::sample(&o, OutMeter::Measured(0.2));
            assert_eq!(reading.state, state);
            let tooltip = reading.tooltip(None);
            // Case-insensitive on purpose: the tray shouts SILENT, and the invariant is
            // that the state is *named*, not that it is typeset identically to the menu.
            assert!(
                tooltip.to_lowercase().contains(&state.title().to_lowercase()),
                "{state:?} tooltip does not name its state: {tooltip}"
            );
            assert!(tooltip.chars().count() <= 120, "tooltip too long: {tooltip}");
            seen.push(tooltip);
        }
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            AppState::ALL.len(),
            "two states must never look the same in the tray"
        );
    }

    #[test]
    fn the_silent_tooltip_says_how_long() {
        let o = observing(0.0, Duration::from_millis(4200));
        let tooltip = Reading::sample(&o, OutMeter::Measured(0.0)).tooltip(None);
        assert!(tooltip.contains("SILENT"), "{tooltip}");
        assert!(tooltip.contains("4.2s"), "{tooltip}");
    }

    #[test]
    fn refused_audio_is_not_described_as_waiting_for_a_phone() {
        // The phone is connected and being turned away. The state is still "searching"
        // because nothing has been accepted, but the tooltip must not tell the user to go
        // and open the app again.
        let o = Observation {
            datagrams: 0,
            refused: 40,
            ..observing(0.0, Duration::ZERO)
        };
        let reading = Reading::sample(&o, OutMeter::Waiting);
        assert_eq!(reading.state, AppState::Searching);
        let tooltip = reading.tooltip(None);
        assert!(tooltip.contains("refused"), "{tooltip}");
        assert!(
            !tooltip.contains("looking for your phone"),
            "audio is arriving, so the app is not still looking: {tooltip}"
        );
        assert_eq!(reading.refused, 40);
    }

    #[test]
    fn timestamps_are_fixed_width_and_readable() {
        assert_eq!(format_elapsed(Duration::ZERO), "00:00.000");
        assert_eq!(format_elapsed(Duration::from_millis(1)), "00:00.001");
        assert_eq!(format_elapsed(Duration::from_millis(61_500)), "01:01.500");
        assert_eq!(format_elapsed(Duration::from_secs(3_600)), "60:00.000");
    }

    #[test]
    fn a_long_tooltip_is_cut_rather_than_silently_truncated_by_windows() {
        let cut = truncate("x".repeat(200), 120);
        assert_eq!(cut.chars().count(), 120);
        assert!(cut.ends_with("..."));
        assert_eq!(truncate("short".to_string(), 120), "short");
    }
}
