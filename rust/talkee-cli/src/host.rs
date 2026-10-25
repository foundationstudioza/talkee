//! `talkee host` — the desktop side.
//!
//! Opens a virtual cable's render endpoint, listens for UDP audio, and plays what
//! arrives into that endpoint. Everything else in the system then sees the cable"s
//! capture endpoint as a live microphone.
//!
//! # Order of operations matters here
//!
//! The output is inspected *before* the queue is created, because the queue must be
//! sized against the sample rate the device actually runs at — which is frequently
//! not the rate shown in the OS sound settings. Getting this backwards produces a
//! queue that is the wrong length, and thus a latency number that silently
//! disagrees with reality.
//!
//! # Order of operations matters here too
//!
//! The pairing key is resolved, and the control listener is bound, *before* the
//! audio socket opens. A host that printed a deep link it could not honour, or that
//! listened for audio with nothing able to unlock it, would be worse than one that
//! failed to start: the user would be looking at a working-looking screen while the
//! microphone stayed dead.
//!
//! Pairing is **opt-in**, and the default is the zero-configuration path that actually
//! works: with no `--key` and no `--secure`, the host accepts plaintext audio from
//! anyone who can reach the port, and says so loudly on startup.
//!
//! That default is a deliberate, temporary trade rather than an oversight. The phone
//! app cannot encrypt yet, so requiring a pairing by default would leave the common
//! case holding a microphone that receives nothing — encryption no client can perform
//! is not security, it is a broken app. Pass `--secure` to generate a key and demand a
//! pairing, or `--key` to use a known one, and change this default the moment the phone
//! side can pair.

use anyhow::{anyhow, Context, Result};
use talkee_store::Store;
use clap::Args;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;
use talkee_core::stream::{SecurityPolicy, SharedSession};
use talkee_core::{local_ipv4_addresses, Receiver, ReceiverConfig, SharedQueue, Stats};
use talkee_discovery as discovery;
use talkee_proto::crypto::{PairLink, Psk};
use talkee_vmic as vmic;

/// Arguments for `talkee host`.
#[derive(Args, Debug)]
pub struct HostArgs {
    /// Render endpoint to play into. Defaults to the first detected virtual
    /// cable, which is what you want almost every time.
    #[arg(long)]
    pub device: Option<String>,

    /// Address to listen on for audio. `0.0.0.0` accepts from anywhere on the
    /// LAN; use `127.0.0.1` for loopback-only testing.
    #[arg(long, default_value = "0.0.0.0:47801")]
    pub listen: SocketAddr,

    /// Linear gain applied to incoming audio, before resampling.
    #[arg(long, default_value_t = 1.0)]
    pub gain: f32,

    /// Depth of the output buffer, in milliseconds.
    ///
    /// Audio is buffered this long before playback starts, and the queue is held
    /// at roughly this depth afterwards. This is *the* latency-versus-dropouts
    /// dial; 40-80 ms is the useful range on a LAN, and the default is 60 ms.
    #[arg(long, default_value_t = 60)]
    pub buffer_ms: u32,

    /// Requested device buffer size in frames. Omit to use the device default.
    /// Small values cut latency but may be rejected on some drivers.
    #[arg(long)]
    pub buffer_frames: Option<u32>,

    /// Port the phone pairs on. Audio is the next port up, and the deep link says
    /// both.
    #[arg(long, default_value_t = talkee_proto::DEFAULT_CONTROL_PORT)]
    pub control_port: u16,

    /// Pairing key to load: base64url, 64 hex characters, or a whole
    /// `talkee://pair?...` link. Omit to generate a fresh key for this run.
    #[arg(long)]
    pub key: Option<String>,

    /// Generate a key, demand a pairing, and refuse plaintext audio.
    ///
    /// Off by default. That is a deliberate trade: the phone app cannot encrypt yet, so
    /// requiring a pairing would leave the common case with a microphone that receives
    /// nothing. Without this flag the host accepts plaintext from anyone who can reach
    /// the port and prints that fact plainly on startup, leaving the judgement to the
    /// user about which network they are on. Turn it on, or pass `--key`, as soon as the
    /// phone side can pair.
    #[arg(long)]
    pub secure: bool,

    /// Do not advertise over mDNS.
    ///
    /// Advertising is discovery, not authorisation: it tells the network that this
    /// host exists, and the pairing key decides who may send audio. Turning it off
    /// only means the user has to type an address.
    #[arg(long)]
    pub no_advertise: bool,

    /// Disable the drift controller, leaving M0's behaviour of throwing the oldest
    /// samples away whenever the queue overflows.
    ///
    /// Kept as an escape hatch, and for A/B measurement: "does the loop help?" is a
    /// question that deserves two runs and a diff rather than an argument.
    #[arg(long)]
    pub no_drift_control: bool,

    /// Disable the automatic level stage, so frames arrive exactly as the sender sent them.
    ///
    /// For measurement. Calibrating a level problem needs the raw numbers, and it is the only way
    /// to tell a quiet sender from an over-eager gain stage — which is how the level stage itself
    /// was justified in the first place.
    #[arg(long)]
    pub no_auto_level: bool,

    /// Replace the saved pairing key, so every phone that was paired has to pair again.
    ///
    /// The only way to revoke a phone. A credential that is kept between runs and cannot be
    /// replaced is a credential you cannot take back.
    #[arg(long)]
    pub rotate_key: bool,
}

/// Loads a key from whatever the user pasted.
///
/// Accepts a whole deep link as well as a bare key, because the link is what the
/// host printed and what a user will copy: making them extract the `k=` parameter by
/// hand is how keys end up pasted into the wrong tool, or into a chat window.
fn load_key(text: &str) -> Result<Psk> {
    let text = text.trim();
    if text.starts_with("talkee://") {
        let link = PairLink::parse(text).context("reading the pairing link in --key")?;
        return Ok(link.psk);
    }
    Psk::from_text(text).context("reading the pairing key in --key")
}

/// Runs the host until interrupted.
pub fn run(args: HostArgs) -> Result<()> {
    if args.buffer_ms == 0 {
        return Err(anyhow!("--buffer-ms must be greater than zero"));
    }
    if !args.gain.is_finite() || args.gain < 0.0 {
        return Err(anyhow!("--gain must be a finite value >= 0"));
    }
    // Whether a pairing channel is opened at all. A key the user typed is a request to
    // use it, so `--key` implies `--secure`.
    let secure = args.secure || args.key.is_some();
    if secure && args.control_port == 0 {
        return Err(anyhow!("--control-port 0 is not a port a phone could connect to"));
    }

    // The key is resolved before any socket is opened: a mistyped --key should fail loudly here,
    // not leave a microphone listening with nothing able to unlock it.
    //
    // It is the *machine's* key rather than this run's, so a phone pairs once instead of scanning a
    // code every time this starts. `--key` still wins: naming a key is being explicit, and it is how
    // the tests drive a known one.
    let mut pairing_notes: Vec<String> = Vec::new();
    let psk = match &args.key {
        Some(text) => load_key(text)?,
        None => {
            let store = Store::open().context("opening the pairing store")?;
            if args.rotate_key {
                store
                    .forget_pairing()
                    .context("discarding the saved pairing")?;
                pairing_notes.push("rotated: every phone has to pair again".to_string());
            }
            let pairing = store.pairing().context("loading the pairing key")?;
            if let Some(why) = pairing.replaced {
                // Said out loud because it is neither the user's fault nor visible from the
                // microphone end: the phone that used to work simply stops.
                pairing_notes.push(format!(
                    "the saved key could not be read ({why}), so a new one was made and every \
                     phone has to pair again"
                ));
            } else if pairing.created && !args.rotate_key {
                pairing_notes.push("this machine's first key".to_string());
            } else if !pairing.created {
                pairing_notes.push(format!("reused from {}", store.pairing_path().display()));
            }
            pairing.psk
        }
    };
    let security = if secure {
        SecurityPolicy::RequireEncrypted
    } else {
        SecurityPolicy::AllowPlaintext
    };

    let (query, family) = match &args.device {
        Some(explicit) => (explicit.clone(), None),
        None => {
            let (name, family) = vmic::find_virtual_cable()?.ok_or_else(|| {
                anyhow!(
                    "no virtual audio cable detected, so there is nothing to render into.\n\
                     Run `talkee probe` to see what is installed and what to do about it."
                )
            })?;
            (name, Some(family))
        }
    };

    let format = vmic::inspect_render(&query)?;
    println!("talkee host {} — desktop receiver", env!("CARGO_PKG_VERSION"));
    println!(
        "  output      : {} ({} Hz, {} ch, {})",
        format.name, format.sample_rate, format.channels, format.sample_format
    );
    if let Some(family) = family {
        println!("  cable family: {} ({})", family.label, family.id);
    }

    let stats = Arc::new(Stats::new());

    // The requested depth is the *prefill* target: audio does not start until
    // this much has accumulated, which is what stops the callback starving. The
    // capacity is deliberately a few times that, so a burst of late frames is
    // absorbed rather than dropped, while a genuine clock mismatch still gets
    // trimmed back to the target depth over time.
    let target_frames = ((args.buffer_ms as u64 * format.sample_rate as u64) / 1000).max(1) as usize;
    let queue = Arc::new(SharedQueue::new(
        (target_frames * 4).max(1024),
        Arc::clone(&stats),
    ));
    queue.set_prefill(target_frames);
    println!(
        "  buffer      : {} ms ({} frames) before playback starts",
        args.buffer_ms, target_frames
    );
    // The same number is the control setpoint: prefill fills the queue to it, and the
    // drift loop then holds it there. Zero frames means "no controller", which is the
    // M0 behaviour.
    let control_frames = if args.no_drift_control { 0 } else { target_frames };
    println!(
        "  drift loop  : {}",
        if args.no_drift_control {
            "off — the oldest samples are dropped when the queue overflows".to_string()
        } else {
            format!("on, holding {} frames", control_frames)
        }
    );

    // The control listener comes up before the audio socket, so a port clash is a
    // startup error rather than a deep link that leads nowhere. It binds the same
    // interface as the audio socket: a user who restricted audio to loopback did not
    // ask for a pairing port on the whole network.
    let listener = if secure {
        Some(
            TcpListener::bind((args.listen.ip(), args.control_port)).with_context(|| {
                format!(
                    "binding the pairing channel on {}:{}",
                    args.listen.ip(),
                    args.control_port
                )
            })?,
        )
    } else {
        None
    };

    // Keep the writer bound for the whole session: dropping it closes the device
    // and silences the microphone. Hence the leading underscore, not `let _`.
    let _writer = vmic::open_output(&query, Arc::clone(&queue), args.buffer_frames)
        .with_context(|| format!("opening {}", format.name))?;

    let config = ReceiverConfig {
        listen: args.listen,
        device_rate: format.sample_rate,
        gain: args.gain,
        target_frames: control_frames,
        auto_level: !args.no_auto_level,
        security,
    };
    let mut receiver = Receiver::bind(config, Arc::clone(&stats), Arc::clone(&queue))
        .with_context(|| format!("binding {}", args.listen))?;
    let bound = receiver.local_addr()?;
    let shared = SharedSession::new();
    receiver.attach_session(shared.clone());
    println!();
    println!("Listening for audio on {bound}");
    let addresses = local_ipv4_addresses();
    if addresses.is_empty() {
        println!("  No LAN address found; this machine does not appear to be on a network.");
    } else {
        for ip in &addresses {
            println!("  point the phone at: {ip}:{}", bound.port());
        }
    }

    if let Some(listener) = &listener {
        // The link must say an address the audio socket actually accepts on, or the
        // phone would pair successfully and then talk into a closed port. So the
        // bind address decides, and only a wildcard bind falls back to the LAN
        // address a phone would recognise.
        let advertised_host = if bound.ip().is_unspecified() {
            addresses
                .first()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".to_string())
        } else {
            bound.ip().to_string()
        };
        let link = PairLink::new(
            &advertised_host,
            args.control_port,
            discovery::default_instance_name(),
            psk.clone(),
        )?;
        println!("  security    : AES-256-GCM, and no audio is accepted before a pairing");
        println!(
            "  pair on     : {}:{}",
            args.listen.ip(),
            args.control_port
        );
        println!("  pairing link: {}", link.to_uri());
        println!(
            "                the link carries this machine's key: anyone who sees it can speak \
             through this microphone until the key is rotated"
        );
        for note in &pairing_notes {
            println!("  pairing     : {note}");
        }
        // The link is 132 characters and it *is* the key, so nobody is going to type it. Drawn
        // rather than written to a file, because the copy on this screen is the only copy — and
        // drawn from the same string that was just printed, so the two cannot disagree.
        println!();
        match crate::qr::qr_text(&link.to_uri()) {
            Ok(code) => print!("{code}"),
            Err(e) => println!("  (could not draw the pairing link as a QR code: {e:#})"),
        }
        println!();
        if bound.port() != args.control_port.saturating_add(1) {
            println!(
                "  note        : audio is on {} but the link implies {} (control + 1); a phone \
                 using the link alone will need --to {}",
                bound.port(),
                args.control_port.saturating_add(1),
                bound.port()
            );
        }
        match listener.local_addr() {
            Ok(addr) => println!("  control     : accepting pairings on {addr}"),
            Err(e) => println!("  control     : accepting pairings (address unreadable: {e})"),
        }
    } else {
        println!("  security    : NONE — plaintext audio is accepted from anyone who can reach this port");
        println!("                (pass --secure to require a pairing and encrypt the audio)");
    }
    // Zero-configuration: announce ourselves so a phone can find us without the
    // user typing anything. A failure here is not fatal — an explicitly configured
    // client can still reach us — so warn and carry on rather than refusing to
    // start.
    //
    // The SRV port is the control port, because that is the port a peer must reach
    // first; the TXT record carries the audio port. Advertising is discovery, not
    // authorisation: it says a host is here, and the pairing key says who may be
    // heard.
    let srv_port = if secure { args.control_port } else { bound.port() };
    // Not advertised when nothing on the network could reach it: an advertisement
    // for a loopback-only host is a phone that finds a host it cannot talk to.
    let _advertisement = if args.no_advertise || args.listen.ip().is_loopback() {
        None
    } else {
        let instance = discovery::default_instance_name();
        match discovery::advertise(&instance, srv_port, bound.port(), talkee_proto::PROTOCOL_VERSION) {
            Ok(advertisement) => {
                println!("  discoverable: {}", advertisement.fullname());
                Some(advertisement)
            }
            Err(e) => {
                println!("  discoverable: no ({e})");
                None
            }
        }
    };

    if let Some(family) = family {
        println!(
            "  in other apps, select a microphone containing: \"{}\"",
            family.capture_pattern
        );
    }
    println!();

    // The pairing channel runs on its own thread: the receive loop below owns this
    // one and blocks in `recv_from`.
    if let Some(listener) = listener {
        let psk = Arc::new(psk);
        let controls = talkee_core::live::LiveControls::new(true, true, !args.no_auto_level);
        thread::Builder::new()
            .name("talkee-pairing".into())
            .spawn(move || talkee_core::control::serve_pairing(listener, psk, shared, controls))
            .context("starting the pairing thread")?;
    }
    println!("Press Ctrl+C to stop. A health line prints every few seconds.");
    println!();

    // M0 has no signal handler, by design: Ctrl+C terminates the process, which
    // is the documented way to stop it. M4 adds a graceful shutdown.
    let shutdown = AtomicBool::new(false);
    receiver.run(&shutdown)?;
    Ok(())
}
