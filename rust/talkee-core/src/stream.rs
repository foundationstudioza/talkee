//! UDP audio receive path.
//!
//! One datagram carries one frame. The socket lives on its own OS thread
//! deliberately: it is *not* the audio thread, so it may block, allocate and
//! resample. Its only real-time obligation is to not hold the
//! [`SharedQueue`] lock for long, which it satisfies by doing all decoding and
//! resampling *before* taking the lock once per frame.

use crate::controller::{DepthController, UPDATE_INTERVAL_MS};
use crate::level::AutoLevel;
use crate::meter::ArrivalWindow;
use crate::queue::SharedQueue;
use crate::resample::LinearResampler;
use crate::stats::Stats;
use anyhow::{Context, Result};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use talkee_proto::crypto::proof::SessionId;
use talkee_proto::crypto::session::{OpenError, Session};
use talkee_proto::AudioHeader;

/// Largest plausible sequence gap before we assume the sender restarted.
///
/// At 10 ms frames this is 10 seconds. Anything beyond it is far more likely to
/// be a new session than a 10-second hole in one, and counting it as loss would
/// poison the statistic with a garbage number.
const MAX_PLAUSIBLE_GAP: i32 = 1000;

/// How many rejected datagrams to describe on stderr before going quiet.
///
/// The counter keeps counting. A wrong port or a port scan would otherwise flood
/// the console with the same line, and the log stops being read exactly when it
/// matters.
const MAX_REPORTED_REJECTIONS: u64 = 5;

/// How long the socket blocks before re-checking the shutdown flag.
const RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// How often to print a health line while receiving.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(5);

/// What the receiver does with audio that is not sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityPolicy {
    /// Accept only frames sealed with the key from a pairing that verified.
    ///
    /// Selected by `--secure` or `--key`. A virtual microphone on a shared network must
    /// not accept audio from anyone who can reach the port: an unencrypted datagram is
    /// not "probably fine", it has no sender identity at all.
    ///
    /// It is *not* the default yet, and that is a deliberate, temporary trade — see
    /// [`AllowPlaintext`](Self::AllowPlaintext).
    RequireEncrypted,
    /// Accept plaintext frames from anyone, as M0 did.
    ///
    /// The default, because the phone app cannot pair yet, and a host that demanded a
    /// pairing it can never receive would be a working-looking screen with a dead
    /// microphone. It is also what the latency measurement path uses. The host states
    /// which of the two is in force on startup, so the trade is at least visible.
    AllowPlaintext,
}

/// A pairing session that the control channel installs into a running receiver.
///
/// The socket lives on the receive thread and the pairing handshake on another, so
/// the key has to cross between them. A mutex around an `Option<Session>` is the
/// whole mechanism: it is locked once per frame on the *receive* thread, never on
/// the audio thread, and it is uncontended except for the instant a pairing lands.
#[derive(Clone, Default)]
pub struct SharedSession(Arc<Mutex<Option<Session>>>, Arc<AtomicU64>);

impl SharedSession {
    /// Creates an empty handle: paired with nobody, accepting nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a session that has just been authenticated over the control
    /// channel, replacing any previous one.
    ///
    /// Replacing is deliberate. A new session id means a new key, so the replay
    /// high-water mark starts again from this sender's first frame — which is what
    /// lets a phone that restarted, and therefore resumed at `seq = 0`, be heard.
    /// Frames from the old key can no longer be opened at all.
    pub fn install(&self, session: Session) {
        // Bumped before the session is visible: the receive path compares this
        // against the epoch it last saw and resets its sequence tracking when it
        // changes, so it must never observe the new key with the old expectation.
        self.1.fetch_add(1, Ordering::Relaxed);
        *self.lock() = Some(session);
    }

    /// How many times a session has been installed, so the receive path can tell
    /// that the sender on the far end is a different one than it was tracking.
    pub fn epoch(&self) -> u64 {
        self.1.load(Ordering::Relaxed)
    }

    /// Forgets the session. Audio stops being accepted until the next pairing.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// Forgets the session, but only if it is still the one named.
    ///
    /// The check is not pedantry: a phone that reconnected installs a new session,
    /// and the moment its *old* connection is reaped it would otherwise clear the
    /// session its new connection had just established — an intermittent, load
    /// dependent silence that would be miserable to diagnose.
    ///
    /// Returns whether the session was actually dropped.
    pub fn clear_if(&self, id: SessionId) -> bool {
        let mut guard = self.lock();
        if guard.as_ref().map(|session| session.id()) == Some(id) {
            *guard = None;
            true
        } else {
            false
        }
    }

    /// True once a pairing has verified and not been cleared.
    pub fn is_paired(&self) -> bool {
        self.lock().is_some()
    }

    /// The hex id of the current session, for the health line and the log.
    pub fn session_id(&self) -> Option<String> {
        self.lock().as_ref().map(|session| session.id().to_hex())
    }

    /// The session, for the receive path.
    ///
    /// Poisoning is recovered rather than propagated: the guarded value is a single
    /// assignment, so there is no half-updated invariant to protect, and turning
    /// one thread's panic into the receive thread's panic would take the microphone
    /// down as well.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Session>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Receiver settings, resolved from the command line.
#[derive(Debug, Clone, Copy)]
pub struct ReceiverConfig {
    /// Address to bind. Port 0 lets the OS choose, which the tests rely on.
    pub listen: SocketAddr,
    /// Sample rate of the output endpoint, in Hz. The resampler targets this.
    pub device_rate: u32,
    /// Linear gain applied to every incoming frame, before resampling.
    pub gain: f32,
    /// Lift a quiet sender toward [`crate::level::TARGET_PEAK`].
    ///
    /// On by default, because the sender is usually a phone whose microphone this program has no
    /// other control over. Off is for measuring the signal exactly as it arrives — calibration,
    /// and working out whether a level problem is the sender's or ours.
    pub auto_level: bool,
    /// Depth the drift controller aims to hold, in frames at `device_rate`.
    ///
    /// Zero disables the controller and leaves M0's behaviour: the queue is bounded by
    /// its capacity and the oldest frames are thrown away when it fills. Worth having
    /// as an escape hatch, because "does this loop actually help" is a question that
    /// should be answerable by measurement rather than by argument.
    pub target_frames: usize,
    /// What to do with audio that is not sealed. See [`SecurityPolicy`].
    pub security: SecurityPolicy,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:47801".parse().expect("static address is valid"),
            device_rate: 48_000,
            gain: 1.0,
            target_frames: 0,
            auto_level: true,
            // Secure by default. A caller that wants M0's behaviour has to say so
            // explicitly, because the failure mode of the other default is a
            // stranger's audio coming out of the user's microphone.
            security: SecurityPolicy::RequireEncrypted,
        }
    }
}

/// What to do with a datagram, based on its sequence number.
///
/// Pulled out as a pure function because wrapping sequence arithmetic is easy to
/// get subtly wrong and awkward to test through a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqOutcome {
    /// First datagram of a session; establishes the baseline.
    First,
    /// Exactly what we expected next.
    Expected,
    /// A gap: this many frames were never received.
    Skipped(u32),
    /// Older than what we have already accepted: duplicate or reordered.
    Late,
    /// Too far ahead to be a gap; treat it as a new stream.
    Resync,
}

/// Classifies an incoming sequence number against the expected next one.
///
/// Uses `wrapping_sub`, so a sender that runs long enough to wrap from
/// `u32::MAX` back to `0` is seen as `Expected` rather than as a four-billion
/// frame gap.
pub fn classify_seq(next: Option<u32>, seq: u32) -> SeqOutcome {
    match next {
        None => SeqOutcome::First,
        Some(expected) => {
            let diff = seq.wrapping_sub(expected) as i32;
            if diff == 0 {
                SeqOutcome::Expected
            } else if diff > 0 && diff <= MAX_PLAUSIBLE_GAP {
                SeqOutcome::Skipped(diff as u32)
            } else if diff > 0 {
                SeqOutcome::Resync
            } else {
                SeqOutcome::Late
            }
        }
    }
}

/// Largest absolute sample in `frames`, for a level meter.
///
/// Not clamped: a meter that silently reports 1.0 for anything louder is a meter that
/// cannot tell a user their input is clipping, which is the one thing it is for.
fn peak(frames: &[f32]) -> f32 {
    frames.iter().fold(0.0f32, |loudest, sample| loudest.max(sample.abs()))
}

/// The M0 UDP audio receiver.
pub struct Receiver {
    socket: UdpSocket,
    config: ReceiverConfig,
    stats: Arc<Stats>,
    queue: Arc<SharedQueue>,
    resampler: LinearResampler,
    /// Rate the resampler was built for, so we can rebuild if the sender changes.
    resampler_source_rate: u32,
    /// Arrival pattern of the current health-line window. See [`crate::meter`]
    /// for why the health line reports a window rather than a session average.
    window: ArrivalWindow,
    /// Holds the output queue at `config.target_frames`. `None` when disabled.
    controller: Option<DepthController>,
    /// The resampler's ratio before any drift correction: output rate over the
    /// sender's rate. Kept so the controller can be applied as a *scale* on it
    /// rather than replacing it, which would lose the nominal rate conversion.
    base_ratio: f64,
    /// When the controller last ran, so it is fed at [`UPDATE_INTERVAL_MS`] and not
    /// once per audio frame.
    last_control: Instant,
    /// The automatic level stage, so a quiet phone arrives at a usable level. See [`crate::level`].
    level: AutoLevel,
    /// Removes what the PC itself is playing from the phone's microphone. `None` when off.
    echo: Option<crate::echo::EchoCanceller>,
    /// Settings the tray, the dashboard and the phone can change while this runs.
    controls: Option<crate::live::LiveControls>,
    /// The version of `controls` already applied, so a change is acted on once.
    controls_seen: u64,
    next_seq: Option<u32>,
    /// The pairing the control channel established, if any. `None` means this
    /// receiver was never told about a control channel, which happens in tests and
    /// in the plaintext path.
    session: Option<SharedSession>,
    /// Which pairing [`Receiver::session`] was last seen holding, so a new one can
    /// reset the sequence tracking. See `SharedSession::epoch`.
    session_epoch: u64,
    datagram: Vec<u8>,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    last_log: Instant,
}

impl Receiver {
    /// Binds the socket and prepares the pipeline.
    ///
    /// The receive buffer is enlarged because a 10 ms frame cadence leaves very
    /// little slack for a scheduling hiccup; the default Windows buffer is small
    /// enough to drop datagrams in the kernel, before we ever see them.
    pub fn bind(
        config: ReceiverConfig,
        stats: Arc<Stats>,
        queue: Arc<SharedQueue>,
    ) -> Result<Self> {
        let socket = UdpSocket::bind(config.listen)
            .with_context(|| format!("binding UDP {}", config.listen))?;
        // NOTE (M0): we would like to enlarge SO_RCVBUF here, because a 10 ms
        // frame cadence leaves little slack for a scheduling stall. `std` cannot
        // set socket options, and the Windows default (~64 KiB) is worth about
        // 330 ms at raw-PCM rates — comfortably more than our queue depth — so we
        // accept the default for now and revisit with `socket2` in M1, when Opus
        // makes the numbers small enough that kernel drops would actually bite.
        socket
            .set_read_timeout(Some(RECV_TIMEOUT))
            .context("setting socket read timeout")?;
        let resampler = LinearResampler::new(48_000, config.device_rate);
        Ok(Self {
            socket,
            config,
            stats,
            queue,
            resampler,
            resampler_source_rate: 48_000,
            window: ArrivalWindow::new(),
            controller: (config.target_frames > 0)
                .then(|| DepthController::new(config.target_frames, config.device_rate)),
            base_ratio: config.device_rate.max(1) as f64 / 48_000.0,
            last_control: Instant::now(),
            level: AutoLevel::new(),
            echo: None,
            controls: None,
            controls_seen: 0,
            next_seq: None,
            session: None,
            session_epoch: 0,
            datagram: vec![0u8; talkee_proto::HEADER_LEN + talkee_proto::MAX_PAYLOAD_LEN],
            mono: Vec::with_capacity(4096),
            resampled: Vec::with_capacity(8192),
            last_log: Instant::now(),
        })
    }

    /// Turns on echo cancellation. The caller keeps the [`crate::echo::RenderFeed`] and pushes what
    /// the PC is playing into it; this receiver runs the phone's audio through the canceller.
    pub fn set_echo_canceller(&mut self, echo: crate::echo::EchoCanceller) {
        self.echo = Some(echo);
    }

    /// Hands the receiver the shared live settings. Without them it behaves as it was configured.
    pub fn set_controls(&mut self, controls: crate::live::LiveControls) {
        self.controls = Some(controls);
    }

    /// Applies any change to the live settings. One atomic load when nothing changed.
    fn sync_controls(&mut self) {
        let Some(controls) = self.controls.as_ref() else {
            return;
        };
        let version = controls.version();
        if version == self.controls_seen {
            return;
        }
        self.controls_seen = version;
        if let Some(echo) = self.echo.as_mut() {
            echo.configure(
                controls.get(crate::live::Setting::EchoCancel),
                controls.get(crate::live::Setting::NoiseSuppression),
            );
        }
    }

    /// The echo canceller's measurements, when it is on.
    pub fn echo_stats(&self) -> Option<crate::echo::EchoStats> {
        self.echo.as_ref().map(|echo| echo.stats())
    }

    /// The address actually bound, which matters when `listen` used port 0.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket.local_addr().context("reading local address")
    }

    /// Receives until `shutdown` is set.
    ///
    /// Socket errors are logged rather than fatal: a Wi-Fi roam makes the socket
    /// unusable for a moment, and the M0 spike would rather keep the stream
    /// alive and recover than exit.
    pub fn run(&mut self, shutdown: &AtomicBool) -> Result<()> {
        while !shutdown.load(Ordering::Relaxed) {
            match self.socket.recv_from(&mut self.datagram) {
                Ok((len, _from)) => {
                    // `mem::take` moves the buffer out so we can pass a slice of
                    // it while borrowing `self` mutably. O(1), no allocation.
                    let buffer = std::mem::take(&mut self.datagram);
                    self.ingest(&buffer[..len]);
                    self.datagram = buffer;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.maybe_announce();
                }
                Err(e) => {
                    // ConnectionReset is normal on Windows once a sender goes
                    // away (an ICMP port-unreachable surfaces this way), and is
                    // not worth tearing the stream down for.
                    if e.kind() != std::io::ErrorKind::ConnectionReset {
                        eprintln!("talkee: socket error: {e}");
                    }
                }
            }
        }
        Ok(())
    }
}

impl Receiver {
    /// Installs the pairing handle the control channel fills in.
    ///
    /// Kept separate from [`Receiver::bind`] because the pairing arrives *after*
    /// the socket is listening: a host that waited for a phone before opening its
    /// audio port would have a chicken-and-egg problem on every restart.
    pub fn attach_session(&mut self, session: SharedSession) {
        self.session = Some(session);
    }

    /// Counts a rejected datagram, printing the first few reasons.
    ///
    /// Every rejection path goes through here so that "how many datagrams did we
    /// throw away, and does the log say why" has one answer rather than five.
    fn reject(&self, reason: impl std::fmt::Display) {
        let rejections = self.stats.parse_errors.fetch_add(1, Ordering::Relaxed) + 1;
        if rejections <= MAX_REPORTED_REJECTIONS {
            eprintln!("talkee: dropping datagram: {reason}");
        }
    }

    /// Opens a sealed frame, or counts it as dropped and returns `None`.
    ///
    /// The header is the associated data, so every field in it — sample rate,
    /// channel count, format, flags, `seq`, and the payload length itself — is
    /// authenticated along with the audio. An attacker who edits a header byte in
    /// flight gets a tag failure, not a re-labelled frame.
    fn open_sealed(&self, header: &AudioHeader, datagram: &[u8]) -> Option<Vec<u8>> {
        let unpaired = || "no pairing has verified yet, so there is no key for this frame";
        let Some(shared) = self.session.as_ref() else {
            self.reject(unpaired());
            return None;
        };
        let mut guard = shared.lock();
        let Some(session) = guard.as_mut() else {
            self.reject(unpaired());
            return None;
        };
        match session.open(
            header.seq,
            &datagram[..talkee_proto::HEADER_LEN],
            &datagram[talkee_proto::HEADER_LEN..],
        ) {
            Ok(plaintext) => Some(plaintext),
            Err(error) => {
                match error {
                    // A replay, a duplicate or a reordered frame is ordinary
                    // network behaviour and is counted the way the plaintext path
                    // counts one, not as a parse error.
                    OpenError::Replay { .. } => {
                        self.stats.frames_late.fetch_add(1, Ordering::Relaxed);
                    }
                    // Anything else is either a bug or a forgery, and both are worth
                    // seeing: a tampered packet must not look like an empty stream.
                    other => self.reject(other),
                }
                None
            }
        }
    }

    /// Parses and routes one datagram.
    ///
    /// Public so tests (and future callers with their own transport, such as
    /// TCP-over-ADB in M4) can push datagrams in without a socket.
    pub fn ingest(&mut self, datagram: &[u8]) {
        self.sync_controls();
        let header = match AudioHeader::from_bytes(datagram) {
            Ok(header) => header,
            Err(error) => {
                self.reject(error);
                return;
            }
        };

        // The security gate runs before anything else reads the payload or moves any
        // state, so a datagram that is not authentic cannot change the stream at all
        // — not the jitter buffer, not the loss counters, not the meter. An
        // unauthenticated frame is indistinguishable from noise, and is treated as
        // such.
        let sealed;
        let payload: &[u8] = if header.is_encrypted() {
            match self.open_sealed(&header, datagram) {
                Some(plaintext) => {
                    sealed = plaintext;
                    &sealed
                }
                None => return,
            }
        } else if self.config.security == SecurityPolicy::RequireEncrypted {
            self.reject(
                "unencrypted audio, and this receiver is pairing with a phone. Start it with \
                 --no-pairing to accept plaintext, or scan its pairing code so the sender can seal",
            );
            return;
        } else {
            &datagram[talkee_proto::HEADER_LEN..]
        };

        // A new pairing means a new sender session: a fresh key, and sequence
        // numbers that start again from zero. Without this reset, the first ten
        // seconds of a phone that reconnected would be thrown away as thousands of
        // late frames, and the user would see a working pairing and a silent
        // microphone.
        if header.is_encrypted() {
            if let Some(shared) = self.session.as_ref() {
                let epoch = shared.epoch();
                if epoch != self.session_epoch {
                    // Only worth saying out loud when there *was* a previous sender:
                    // the first pairing of a run resets nothing that matters.
                    let reconnected = self.session_epoch != 0;
                    self.session_epoch = epoch;
                    if reconnected {
                        println!(
                            "talkee: a new pairing is in use; treating the stream as restarted"
                        );
                    }
                    self.next_seq = None;
                    self.queue.clear();
                    self.resampler.reset();
                    self.window = ArrivalWindow::new();
                }
            }
        }

        match classify_seq(self.next_seq, header.seq) {
            SeqOutcome::First => {
                println!("talkee: first datagram received, stream is live");
                self.next_seq = Some(header.seq.wrapping_add(1));
            }
            SeqOutcome::Expected => {
                self.next_seq = Some(header.seq.wrapping_add(1));
            }
            SeqOutcome::Skipped(missing) => {
                self.stats.frames_lost.fetch_add(missing as u64, Ordering::Relaxed);
                self.next_seq = Some(header.seq.wrapping_add(1));
            }
            SeqOutcome::Late => {
                self.stats.frames_late.fetch_add(1, Ordering::Relaxed);
                return;
            }
            SeqOutcome::Resync => {
                println!("talkee: sequence jumped far ahead, treating this as a new stream");
                self.next_seq = Some(header.seq.wrapping_add(1));
                self.queue.clear();
                self.resampler.reset();
                // A window straddling a restart would report a rate for two
                // different streams at once, so it goes with the queue.
                self.window = ArrivalWindow::new();
            }
        }

        self.stats.datagrams.fetch_add(1, Ordering::Relaxed);

        // Muted from the PC, the tray or the phone. The datagram has been authenticated and counted —
        // the phone is still there — but nothing it carries reaches the cable.
        if self
            .controls
            .as_ref()
            .is_some_and(|c| c.get(crate::live::Setting::Paused))
        {
            self.stats.set_input_level(0.0);
            return;
        }

        // Rebuild the resampler if the sender's rate changed under us.
        if header.sample_rate != self.resampler_source_rate {
            println!(
                "talkee: sender rate changed {} -> {} Hz, rebuilding resampler",
                self.resampler_source_rate, header.sample_rate
            );
            self.resampler = LinearResampler::new(header.sample_rate, self.config.device_rate);
            self.resampler_source_rate = header.sample_rate;
            self.base_ratio =
                self.config.device_rate.max(1) as f64 / header.sample_rate.max(1) as f64;
            // Re-apply the correction the loop had already found for the *old* rate, so
            // a rate change does not throw away a drift estimate that took a minute to
            // converge. It will re-settle on its own within a minute either way.
            if let Some(controller) = self.controller.as_ref() {
                self.resampler
                    .set_ratio(self.base_ratio * (1.0 + controller.correction()));
            }
            // The arrival window is deliberately not reset here: the change is
            // announced on stdout, and at most one 5 s window can straddle it.
        }

        self.mono.clear();
        let frames = header.decode_pcm_mono(payload, &mut self.mono);
        if frames == 0 {
            return;
        }
        self.stats.samples_in.fetch_add(frames as u64, Ordering::Relaxed);
        self.window.observe(frames);

        // First, on the signal exactly as the phone captured it: the canceller predicts the echo
        // from the reference, and any gain applied before this point would make the prediction
        // wrong by that factor. Only meaningful at the rate it works at.
        if header.sample_rate == crate::echo::RATE {
            if let Some(echo) = self.echo.as_mut() {
                echo.process(&mut self.mono);
            }
        }

        if (self.config.gain - 1.0).abs() > f32::EPSILON {
            for sample in self.mono.iter_mut() {
                *sample *= self.config.gain;
            }
        }

        // Peak of the frame we are about to hand over, for the desktop level meter.
        // Measured here, after the gain, so the meter shows what the device will see.
        // The automatic level stage sits after the manual gain and before the meter, so the meter
        // shows what the device will receive rather than what happened to arrive.
        let auto_level = match self.controls.as_ref() {
            Some(c) => c.get(crate::live::Setting::AutoLevel),
            None => self.config.auto_level,
        };
        if auto_level {
            let frame_seconds = self.mono.len() as f32 / header.sample_rate.max(1) as f32;
            self.level
                .process(&mut self.mono, Duration::from_secs_f32(frame_seconds));
        }

        self.stats.set_input_level(peak(&self.mono));

        if self.resampler.is_identity() {
            // Skip a pointless copy when the sender and device rates agree.
            self.queue.push(&self.mono);
        } else {
            self.resampled.clear();
            self.resampler.process(&self.mono, &mut self.resampled);
            self.queue.push(&self.resampled);
        }

        self.apply_control();
        self.maybe_announce();
    }

    /// Drives the drift controller, if one is configured.
    ///
    /// Runs on the receive thread, which is allowed to take the queue lock, so the
    /// audio callback never has to. Two things come back from it: a scale for the
    /// resampler's ratio, and — when a burst has to be shed rather than absorbed — a
    /// count of frames to discard.
    fn apply_control(&mut self) {
        let Some(controller) = self.controller.as_mut() else {
            return;
        };
        let elapsed = self.last_control.elapsed();
        if elapsed < Duration::from_millis(UPDATE_INTERVAL_MS as u64) {
            return;
        }
        self.last_control = Instant::now();

        let correction = controller.update(self.queue.len(), elapsed);
        if correction.drop_oldest > 0 {
            self.queue.drop_oldest(correction.drop_oldest);
        }
        self.resampler
            .set_ratio(self.base_ratio * correction.ratio_scale);
    }

    /// Prints a periodic health line. This is the M0 instrument panel.
    ///
    /// Every rate on this line describes the **window since the previous line**,
    /// not the session, so a run with one bad patch is still readable afterwards.
    /// See [`crate::meter`] for the measurement that shaped it and
    /// `docs/LATENCY.md` for how to read the columns.
    fn maybe_announce(&mut self) {
        if self.last_log.elapsed() < ANNOUNCE_EVERY {
            return;
        }
        self.last_log = Instant::now();
        let s = self.stats.snapshot();
        let device_rate = self.config.device_rate.max(1) as f32;

        // Rates print as `--` rather than as a number when nothing arrived in the
        // window: a confident `0 ppm` would read like a healthy stream.
        let nominal = self.resampler_source_rate;
        let (dgram_rate, frame_rate, frame_size, ppm) = match self.window.close(nominal) {
            Some(r) => (
                format!("{:>6.1}", r.datagrams_per_second),
                format!("{:>7.0}", r.frames_per_second),
                format!("{:>4.0}", r.frames_per_datagram),
                format!("{:>+8.1}", r.ppm),
            ),
            None => (
                "    --".to_string(),
                "     --".to_string(),
                "  --".to_string(),
                "      --".to_string(),
            ),
        };
        // How much quieter the echo canceller is making the speakers' sound, in dB. `off` when there
        // is no canceller and `--` when it has had no echo to measure yet.
        let echo = match self.echo.as_ref().map(|e| e.stats()) {
            None => "off".to_string(),
            Some(stats) => match (stats.net_db, stats.erle_db) {
                (Some(net), Some(erle)) => format!("net {net:.1} dB (aec {erle:.1})"),
                (Some(net), None) => format!("net {net:.1} dB"),
                _ => "--".to_string(),
            },
        };
        // The correction the drift loop is applying, in ppm. `--` when there is no
        // loop. This is the column that says what is being done about `drift`.
        let ctrl = match self.controller.as_ref() {
            Some(c) => format!("{:>+8.1}", c.correction() * 1e6),
            None => "      --".to_string(),
        };
        // How hard the level stage is working. Like `ctrl` for drift, this is the number that says
        // what is being *done* about the level rather than what the level is.
        let gain = match self.config.auto_level {
            true => format!("{:>5.1}", self.level.gain()),
            false => "   --".to_string(),
        };

        println!(
            "recv {:>7} dgram | {dgram_rate} dgram/s | {frame_rate} frames/s | {frame_size} frames/dgram | loss {:>6.2}% ({}) | late {:>5} | slip {:>6} | trim {:>6} | under {:>5} | busy {:>4} | queue {:>6.1} ms | gain {gain}x | ctrl {ctrl} ppm | drift {ppm} ppm | echo {echo}",
            s.datagrams,
            s.loss_ratio() * 100.0,
            s.frames_lost,
            s.frames_late,
            s.slip_drops,
            s.trim_drops,
            s.underruns,
            s.callback_busy,
            self.queue.len() as f32 * 1000.0 / device_rate,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use talkee_proto::crypto::session::{Direction, Session};
    use talkee_proto::crypto::{Psk, SessionId};
    use talkee_proto::{flags, SampleFormat, HEADER_LEN};

    fn config(device_rate: u32) -> ReceiverConfig {
        ReceiverConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            device_rate,
            gain: 1.0,
            // Left off in tests that are about framing and resampling: automatic gain would move
            // the samples they assert on, and this module already has `level::tests` for the gain.
            auto_level: false,
            target_frames: 0,
            // The tests below send plaintext on purpose, so that the M0 pipeline
            // (resampling, drift, queueing) stays covered while M2 adds the
            // authenticated path alongside it.
            security: SecurityPolicy::AllowPlaintext,
        }
    }

    /// Builds a valid single-channel f32 datagram carrying `samples` copies of
    /// `value`.
    fn datagram(seq: u32, samples: u16, rate: u32, value: f32) -> Vec<u8> {
        let header = AudioHeader::new(seq, 0, rate, samples, 1, SampleFormat::F32, 0);
        let mut buf = header.to_bytes().to_vec();
        assert_eq!(buf.len(), HEADER_LEN);
        for _ in 0..samples {
            buf.extend_from_slice(&value.to_le_bytes());
        }
        buf
    }

    fn receiver(device_rate: u32) -> (Receiver, Arc<Stats>, Arc<SharedQueue>) {
        let stats = Arc::new(Stats::new());
        let queue = Arc::new(SharedQueue::new(device_rate as usize, Arc::clone(&stats)));
        let rx = Receiver::bind(config(device_rate), Arc::clone(&stats), Arc::clone(&queue))
            .expect("binding an ephemeral localhost port must work");
        (rx, stats, queue)
    }

    #[test]
    fn in_order_datagrams_land_in_the_queue() {
        let (mut rx, stats, queue) = receiver(48_000);
        for seq in 0..3 {
            rx.ingest(&datagram(seq, 480, 48_000, 0.25));
        }
        assert_eq!(queue.len(), 1440, "three frames of 480 must all be queued");
        let s = stats.snapshot();
        assert_eq!(s.datagrams, 3);
        assert_eq!(s.samples_in, 1440);
        assert_eq!(s.frames_lost, 0);
        assert_eq!(s.frames_late, 0);
        assert_eq!(s.parse_errors, 0);
    }

    #[test]
    fn duplicate_datagram_is_counted_and_dropped() {
        let (mut rx, stats, queue) = receiver(48_000);
        rx.ingest(&datagram(0, 480, 48_000, 0.5));
        rx.ingest(&datagram(1, 480, 48_000, 0.5));
        let after_two = queue.len();
        rx.ingest(&datagram(1, 480, 48_000, 0.5));
        assert_eq!(queue.len(), after_two, "a duplicate must not add audio");
        assert_eq!(stats.snapshot().frames_late, 1);
    }

    #[test]
    fn gap_is_counted_as_lost_frames() {
        let (mut rx, stats, _queue) = receiver(48_000);
        rx.ingest(&datagram(0, 480, 48_000, 0.1));
        rx.ingest(&datagram(5, 480, 48_000, 0.1)); // 1..=4 never arrived
        assert_eq!(stats.snapshot().frames_lost, 4);
    }

    #[test]
    fn absurd_gap_resyncs_instead_of_billing_phantom_loss() {
        let (mut rx, stats, queue) = receiver(48_000);
        rx.ingest(&datagram(0, 480, 48_000, 0.1));
        assert!(!queue.is_empty(), "the first frame must be queued");
        rx.ingest(&datagram(100_000, 480, 48_000, 0.1));
        let s = stats.snapshot();
        assert_eq!(s.frames_lost, 0, "a resync must not be billed as loss");
        assert_eq!(queue.len(), 480, "the stale queue must have been cleared");
    }

    #[test]
    fn classify_seq_covers_the_basic_cases() {
        assert_eq!(classify_seq(None, 42), SeqOutcome::First);
        assert_eq!(classify_seq(Some(7), 7), SeqOutcome::Expected);
        assert_eq!(classify_seq(Some(7), 8), SeqOutcome::Skipped(1));
        assert_eq!(classify_seq(Some(7), 9), SeqOutcome::Skipped(2));
        assert_eq!(classify_seq(Some(7), 6), SeqOutcome::Late);
        assert_eq!(classify_seq(Some(7), 7 + 1000), SeqOutcome::Skipped(1000));
        assert_eq!(classify_seq(Some(7), 7 + 1001), SeqOutcome::Resync);
    }

    #[test]
    fn sequence_wrap_around_is_handled() {
        // Expecting u32::MAX next and receiving it is exactly on time.
        assert_eq!(classify_seq(Some(u32::MAX), u32::MAX), SeqOutcome::Expected);
        // Receiving 0 when we expected u32::MAX means u32::MAX itself was lost.
        assert_eq!(classify_seq(Some(u32::MAX), 0), SeqOutcome::Skipped(1));
        // Receiving u32::MAX when we expected 0 is a very old packet, not a
        // four-billion-frame gap.
        assert_eq!(classify_seq(Some(0), u32::MAX), SeqOutcome::Late);
    }

    #[test]
    fn malformed_datagrams_are_counted_not_processed() {
        let (mut rx, stats, queue) = receiver(48_000);
        rx.ingest(&[0u8; 4]); // short
        rx.ingest(&[0xFFu8; HEADER_LEN]); // bad magic
        assert!(queue.is_empty());
        assert_eq!(stats.snapshot().parse_errors, 2);
        assert_eq!(stats.snapshot().datagrams, 0);
    }

    #[test]
    fn resampler_is_applied_when_the_device_rate_differs() {
        // 48 kHz in, 44.1 kHz out: 480 frames in must yield ~441 frames out.
        let (mut rx, _stats, queue) = receiver(44_100);
        rx.ingest(&datagram(0, 480, 48_000, 0.5));
        let produced = queue.len();
        assert!(
            (430..=445).contains(&produced),
            "expected ~441 frames after downsampling, got {produced}"
        );
    }

    #[test]
    fn gain_is_applied_before_resampling() {
        let stats = Arc::new(Stats::new());
        let queue = Arc::new(SharedQueue::new(48_000, Arc::clone(&stats)));
        let mut cfg = config(48_000);
        cfg.gain = 0.5;
        let mut rx = Receiver::bind(cfg, Arc::clone(&stats), Arc::clone(&queue)).unwrap();
        rx.ingest(&datagram(0, 4, 48_000, 0.8));
        let mut out = [1.0f32; 4];
        queue.fill(&mut out);
        assert!(
            out.iter().all(|&s| (s - 0.4).abs() < 1e-6),
            "expected 0.8 * 0.5 = 0.4, got {out:?}"
        );
    }

    #[test]
    fn sender_rate_change_rebuilds_the_resampler() {
        let (mut rx, _stats, queue) = receiver(48_000);
        rx.ingest(&datagram(0, 480, 48_000, 0.1));
        let after_48k = queue.len();
        assert_eq!(after_48k, 480, "48 kHz in, 48 kHz out is a passthrough");

        // The sender switches to 16 kHz. 480 frames at 16 kHz is 30 ms, which at
        // the 48 kHz device rate must become ~1440 frames.
        rx.ingest(&datagram(1, 480, 16_000, 0.1));
        let produced = queue.len() - after_48k;
        assert!(
            (1400..=1480).contains(&produced),
            "expected ~1440 frames after upsampling 16k -> 48k, got {produced}"
        );
    }

    #[test]
    fn frames_survive_the_pipeline_intact() {
        // End to end: build a ramp, ingest it, read it back out.
        let (mut rx, _stats, queue) = receiver(48_000);
        let samples = 8u16;
        let header = AudioHeader::new(0, 0, 48_000, samples, 1, SampleFormat::I16, 0);
        let mut buf = header.to_bytes().to_vec();
        for i in 0..samples {
            let v = ((i as f32 / samples as f32) * 16384.0) as i16;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        rx.ingest(&buf);

        let mut out = [0.0f32; 8];
        let outcome = queue.fill(&mut out);
        assert_eq!(outcome.provided, 8);
        assert!(!outcome.underran());
        for (i, &s) in out.iter().enumerate() {
            let expected = ((i as f32 / samples as f32) * 16384.0) as i16 as f32 / 32768.0;
            assert!((s - expected).abs() < 1e-6, "frame {i}: {s} vs {expected}");
        }
    }

    /// A phone-to-desktop session keyed by a fixed byte, so a test can create two
    /// senders that do or do not share a key.
    fn session(key_byte: u8, id: [u8; 4]) -> Session {
        Session::new(
            &Psk::from_slice(&[key_byte; 32]).unwrap(),
            SessionId::from_bytes(id),
            Direction::PhoneToDesktop,
        )
    }

    /// Seals a frame exactly as a paired sender does: the 24-byte header is the
    /// associated data, so it is built first and sent unchanged.
    fn sealed_datagram(
        session: &mut Session,
        seq: u32,
        samples: u16,
        rate: u32,
        value: f32,
    ) -> Vec<u8> {
        let header = AudioHeader::new(
            seq,
            0,
            rate,
            samples,
            1,
            SampleFormat::F32,
            flags::ENCRYPTED,
        );
        let mut plaintext = Vec::with_capacity(samples as usize * 4);
        for _ in 0..samples {
            plaintext.extend_from_slice(&value.to_le_bytes());
        }
        let mut datagram = header.to_bytes().to_vec();
        let mut sealed = Vec::new();
        session.seal(seq, &datagram, &plaintext, &mut sealed).unwrap();
        datagram.extend_from_slice(&sealed);
        datagram
    }

    /// A receiver under an explicit policy, optionally already paired.
    fn secured_receiver(
        device_rate: u32,
        security: SecurityPolicy,
        paired: Option<Session>,
    ) -> (Receiver, Arc<Stats>, Arc<SharedQueue>) {
        let mut cfg = config(device_rate);
        cfg.security = security;
        let stats = Arc::new(Stats::new());
        let queue = Arc::new(SharedQueue::new(device_rate as usize, Arc::clone(&stats)));
        let mut rx = Receiver::bind(cfg, Arc::clone(&stats), Arc::clone(&queue))
            .expect("binding an ephemeral localhost port must work");
        let shared = SharedSession::new();
        if let Some(session) = paired {
            shared.install(session);
        }
        rx.attach_session(shared);
        (rx, stats, queue)
    }

    #[test]
    fn a_sealed_frame_from_a_paired_sender_is_decoded() {
        let mut sender = session(7, [1, 2, 3, 4]);
        let (mut rx, stats, queue) = secured_receiver(
            48_000,
            SecurityPolicy::RequireEncrypted,
            Some(session(7, [1, 2, 3, 4])),
        );
        rx.ingest(&sealed_datagram(&mut sender, 0, 480, 48_000, 0.5));
        assert_eq!(queue.len(), 480, "the decrypted audio must reach the queue");
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.datagrams, 1);
        assert_eq!(snapshot.parse_errors, 0);
        assert_eq!(snapshot.samples_in, 480);
    }

    #[test]
    fn plaintext_audio_is_refused_when_a_pairing_is_required() {
        let (mut rx, stats, queue) =
            secured_receiver(48_000, SecurityPolicy::RequireEncrypted, None);
        rx.ingest(&datagram(0, 480, 48_000, 0.5));
        assert!(queue.is_empty(), "an unpaired sender must not be heard");
        assert_eq!(stats.snapshot().parse_errors, 1);
        assert_eq!(stats.snapshot().datagrams, 0);
    }

    #[test]
    fn plaintext_audio_is_refused_even_by_a_receiver_that_is_already_paired() {
        // Otherwise a paired phone and a stranger on the same network could both
        // feed the microphone, and the pairing would only be decoration.
        let (mut rx, stats, queue) = secured_receiver(
            48_000,
            SecurityPolicy::RequireEncrypted,
            Some(session(7, [1, 2, 3, 4])),
        );
        rx.ingest(&datagram(0, 480, 48_000, 0.5));
        assert!(queue.is_empty());
        assert_eq!(stats.snapshot().parse_errors, 1);
    }

    #[test]
    fn a_sealed_frame_from_the_wrong_key_is_refused() {
        // The stranger's view of the network: it can see the frames, it just cannot
        // produce one that opens.
        let mut stranger = session(9, [4, 3, 2, 1]);
        let (mut rx, stats, queue) = secured_receiver(
            48_000,
            SecurityPolicy::RequireEncrypted,
            Some(session(7, [1, 2, 3, 4])),
        );
        rx.ingest(&sealed_datagram(&mut stranger, 0, 480, 48_000, 0.5));
        assert!(queue.is_empty(), "a wrong key must not produce audio");
        assert_eq!(stats.snapshot().parse_errors, 1);
        assert_eq!(stats.snapshot().datagrams, 0);
    }

    #[test]
    fn a_frame_sealed_for_another_session_is_refused() {
        // Same pairing key, different session id. The session id is covered by the
        // pairing proof and mixed into the key, which is what stops a recording of
        // one session from being replayed into the next.
        let mut other = session(7, [9, 9, 9, 9]);
        let (mut rx, stats, queue) = secured_receiver(
            48_000,
            SecurityPolicy::RequireEncrypted,
            Some(session(7, [1, 2, 3, 4])),
        );
        rx.ingest(&sealed_datagram(&mut other, 0, 480, 48_000, 0.5));
        assert!(queue.is_empty());
        assert_eq!(stats.snapshot().parse_errors, 1);
    }

    #[test]
    fn a_replayed_sealed_datagram_is_dropped_the_second_time() {
        let mut sender = session(7, [1, 2, 3, 4]);
        let (mut rx, stats, queue) = secured_receiver(
            48_000,
            SecurityPolicy::RequireEncrypted,
            Some(session(7, [1, 2, 3, 4])),
        );
        let frame = sealed_datagram(&mut sender, 5, 480, 48_000, 0.5);
        rx.ingest(&frame);
        let after_first = queue.len();
        assert!(after_first > 0, "the live frame must be heard");
        rx.ingest(&frame);
        assert_eq!(queue.len(), after_first, "a replayed frame must not add audio");
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.datagrams, 1, "the replay is not a second datagram");
        assert_eq!(snapshot.frames_late, 1, "a replay is counted as late");
        assert_eq!(snapshot.parse_errors, 0, "a replay is not a parse error");
    }

    #[test]
    fn a_tampered_sealed_datagram_is_dropped() {
        let mut sender = session(7, [1, 2, 3, 4]);
        let (mut rx, stats, queue) = secured_receiver(
            48_000,
            SecurityPolicy::RequireEncrypted,
            Some(session(7, [1, 2, 3, 4])),
        );
        let mut frame = sealed_datagram(&mut sender, 6, 480, 48_000, 0.5);
        let tag_byte = frame.len() - 1;
        frame[tag_byte] ^= 0x01;
        rx.ingest(&frame);
        assert!(queue.is_empty(), "a frame with a broken tag must not be heard");
        assert_eq!(stats.snapshot().parse_errors, 1);
    }

    #[test]
    fn editing_a_header_field_in_flight_breaks_the_frame() {
        // The header is the associated data, so this is what stops an attacker
        // relabelling a frame: 480 frames of 48 kHz cannot be passed off as anything
        // else now that the header is authenticated along with the audio.
        let mut sender = session(7, [1, 2, 3, 4]);
        let (mut rx, stats, queue) = secured_receiver(
            48_000,
            SecurityPolicy::RequireEncrypted,
            Some(session(7, [1, 2, 3, 4])),
        );
        let mut frame = sealed_datagram(&mut sender, 7, 480, 48_000, 0.5);
        frame[12] ^= 0x01; // sample_rate, at offset 12, is inside the AAD
        rx.ingest(&frame);
        assert!(queue.is_empty());
        assert_eq!(stats.snapshot().parse_errors, 1);

        // ...and the untampered frame is still accepted afterwards: one altered
        // datagram must not poison the stream, and a failed tag must not advance the
        // replay mark past a frame the sender really did send.
        let mut sender = session(7, [1, 2, 3, 4]);
        rx.ingest(&sealed_datagram(&mut sender, 7, 480, 48_000, 0.5));
        assert_eq!(queue.len(), 480);
    }

    #[test]
    fn the_default_policy_requires_encryption_and_a_pairing() {
        assert_eq!(
            ReceiverConfig::default().security,
            SecurityPolicy::RequireEncrypted,
            "an unpaired stranger must not be the default"
        );
        let shared = SharedSession::new();
        assert!(!shared.is_paired(), "a fresh handle is paired with nobody");
        assert_eq!(shared.session_id(), None);
        shared.install(session(1, [0, 0, 0, 1]));
        assert!(shared.is_paired());
        assert_eq!(shared.session_id().as_deref(), Some("00000001"));
        shared.clear();
        assert!(!shared.is_paired());
    }

    #[test]
    fn a_phone_that_pairs_again_can_start_its_sequence_numbers_over() {
        // What happens when the phone reconnects: a new session id, a new key, and
        // `seq` back to zero. Without the epoch reset every frame would look like a
        // four-thousand-frame-old duplicate, and the user would see a successful
        // pairing and a silent microphone.
        let mut first = session(7, [1, 2, 3, 4]);
        let shared = SharedSession::new();
        shared.install(session(7, [1, 2, 3, 4]));
        let mut cfg = config(48_000);
        cfg.security = SecurityPolicy::RequireEncrypted;
        let stats = Arc::new(Stats::new());
        let queue = Arc::new(SharedQueue::new(48_000, Arc::clone(&stats)));
        let mut rx = Receiver::bind(cfg, Arc::clone(&stats), Arc::clone(&queue)).unwrap();
        rx.attach_session(shared.clone());

        rx.ingest(&sealed_datagram(&mut first, 4_000, 480, 48_000, 0.5));
        assert_eq!(queue.len(), 480);

        let mut second = session(7, [5, 6, 7, 8]);
        shared.install(session(7, [5, 6, 7, 8]));
        rx.ingest(&sealed_datagram(&mut second, 0, 480, 48_000, 0.5));
        assert_eq!(queue.len(), 480, "the restarted stream must be heard");
        assert_eq!(stats.snapshot().frames_late, 0, "and not counted as late");
    }

    #[test]
    fn clearing_a_session_only_affects_the_session_named() {
        // A phone that reconnects installs a new session; the moment its old
        // connection is reaped, that old connection must not clear the new session.
        let shared = SharedSession::new();
        let first = SessionId::from_bytes([1, 2, 3, 4]);
        let second = SessionId::from_bytes([5, 6, 7, 8]);
        let key = Psk::from_slice(&[7u8; 32]).unwrap();
        assert!(!shared.clear_if(first), "there is nothing to clear yet");
        shared.install(Session::new(&key, first, Direction::PhoneToDesktop));
        shared.install(Session::new(&key, second, Direction::PhoneToDesktop));
        assert!(
            !shared.clear_if(first),
            "an older control connection must not clear a newer session"
        );
        assert!(shared.is_paired());
        assert!(shared.clear_if(second));
        assert!(!shared.is_paired());
    }

}



