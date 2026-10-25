//! The live session: cable probe, receiver, device, advertisement.
//!
//! # What belongs here, and what does not
//!
//! This module is the only impure thing in the app. It owns the `Arc<Stats>` the level
//! meter reads, the `Arc<SharedQueue>` the audio callback drains, the receiver thread
//! and the mDNS advertisement — and it hands the rest of the app a
//! [`state::Reading`], which is a plain value. Every decision about what the user
//! should be told lives in `state`, so it can be tested without a device.
//!
//! The startup sequence is deliberately the same as `talkee host`'s, in the same
//! order, because that order is load-bearing: the endpoint's real format is read
//! *before* the queue is created, since the queue has to be sized against the rate the
//! device actually runs at. Getting that backwards produces a queue of the wrong
//! length and a latency number that silently disagrees with reality.
//!
//! # Ownership rule
//!
//! The `Box<dyn VirtualMicWriter>` is not `Send`, and cpal's stream must be dropped on
//! the thread that opened it. So every device operation happens on the main thread,
//! driven by [`Engine::start`] and [`Engine::stop`]; only the socket lives on a worker.

use anyhow::{anyhow, Context, Result};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use talkee_core::control::serve_pairing;
use talkee_core::live::LiveControls;
use talkee_core::stream::{SecurityPolicy, SharedSession};
use talkee_core::{
    local_ipv4_addresses, Receiver, ReceiverConfig, SharedQueue, Stats, VirtualMicWriter,
};
use talkee_discovery as discovery;
use talkee_proto::crypto::{PairLink, Psk};
use talkee_store::Store;
use talkee_vmic as vmic;

use crate::state::{self, Observation, OutMeter, OutTracker, Reading};

/// How often the cable is looked for while no session is running.
///
/// A user who has just clicked through the VB-CABLE installer should see the app
/// notice on its own, and cpal's device enumeration is a WASAPI activation per
/// endpoint — too heavy to run every frame, cheap enough every two seconds.
const CABLE_REPROBE: Duration = Duration::from_secs(2);

/// How long to keep looking for our own mDNS advertisement before calling it unseen.
const MDNS_SELF_CHECK_WINDOW: Duration = Duration::from_secs(10);

/// Everything the app was told on the command line.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Address to receive audio on.
    pub listen: SocketAddr,
    /// Render endpoint to play into. `None` means "the first virtual cable found".
    pub device: Option<String>,
    /// Linear gain applied to incoming audio, before resampling.
    pub gain: f32,
    /// Depth of the output queue, in milliseconds. The latency-versus-dropouts dial.
    pub buffer_ms: u32,
    /// Requested device buffer size in frames. `None` uses the device default.
    pub buffer_frames: Option<u32>,
    /// Whether to announce this PC over mDNS.
    pub advertise: bool,
    /// Refuse unencrypted audio and require a paired phone — the default.
    pub no_pairing: bool,
    /// mDNS instance name, from `talkee-discovery`.
    pub instance: String,
    /// Remove what this PC is playing from the phone's microphone. See `talkee_core::echo`.
    pub echo_cancel: bool,
    /// Also run noise suppression after the canceller. On by default: it is what makes a quiet room
    /// sound clean. The cost is that under loud music it can gate quiet speech.
    pub noise_suppression: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:47801".parse().expect("static address is valid"),
            device: None,
            gain: 1.0,
            buffer_ms: 60,
            buffer_frames: None,
            advertise: true,
            no_pairing: false,
            instance: discovery::default_instance_name(),
            echo_cancel: true,
            noise_suppression: true,
        }
    }
}

impl EngineConfig {
    /// The security policy this configuration asks for.
    ///
    /// **Pairing is the default**, and it became the default the moment the phone could pair — which
    /// it now can. A microphone on a network the user does not own must not accept audio from anyone
    /// who can reach the port, and a QR code in the tray that nobody can see is not a feature.
    ///
    /// `--no-pairing` restores plaintext audio, for measurement and for a network the user controls.
    pub fn security(&self) -> SecurityPolicy {
        if self.no_pairing {
            SecurityPolicy::AllowPlaintext
        } else {
            SecurityPolicy::RequireEncrypted
        }
    }
}

/// Frames the output queue holds before playback starts, and the depth the drift
/// controller then holds it at.
///
/// One number used twice on purpose: prefill fills the queue to it and the controller
/// keeps it there, so changing the setting cannot make the two disagree.
pub fn target_frames(buffer_ms: u32, device_rate: u32) -> usize {
    ((buffer_ms as u64 * device_rate as u64) / 1000).max(1) as usize
}

/// Queue capacity for a given target depth.
///
/// Four times the depth, floored at 1024 frames, mirroring `talkee host`: this is the
/// guard that stops one burst from growing latency without bound, not a drift
/// strategy.
pub fn queue_capacity(target: usize) -> usize {
    (target * 4).max(1024)
}


/// What we know about the virtual-cable side of this machine.
///
/// # Seam for `talkee-vmic`'s richer status API
///
/// `talkee-vmic` is growing a richer cable-status API — presence with reasons, endpoint
/// health, a diagnosis for a cable that exists but will not open. When it lands,
/// [`CableStatus::probe`] is the *only* function in this crate that has to change:
/// everything downstream reads this struct's plain data and never a `vmic` type. Do not
/// grow a second device model next to it in the meantime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CableStatus {
    /// True when there is an endpoint this app can plausibly render into.
    pub installed: bool,
    /// The endpoint we will open, once we know it.
    pub endpoint: Option<String>,
    /// Curated family label, when the endpoint is one we recognise.
    pub family: Option<&'static str>,
    /// Substring the user must look for in other apps to find the fake microphone.
    pub capture_hint: Option<String>,
    /// Why there is no usable endpoint, in words the user can act on.
    pub problem: Option<String>,
}

impl CableStatus {
    /// Looks for a render endpoint to play into.
    ///
    /// `preferred` is the user's explicit `--device`; it is trusted as-is, because
    /// someone who names an endpoint has a reason to, and `inspect_render` is what
    /// proves it exists and can be opened.
    pub fn probe(preferred: Option<&str>) -> CableStatus {
        if let Some(name) = preferred {
            return CableStatus {
                installed: true,
                endpoint: Some(name.to_string()),
                family: None,
                capture_hint: None,
                problem: None,
            };
        }
        match vmic::find_virtual_cable() {
            Ok(Some((name, family))) => CableStatus {
                installed: true,
                endpoint: Some(name),
                family: Some(family.label),
                capture_hint: Some(family.capture_pattern.to_string()),
                problem: None,
            },
            Ok(None) => CableStatus {
                installed: false,
                endpoint: None,
                family: None,
                capture_hint: None,
                problem: Some(
                    "no virtual audio cable found. VB-CABLE is the one Talkee is allowed to \
                     install for you: https://vb-audio.com/Cable/"
                        .to_string(),
                ),
            },
            // Failing to enumerate endpoints is not the same as "no cable installed", and
            // the difference is exactly what a user needs to hear.
            Err(e) => CableStatus {
                installed: false,
                endpoint: None,
                family: None,
                capture_hint: None,
                problem: Some(format!("could not list audio devices: {e:#}")),
            },
        }
    }
}

/// What starting a session produced, for the startup banner.
#[derive(Debug, Clone)]
pub struct StartReport {
    /// Endpoint the audio is being played into.
    pub endpoint: String,
    /// Sample rate the endpoint actually runs at.
    pub device_rate: u32,
    /// Channel count the endpoint actually runs at.
    pub device_channels: u16,
    /// Debug name of the device's sample format.
    pub sample_format: String,
    /// Frames buffered before playback starts.
    pub target_frames: usize,
    /// Queue capacity in frames.
    pub queue_capacity: usize,
    /// Address audio is being received on.
    pub listen: SocketAddr,
    /// LAN addresses a phone could reach this PC on.
    pub lan_addresses: Vec<IpAddr>,
    /// Substring to look for when choosing the microphone in another app.
    pub capture_hint: Option<String>,
    /// The mDNS name others can see, when advertising worked.
    pub advertisement: Option<String>,
    /// Why advertising did not work, when it did not.
    pub advertisement_problem: Option<String>,
    /// The pairing link a phone scans, when this run is accepting pairings.
    pub pairing_link: Option<String>,
    /// What to say about the key: it is new, it was reused, or it had to be replaced.
    pub pairing_note: Option<String>,
}

/// The receive socket, running on its own thread.
///
/// The socket is on a worker because `Receiver::run` blocks between datagrams — at
/// most 200 ms per iteration, its own receive timeout. The tray's message pump must
/// keep draining, so the main thread can never wait on it.
struct ReceiverThread {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Failure from the worker, if it stopped early. Held rather than printed: a
    /// library must not write to stdout, and the app prints this as a state transition.
    error: Arc<Mutex<Option<String>>>,
}

impl ReceiverThread {
    /// Takes ownership of a bound receiver and starts draining the socket.
    fn spawn(mut receiver: Receiver) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&error);
        let handle = thread::spawn(move || {
            // `run` returns only when the flag is set or the socket is broken; both are
            // worth recording, and the second one is worth showing the user.
            if let Err(e) = receiver.run(&flag) {
                if let Ok(mut slot) = slot.lock() {
                    *slot = Some(format!("{e:#}"));
                }
            }
        });
        Self {
            shutdown,
            handle: Some(handle),
            error,
        }
    }

    /// Asks the worker to stop and waits for it.
    ///
    /// Bounded by the receiver's own 200 ms receive timeout, so this cannot hang the
    /// shutdown path for a noticeable time.
    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// The worker's failure, if it has one, once.
    fn take_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// Confirms that this machine can see its own mDNS advertisement.
///
/// # Why this is worth a thread
///
/// "Looking for your phone" is the state where the only question that matters is
/// whether the phone can find *this PC*, and mDNS is how it does that. A firewall that
/// swallows multicast is the most common reason a phone never connects, and it is
/// invisible from the sending side — except that the record we just published never
/// comes back to us. Seeing our own record is not proof that a phone will (different
/// stack, same network), but not seeing it is real evidence that something on this
/// machine is blocking discovery, which is worth saying in the one state where the user
/// is waiting for something to happen.
struct MdnsSelfCheck {
    seen: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MdnsSelfCheck {
    /// Starts looking for `instance` in the background.
    fn start(instance: String) -> Self {
        let seen = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&seen);
        let handle = thread::spawn(move || {
            let Ok(browser) = discovery::Browser::start() else {
                return;
            };
            let deadline = Instant::now() + MDNS_SELF_CHECK_WINDOW;
            while Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match browser.next(remaining.min(Duration::from_secs(1))) {
                    Some(host) if host.instance == instance => {
                        flag.store(true, Ordering::Relaxed);
                        return;
                    }
                    Some(_) => continue,
                    // No event inside the slice; keep listening until the deadline.
                    None => continue,
                }
            }
        });
        Self {
            seen,
            handle: Some(handle),
        }
    }

    /// True once our own advertisement has been seen on the network.
    fn saw_ourselves(&self) -> bool {
        self.seen.load(Ordering::Relaxed)
    }

    /// A phrase for the "looking for your phone" tooltip, or `None` while unknown.
    ///
    /// Deliberately says nothing until the check has had its chance: "not visible yet"
    /// during the first seconds would be a false alarm in the exact state where a false
    /// alarm is most expensive.
    fn hint(&self, checked_for: Duration) -> Option<&'static str> {
        if checked_for < MDNS_SELF_CHECK_WINDOW {
            return None;
        }
        if self.saw_ourselves() {
            Some("discoverable on this network")
        } else {
            Some("this PC is not visible to itself — a firewall may be blocking discovery")
        }
    }
}

impl Drop for MdnsSelfCheck {
    fn drop(&mut self) {
        // The browser daemon is dropped inside the thread when it returns; joining here
        // would block the shutdown path for up to the check window, so the thread is
        // simply left to finish. It holds no device.
        let _ = self.handle.take();
    }
}

/// Everything one tick of a live session produced.
struct Frame {
    observation: Observation,
    meter: OutMeter,
    lost: u64,
    underruns: u64,
    queue_frames: usize,
    error: Option<String>,
}

/// One running session: a device being fed, and a socket feeding it.
struct Session {
    stats: Arc<Stats>,
    queue: Arc<SharedQueue>,
    /// Held, not read: dropping this closes the device stream and silences the
    /// microphone. Same contract as `talkee host`'s `_writer`.
    _writer: Box<dyn VirtualMicWriter>,
    receiver: ReceiverThread,
    /// Held so the service is withdrawn when the session stops.
    _advertisement: Option<discovery::Advertisement>,
    /// Held so the speaker capture that feeds the echo canceller stops with the session.
    _loopback: Option<vmic::Loopback>,
    last_datagram_at: Option<Instant>,
    last_datagrams: u64,
    quiet_for: Duration,
    out: OutTracker,
    last_tick: Instant,
}

impl Session {
    /// Samples the shared counters once.
    fn tick(&mut self) -> Frame {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;

        let snapshot = self.stats.snapshot();
        let level_in = self.stats.input_level();
        let level_out = self.stats.output_level();

        // A datagram accepted since the previous tick is the only evidence that the level
        // below it is fresh rather than a leftover peak. The counter is the cheap way to
        // ask: everything else on this path is lock-free atomics.
        let fresh = snapshot.datagrams > self.last_datagrams;
        self.last_datagrams = snapshot.datagrams;
        if fresh {
            self.last_datagram_at = Some(now);
        }

        self.quiet_for = state::advance_quiet(self.quiet_for, fresh, level_in, elapsed);
        self.out
            .observe(level_out, state::level_cells(level_in) > 0, elapsed);

        let observation = Observation {
            cable_present: true,
            datagrams: snapshot.datagrams,
            since_last_datagram: self
                .last_datagram_at
                .map(|at| now.saturating_duration_since(at))
                .unwrap_or_default(),
            quiet_for: self.quiet_for,
            level_in,
            level_out,
            refused: snapshot.parse_errors,
        };
        Frame {
            observation,
            meter: self.out.meter(level_out),
            lost: snapshot.frames_lost,
            underruns: snapshot.underruns,
            queue_frames: self.queue.len(),
            error: self.receiver.take_error(),
        }
    }
}

/// One tick's worth of display state, plus anything the user needs to be told.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// What the tray should show.
    pub reading: Reading,
    /// True when no session is running. The four states describe a receiver and there is
    /// none, so the caller dims the icon rather than inventing a fifth state.
    pub stopped: bool,
    /// A one-off message to print, when something went wrong.
    pub problem: Option<String>,
    /// Extra words for the "looking for your phone" tooltip.
    pub hint: Option<String>,
}

/// The desktop app's runtime: cable, device, socket, advertisement.
pub struct Engine {
    config: EngineConfig,
    cable: CableStatus,
    last_cable_probe: Instant,
    session: Option<Session>,
    mdns: Option<MdnsSelfCheck>,
    mdns_started: Instant,
    problem: Option<String>,
    /// Whether we have already told the user that nothing publishes the output level.
    output_notice_sent: bool,
    /// Whether we have already told the user that arriving audio is being refused.
    refused_notice_sent: bool,
    /// When a start was last attempted, so a failing start is retried on an interval rather than on
    /// every tick. See [`START_RETRY`].
    last_start_attempt: Instant,
    /// The pairing channel: one per process, not one per session.
    pairing: Option<PairingChannel>,
    /// What the tray, the dashboard and the phone can change while this runs. One per process, so a
    /// setting survives a session restart and every surface sees the same value.
    controls: LiveControls,
}

/// The pairing channel, and everything about it that outlives one audio session.
///
/// Deliberately owned by the `Engine` rather than by the session: the control listener, the key and the
/// session handle all belong to the process. See [`Engine::ensure_pairing`] for what went wrong when
/// they belonged to the session instead.
struct PairingChannel {
    /// The handle the pairing thread installs sessions into, and the one every receiver attaches to.
    sessions: SharedSession,
    /// The link a phone scans. Held so the wizard can be handed it again without rebuilding it.
    link: String,
    /// What to say about the key: new, reused, or replaced.
    note: String,
}

/// How often the app will try to start a session that keeps failing.
///
/// Not every tick. A failure that persists — a device another program is holding, a port in use — would
/// otherwise print twenty lines a second and hammer whatever is refusing it. Three seconds is often
/// enough to recover from a device being released, and rare enough to read.
const START_RETRY: Duration = Duration::from_secs(3);

/// What the app says when datagrams arrive and none is accepted.
///
/// The security gate is the usual reason, and it is invisible from the user's side: the
/// phone looks connected and the app looks like it is waiting. Saying which flag opens the
/// M0 path is the difference between a five-second fix and an uninstall.
const REFUSED_NOTICE: &str = "datagrams are arriving but none is being accepted: pairing is on and \
     no phone has paired, so the audio is refused — pair a phone with the code this app wrote at \
     startup, or start with --no-pairing to accept unencrypted audio";

/// What the app says when it discovers that `Stats::output_level` is not being written.
///
/// Printed once per run, because it changes how the output bar should be read: `?` means
/// "not measured", and without this line a user would reasonably read it as a bug in the
/// meter.
const OUTPUT_UNPUBLISHED_NOTICE: &str = "nothing in this build publishes Stats::output_level, \
     so the output bar is shown as unknown (\"?\") rather than as silence — the input bar is \
     unaffected";

impl Engine {
    /// Prepares the engine. No device is opened until [`Engine::start`].
    pub fn new(config: EngineConfig) -> Self {
        let cable = CableStatus::probe(config.device.as_deref());
        let controls = LiveControls::new(config.noise_suppression, config.echo_cancel, true);
        Self {
            config,
            cable,
            last_cable_probe: Instant::now(),
            session: None,
            mdns: None,
            mdns_started: Instant::now(),
            problem: None,
            output_notice_sent: false,
            refused_notice_sent: false,
            last_start_attempt: Instant::now() - START_RETRY,
            pairing: None,
            controls,
        }
    }

    /// The live settings, to hand to the tray and the dashboard.
    pub fn controls(&self) -> LiveControls {
        self.controls.clone()
    }

    /// What we currently know about the cable.
    pub fn cable(&self) -> &CableStatus {
        &self.cable
    }

    /// True while a session is live.
    pub fn is_running(&self) -> bool {
        self.session.is_some()
    }

    /// True when a start is worth attempting now — the interval since the last one has passed.
    ///
    /// Separate from the attempt itself so the caller can decide, and so the decision is testable
    /// without a device or a clock.
    pub fn ready_to_start(&self) -> bool {
        self.last_start_attempt.elapsed() >= START_RETRY
    }

    /// Records that a start was attempted, so the next one waits.
    pub fn started_attempting(&mut self) {
        self.last_start_attempt = Instant::now();
    }

    /// Opens the device, binds the socket and starts announcing.
    ///
    /// The order is `inspect_render` → queue → `open_output` → `Receiver::bind` →
    /// advertise, and it matters: the device's real rate is what sizes the queue, and the
    /// queue has to exist before the stream that consumes it.
    pub fn start(&mut self) -> Result<StartReport> {
        match self.try_start() {
            Ok(report) => Ok(report),
            Err(e) => {
                let message = format!("{e:#}");
                // The cable status is left exactly as the probe found it.
                //
                // It used to be overwritten with `installed: false` for *any* failure, on the reasoning
                // that "the cable is installed but will not open" and "there is no cable" are the same
                // problem from the user's side. They are not, and the cost was real: a failure to bind
                // the control port was reported as "no virtual microphone", which sent a person to
                // reinstall a driver they already had, and the status flipped back and forth every two
                // seconds as the probe contradicted the lie.
                //
                // A probe that genuinely finds nothing sets `problem` itself. Anything else is some
                // other fault, and the honest thing is to say so.
                self.problem = Some(message.clone());
                Err(anyhow::anyhow!(message))
            }
        }
    }

    /// Opens the pairing channel if it is not already open, and says what to report about the key.
    ///
    /// **Idempotent, and that is the fix for a real bug.** This used to run on every start, binding
    /// the control port each time into a thread that owns it for ever � so stopping and starting the
    /// session once left that port bound by the previous thread, every later start failed with
    /// `os error 10048`, and the app then looked as if it had lost its virtual microphone.
    ///
    /// One channel per process is also the more honest design: pairing does not depend on the audio
    /// session, so a phone stays paired across a Stop and a Start instead of proving itself again.
    fn ensure_pairing(&mut self, bind_ip: IpAddr) -> Result<String> {
        if let Some(existing) = &self.pairing {
            return Ok(existing.note.clone());
        }

        let store = Store::open().context("opening the pairing store")?;
        let pairing = store.pairing().context("loading the pairing key")?;
        let note = if let Some(why) = &pairing.replaced {
            format!(
                "the saved key could not be read ({why}), so a new one was made and every phone \
                 has to pair again"
            )
        } else if pairing.created {
            "this machine's first key".to_string()
        } else {
            format!("reused from {}", store.pairing_path().display())
        };

        // Two copies from the same bytes: the `Arc` goes to the pairing thread for the life of the
        // channel, and the other goes into the link. Both are built before the thread starts, because
        // the `move` closure below takes the `Arc` with it.
        let key_for_link = Psk::from_slice(pairing.psk.as_bytes())?;
        let psk = Arc::new(Psk::from_slice(pairing.psk.as_bytes())?);
        let listener = TcpListener::bind((bind_ip, talkee_proto::DEFAULT_CONTROL_PORT))
            .with_context(|| format!("pairing on port {}", talkee_proto::DEFAULT_CONTROL_PORT))?;

        // One `SharedSession` for the life of the process. Every receiver attaches to this same
        // handle, so a phone that is already paired stays paired through a Stop and a Start: the
        // control connection is never interrupted, and its session is still the one installed here.
        let sessions = SharedSession::new();
        let for_thread = sessions.clone();
        let controls = self.controls.clone();
        thread::Builder::new()
            .name("talkee-pairing".into())
            .spawn(move || serve_pairing(listener, Arc::clone(&psk), for_thread, controls))
            .context("starting the pairing channel")?;

        // The link has to name an address a *phone* can reach. The audio socket may be bound to
        // 0.0.0.0, which is not an address anything can connect to, so this is the LAN address � the
        // same one the startup line prints.
        let host = local_ipv4_addresses()
            .into_iter()
            .find_map(|ip| match ip {
                IpAddr::V4(v4) => Some(v4.to_string()),
                IpAddr::V6(_) => None,
            })
            .ok_or_else(|| anyhow!("no LAN address, so there is nowhere for a phone to connect"))?;

        let link = PairLink::new(
            host,
            talkee_proto::DEFAULT_CONTROL_PORT,
            self.config.instance.clone(),
            key_for_link,
        )?;

        self.pairing = Some(PairingChannel {
            sessions,
            link: link.to_uri(),
            note: note.clone(),
        });
        Ok(note)
    }

    /// The body of [`Engine::start`], without the cable-status bookkeeping.
    fn try_start(&mut self) -> Result<StartReport> {
        if self.session.is_some() {
            return Err(anyhow::anyhow!("a session is already running"));
        }
        let probed = CableStatus::probe(self.config.device.as_deref());
        let endpoint = probed
            .endpoint
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{}", probed.problem.clone().unwrap_or_default()))?;

        let format = vmic::inspect_render(&endpoint)
            .with_context(|| format!("reading the format of {endpoint}"))?;
        let target = target_frames(self.config.buffer_ms, format.sample_rate);
        let capacity = queue_capacity(target);

        let stats = Arc::new(Stats::new());
        let queue = Arc::new(SharedQueue::new(capacity, Arc::clone(&stats)));
        queue.set_prefill(target);

        let writer = vmic::open_output(&format.name, Arc::clone(&queue), self.config.buffer_frames)
            .with_context(|| format!("opening {}", format.name))?;

        let receiver_config = ReceiverConfig {
            listen: self.config.listen,
            device_rate: format.sample_rate,
            gain: self.config.gain,
            // The number that prefilled the queue is the control setpoint too, so the
            // controller holds what the prefill built instead of fighting it.
            target_frames: target,
            security: self.config.security(),
            // On, always, in the app. The phone sends an unprocessed microphone and the level is
            // entirely at the mercy of how far away it is; measured here that was 25 dB below a
            // usable feed, which the meter reported honestly and uselessly as a bar at one cell.
            auto_level: true,
        };
        let mut receiver = Receiver::bind(receiver_config, Arc::clone(&stats), Arc::clone(&queue))
            .with_context(|| format!("binding {}", self.config.listen))?;
        let bound = receiver.local_addr()?;

        // Echo cancellation. A failure to get the reference is reported and never fatal: the
        // microphone works without it, it just hears the speakers too.
        // Always built when there is a reference to build it from: whether it *runs* is a live
        // switch, and a switch that can only turn off what was on at startup is half a switch.
        receiver.set_controls(self.controls.clone());
        let mut loopback = None;
        {
            let (canceller, feed) = talkee_core::echo::EchoCanceller::new(self.config.noise_suppression);
            let mut reference = talkee_core::echo::ReferenceInput::new(feed);
            match vmic::open_loopback(move |samples, rate| reference.push(samples, rate)) {
                Ok(capture) => {
                    println!(
                        "talkee: echo cancellation on, listening to what \"{}\" plays",
                        capture.endpoint
                    );
                    receiver.set_echo_canceller(canceller);
                    loopback = Some(capture);
                }
                Err(e) => println!("talkee: echo cancellation is off: {e:#}"),
            }
        }

        // Pairing, when the configuration asked for encryption. Idempotent: the channel belongs to the
        // process, so a session that is starting for the second time reuses it rather than trying to
        // bind the control port again.
        let mut pairing_link = None;
        let mut pairing_note = None;
        if self.config.security() == SecurityPolicy::RequireEncrypted {
            let note = self.ensure_pairing(bound.ip())?;
            if let Some(channel) = &self.pairing {
                // Every receiver needs the handle, because a receiver starts with no session and the
                // pairing thread is what fills it in.
                receiver.attach_session(channel.sessions.clone());
                pairing_link = Some(channel.link.clone());
            }
            pairing_note = Some(note);
        }

        let receiver_thread = ReceiverThread::spawn(receiver);

        // Advertising is what makes the phone side require no user input at all, so a
        // failure here is reported rather than fatal: an explicitly configured client can
        // still reach this PC.
        let (advertisement, advertisement_problem) = if self.config.advertise {
            match discovery::advertise(
                &self.config.instance,
                bound.port(),
                bound.port(),
                talkee_proto::PROTOCOL_VERSION,
            ) {
                Ok(advertisement) => (Some(advertisement), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        } else {
            (None, None)
        };

        if advertisement.is_some() && self.mdns.is_none() {
            self.mdns = Some(MdnsSelfCheck::start(self.config.instance.clone()));
            self.mdns_started = Instant::now();
        }

        let capture_hint = probed.capture_hint.clone();
        // The advertisement moves into the session, which holds it so the service is
        // withdrawn on stop; its name is what a phone sees, and that is what we report.
        let advertisement_name = advertisement.as_ref().map(|a| a.fullname().to_string());
        self.cable = CableStatus {
            installed: true,
            endpoint: Some(format.name.clone()),
            family: probed.family,
            capture_hint: capture_hint.clone(),
            problem: None,
        };
        self.session = Some(Session {
            stats,
            queue,
            _writer: writer,
            receiver: receiver_thread,
            _advertisement: advertisement,
            _loopback: loopback,
            last_datagram_at: None,
            last_datagrams: 0,
            quiet_for: Duration::ZERO,
            out: OutTracker::default(),
            last_tick: Instant::now(),
        });

        Ok(StartReport {
            endpoint: format.name,
            device_rate: format.sample_rate,
            device_channels: format.channels,
            sample_format: format.sample_format,
            target_frames: target,
            queue_capacity: capacity,
            listen: bound,
            lan_addresses: local_ipv4_addresses(),
            capture_hint,
            advertisement: advertisement_name,
            advertisement_problem,
            pairing_link,
            pairing_note,
        })
    }

    /// Releases the device, the socket and the advertisement.
    pub fn stop(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.receiver.stop();
            // Dropping the session is what closes the device stream and withdraws the mDNS
            // service; both are drop-driven, so there is no separate teardown call that a
            // later refactor could forget to make.
            drop(session);
        }
        // Probe the cable immediately rather than up to two seconds later, so Stop
        // followed by Start is not slowed down by the polling interval.
        self.last_cable_probe = Instant::now() - CABLE_REPROBE;
    }

    /// Samples everything once and turns it into display state.
    pub fn tick(&mut self) -> Status {
        let problem = self.problem.take();
        match self.session.as_mut() {
            Some(session) => {
                let frame = session.tick();
                let mut reading = Reading::sample(&frame.observation, frame.meter);
                reading.lost = frame.lost;
                reading.underruns = frame.underruns;
                reading.queue_frames = frame.queue_frames;
                if frame.meter == OutMeter::Unpublished && !self.output_notice_sent {
                    // Exactly once, and only after the grace period inside OutTracker has
                    // passed, so this is never a false alarm about a device that simply has
                    // not started yet.
                    self.output_notice_sent = true;
                    self.problem = Some(OUTPUT_UNPUBLISHED_NOTICE.to_string());
                }
                if frame.observation.refused > 0
                    && frame.observation.datagrams == 0
                    && !self.refused_notice_sent
                {
                    // Datagrams are arriving and none is being accepted. Either the security
                    // gate is refusing them or they are malformed; the wording covers both,
                    // because the action is the same either way: look at what is on the wire.
                    self.refused_notice_sent = true;
                    self.problem = Some(REFUSED_NOTICE.to_string());
                }
                if let Some(error) = &frame.error {
                    // The socket died. Keep the session, so the state still reports the
                    // silence that follows, and tell the user why exactly once.
                    self.problem = Some(format!("the receive socket stopped: {error}"));
                }
                Status {
                    reading,
                    stopped: false,
                    problem,
                    hint: self.hint(),
                }
            }
            None => {
                if self.last_cable_probe.elapsed() >= CABLE_REPROBE {
                    self.cable = CableStatus::probe(self.config.device.as_deref());
                    self.last_cable_probe = Instant::now();
                }
                let observation = state::idle_observation(self.cable.installed);
                Status {
                    reading: Reading::sample(&observation, OutMeter::Waiting),
                    stopped: true,
                    problem,
                    hint: None,
                }
            }
        }
    }

    /// Extra words for the "looking for your phone" tooltip.
    fn hint(&self) -> Option<String> {
        if !self.config.advertise {
            return Some("not announced on this network".to_string());
        }
        self.mdns
            .as_ref()
            .and_then(|check| check.hint(self.mdns_started.elapsed()))
            .map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn queue_depth_follows_the_device_rate_not_the_requested_one() {
        // The reason `inspect_render` runs before the queue is built: a device that
        // actually runs at 44.1 kHz needs a differently sized queue than one that runs at
        // 48 kHz, and using the wrong one changes the latency the app reports.
        assert_eq!(target_frames(60, 48_000), 2_880);
        assert_eq!(target_frames(60, 44_100), 2_646);
        assert_eq!(target_frames(80, 48_000), 3_840);
    }

    #[test]
    fn a_zero_depth_still_produces_a_usable_queue() {
        // `--buffer-ms 0` is rejected by the argument parser, but the floor is what keeps
        // a rounding-to-zero rate from producing a queue the prefill gate can never open.
        assert_eq!(target_frames(0, 48_000), 1);
        assert_eq!(target_frames(1, 8_000), 8);
    }

    #[test]
    fn capacity_always_has_room_for_a_burst() {
        // The capacity guard inside SharedQueue only means something if it is meaningfully
        // larger than the depth it is guarding.
        for target in [1usize, 2_880, 3_840] {
            let capacity = queue_capacity(target);
            assert!(capacity >= target * 4);
            assert!(capacity >= 1_024, "a tiny target must not shrink the queue");
        }
    }

    #[test]
    fn an_explicit_device_is_trusted_and_a_failure_has_words() {
        // No cpal in this test: an explicitly named endpoint is taken at its word, and a
        // missing one has to produce something a user can read rather than `None`.
        let explicit = CableStatus::probe(Some("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(explicit.installed);
        assert_eq!(
            explicit.endpoint.as_deref(),
            Some("CABLE Input (VB-Audio Virtual Cable)")
        );
        assert!(explicit.problem.is_none());
    }

    #[test]
    fn a_failed_start_is_reported_as_a_problem_rather_than_a_missing_cable() {
        // The bug this pins, found in a real run: a failure to bind the control port was reported as
        // "no virtual microphone". That sent a person to reinstall a driver they already had, and the
        // status then flipped back and forth every two seconds as the cable probe contradicted it.
        //
        // A device that genuinely cannot be opened is still a problem worth showing — it is just not
        // evidence that the cable is missing, and saying so is the difference between a user checking
        // their sound devices and a user re-running an installer.
        let mut engine = Engine::new(EngineConfig {
            listen: "127.0.0.1:0".parse().expect("valid address"),
            device: Some("definitely-not-a-real-endpoint".to_string()),
            advertise: false,
            ..EngineConfig::default()
        });
        assert!(
            engine.cable().installed,
            "an explicitly named endpoint is trusted until something tries to open it"
        );
        let error = engine.start().expect_err("a fake endpoint cannot be opened");
        assert!(!error.to_string().is_empty());
        assert!(!engine.is_running());

        // The status says what was probed — the cable is there — and the *problem* carries the reason
        // the session did not start.
        assert!(
            engine.cable().installed,
            "a failed start must not claim the cable is missing: {}",
            engine.cable().problem.clone().unwrap_or_default()
        );
        let status = engine.tick();
        assert!(
            status.problem.is_some(),
            "a failed start must leave an explanation behind"
        );

        // And a stopped engine reports itself as stopped, with the state coming from the
        // cable rather than from a receiver that is not running. The cable is there, so the state is
        // "looking for your phone" — the `stopped` flag is what says no session is running, not a
        // claim that the hardware has gone missing.
        let status = engine.tick();
        assert!(status.stopped);
        assert_eq!(status.reading.state, crate::state::AppState::Searching);
    }

    #[test]
fn a_failing_start_is_retried_on_an_interval_rather_than_every_tick() {
        // The loop runs at 20 Hz. Retrying a failing start every tick printed twenty lines a second
        // and hammered whatever was refusing — in the run that found this, a port conflict. The gate
        // is what keeps a persistent failure readable.
        let mut engine = Engine::new(EngineConfig {
            listen: "127.0.0.1:0".parse().expect("valid address"),
            advertise: false,
            ..EngineConfig::default()
        });
        // A fresh engine is ready, so the first start is not delayed.
        assert!(engine.ready_to_start());

        engine.started_attempting();
        assert!(
            !engine.ready_to_start(),
            "a start that just failed must not be retried on the next tick"
        );
    }

    #[test]
    fn the_pairing_channel_is_kept_across_a_stop_and_a_start() {
        // The bug this guards, found in a real session: the pairing listener belonged to the session, so
        // stopping and starting the session once left the control port bound by the previous thread.
        // Every later start failed with `os error 10048`, and the app then reported it as a missing
        // virtual microphone — it could never stream again after one Stop and Start.
        //
        // Asserted on the decision rather than on the binding, because binding needs the real port and
        // a test that fights the running app for it is a test that fails for the wrong reason.
        let mut engine = Engine::new(EngineConfig {
            listen: "127.0.0.1:0".parse().expect("valid address"),
            advertise: false,
            ..EngineConfig::default()
        });
        assert!(
            engine.pairing.is_none(),
            "a new engine has no pairing channel yet"
        );

        // Start a session twice in a row, as the retry loop does, and confirm the second start does not
        // try to open the channel again. The device is faked, so this fails at the device and never
        // reaches the pairing code — which is why the check is on the flag rather than on the result.
        engine.pairing = Some(PairingChannel {
            sessions: SharedSession::new(),
            link: "talkee://pair?v=1&h=127.0.0.1&p=47800&k=AA".to_string(),
            note: "test".to_string(),
        });
        engine.started_attempting();

        // A second attempt with the channel already open: `ready_to_start` is the loop's gate, and the
        // channel stays put. Before the fix this path bound the port a second time and failed.
        assert!(!engine.ready_to_start(), "an attempt was just made");
        let channel = engine.pairing.as_ref().expect("the channel is still there");
        assert_eq!(channel.note, "test", "the channel survived");
    }

    #[test]
    fn ticking_a_stopped_engine_does_not_claim_a_live_session() {
        let mut engine = Engine::new(EngineConfig {
            listen: "127.0.0.1:0".parse().expect("valid address"),
            advertise: false,
            ..EngineConfig::default()
        });
        for _ in 0..3 {
            let status = engine.tick();
            assert!(status.stopped, "no session is running");
            assert_eq!(
                status.reading.level_in, 0.0,
                "a stopped engine has no audio to show"
            );
            assert!(status.hint.is_none(), "not looking for anything yet");
        }
    }

    #[test]
    fn the_mdns_hint_stays_quiet_until_the_check_has_had_time() {
        // The self-check runs in the background; until its window has elapsed, any answer
        // would be a guess, and a false "your firewall is blocking discovery" in the
        // searching state is the most expensive false alarm this app could raise.
        let check = MdnsSelfCheck {
            seen: Arc::new(AtomicBool::new(false)),
            handle: None,
        };
        assert_eq!(check.hint(Duration::ZERO), None);
        assert_eq!(check.hint(MDNS_SELF_CHECK_WINDOW - Duration::from_millis(1)), None);
        assert!(check.hint(MDNS_SELF_CHECK_WINDOW).is_some());
        assert!(check.hint(MDNS_SELF_CHECK_WINDOW + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn seeing_our_own_record_changes_the_hint() {
        let seen = MdnsSelfCheck {
            seen: Arc::new(AtomicBool::new(true)),
            handle: None,
        };
        let unseen = MdnsSelfCheck {
            seen: Arc::new(AtomicBool::new(false)),
            handle: None,
        };
        assert_eq!(
            seen.hint(MDNS_SELF_CHECK_WINDOW),
            Some("discoverable on this network")
        );
        assert_ne!(seen.hint(MDNS_SELF_CHECK_WINDOW), unseen.hint(MDNS_SELF_CHECK_WINDOW));
    }
}


