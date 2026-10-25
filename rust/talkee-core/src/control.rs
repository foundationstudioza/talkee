//! The pairing channel: how a phone proves it holds this machine's key.
//!
//! # Why this lives in `talkee-core`
//!
//! It is platform-free and it is needed by *two* front ends: the `talkee host` command and the
//! tray application. It used to live inside the CLI, which meant the application — a receiver in
//! exactly the same sense — had the security policy but no way to be convinced by anything. Moving
//! it here is what lets a QR code in the tray mean something.
//!
//! The design notes that matter are on [`handle_pairing`], and the wire format is
//! `docs/PROTOCOL.md`.
//!
//! # What is not negotiable here
//!
//! * **The challenge is chosen by the receiver.** A peer that supplied its own nonce could answer
//!   one it had already seen answered.
//! * **Nothing touches the audio path before the proof verifies.** The session is installed only
//!   after `verify_proof` succeeds.
//! * **The session is dropped when the connection ends**, so a phone that goes away stops being
//!   able to inject frames at all rather than merely stopping of its own accord.

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use talkee_proto::crypto::session::{Direction, Session};
use talkee_proto::crypto::{verify_proof, Challenge, Psk, SessionId};
use talkee_proto::{control, ControlMsg, SampleFormat};

use crate::live::{LiveControls, Setting};
use crate::stream::SharedSession;

/// How long a peer has to get through the handshake before it is let go.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the control loop wakes to look for settings that changed on the PC.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// How long a paired control connection may go quiet before it is let go.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// How many control connections may be in flight at once.
///
/// Pairing needs one. The rest is headroom for a phone that retried because the user was reading
/// the dialog, and a bound that stops a connecting script from spawning threads without limit.
const MAX_CONTROL_CONNECTIONS: usize = 8;

/// Replaces anything that is not a printable ASCII character.
///
/// Peer-supplied strings are printed to a terminal, and a name containing an ANSI escape sequence
/// can rewrite the screen — or, worse, the history — of the person running the host.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .collect()
}

/// Accepts pairing connections until the process exits.
///
/// One thread per connection, with a cap. A paired phone keeps its control connection open for
/// telemetry, and the accept loop must stay free while it does; the cap is what stops a script that
/// connects in a loop from spawning threads without limit, and the handshake timeout is what stops
/// one that says nothing.
pub fn serve_pairing(
    listener: TcpListener,
    psk: Arc<Psk>,
    shared: SharedSession,
    controls: LiveControls,
) {
    let live = Arc::new(AtomicUsize::new(0));
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("talkee: control accept failed: {e}");
                continue;
            }
        };
        if live.load(Ordering::Relaxed) >= MAX_CONTROL_CONNECTIONS {
            eprintln!(
                "talkee: refusing a pairing connection: {MAX_CONTROL_CONNECTIONS} are already in flight"
            );
            drop(stream);
            continue;
        }
        live.fetch_add(1, Ordering::Relaxed);
        let psk = Arc::clone(&psk);
        let shared = shared.clone();
        let controls = controls.clone();
        let in_flight = Arc::clone(&live);
        let spawned = thread::Builder::new()
            .name("talkee-pairing-conn".into())
            .spawn(move || {
                if let Err(error) = handle_pairing(stream, &psk, &shared, &controls) {
                    eprintln!("talkee: pairing connection: {error:#}");
                }
                in_flight.fetch_sub(1, Ordering::Relaxed);
            });
        if let Err(e) = spawned {
            eprintln!("talkee: could not start a pairing thread: {e}");
            live.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Runs one pairing handshake on one control connection.
///
/// The receiver speaks second, and that is the point: the challenge must be chosen by the side that
/// has to be convinced. A peer that supplied its own nonce could answer one it had already seen
/// answered.
///
/// The session is installed only after `verify_proof` succeeds, and it is dropped when this
/// connection ends, so a phone that goes away stops being able to inject frames at all rather than
/// merely stopping of its own accord.
fn handle_pairing(
    stream: TcpStream,
    psk: &Psk,
    shared: &SharedSession,
    controls: &LiveControls,
) -> Result<()> {
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .context("setting the handshake timeout")?;
    stream
        .set_write_timeout(Some(HANDSHAKE_TIMEOUT))
        .context("setting the handshake timeout")?;
    let peer = stream
        .peer_addr()
        .map(|address| address.to_string())
        .unwrap_or_else(|_| "unknown peer".to_string());
    let mut writer = stream;
    let mut reader =
        control::buffered_reader(writer.try_clone().context("cloning the control socket")?);
    let mut line = String::new();

    // 1. HELLO, from the peer.
    control::read_line(&mut reader, &mut line).context("reading HELLO")?;
    let (version, name, caps) = match ControlMsg::from_line(&line).context("reading HELLO")? {
        ControlMsg::Hello {
            ver, name, caps, ..
        } => (ver, name, caps),
        other => bail!("a pairing connection starts with HELLO, and this one sent {other:?}"),
    };
    if version != talkee_proto::PROTOCOL_VERSION {
        // A version mismatch is a hard stop rather than a best-effort parse: the failure mode of
        // guessing is garbage audio, which is far harder to diagnose than silence.
        let reason = format!(
            "this host speaks protocol version {}, not {version}",
            talkee_proto::PROTOCOL_VERSION
        );
        let _ = ControlMsg::Rejected { reason }.write_to(&mut writer);
        bail!("peer speaks protocol version {version}");
    }
    println!(
        "talkee: {} at {peer} wants to pair ({caps:?})",
        printable(&name)
    );

    // 2. CHALLENGE, from the receiver, fresh for this connection.
    let challenge = Challenge::generate()?;
    ControlMsg::Challenge {
        nonce: challenge.to_hex(),
    }
    .write_to(&mut writer)
    .context("sending CHALLENGE")?;

    // 3. PAIR, from the peer.
    control::read_line(&mut reader, &mut line).context("reading PAIR")?;
    let (proof, session_hex) = match ControlMsg::from_line(&line).context("reading PAIR")? {
        ControlMsg::Pair { proof, session } => (proof, session),
        ControlMsg::Rejected { reason } => {
            bail!("the peer rejected this host: {}", printable(&reason))
        }
        other => bail!("expected PAIR, got {other:?}"),
    };
    let session_id = match SessionId::from_hex(&session_hex) {
        Ok(id) => id,
        Err(e) => {
            let _ = ControlMsg::Rejected {
                reason: "the session id is not 4 hex-encoded bytes".to_string(),
            }
            .write_to(&mut writer);
            bail!("bad session id: {e}");
        }
    };

    if !verify_proof(psk, challenge.as_bytes(), &session_id, &proof) {
        // The event this whole path exists for. Say so once, refuse, and leave nothing installed: an
        // unauthenticated peer must not even reach the audio socket, let alone the speaker.
        eprintln!(
            "talkee: {peer} does not hold the pairing key; no audio will be accepted from it"
        );
        let _ = ControlMsg::Rejected {
            reason: "the pairing proof did not verify against this host key".to_string(),
        }
        .write_to(&mut writer);
        return Ok(());
    }

    // 4. Install the session, then say how to encode. Nothing before this point has touched the
    //    audio path.
    shared.install(Session::new(psk, session_id, Direction::PhoneToDesktop));
    println!(
        "talkee: {peer} paired (session {}); frames sealed with this key are accepted",
        session_id.to_hex()
    );
    ControlMsg::Config {
        sample_rate: 48_000,
        channels: 1,
        format: SampleFormat::F32,
        frame_ms: 10,
        bitrate: 0,
        fec: false,
        source: "unprocessed".to_string(),
    }
    .write_to(&mut writer)
    .context("sending CONFIG")?;
    controls.set_phone(Some(printable(&name)));

    // 5. Stay connected for telemetry. The connection now has an idle timeout that can be missed
    //    without consequence: a phone that stops sending PINGs is still paired.
    //
    // The read wakes every few hundred milliseconds so the loop can notice a setting changed on the
    // PC — the tray, the dashboard — and tell the phone. A blocking read could only ever answer the
    // phone, which is how the two sides ended up disagreeing about whether anything was on.
    writer
        .set_read_timeout(Some(POLL_INTERVAL))
        .context("setting the control poll interval")?;
    let mut echoed: Option<u32> = None;
    let mut told_version = 0u64;
    let mut idle_since = std::time::Instant::now();
    loop {
        let version = controls.version();
        if version != told_version {
            told_version = version;
            ControlMsg::State {
                paused: controls.get(Setting::Paused),
                noise_suppression: controls.get(Setting::NoiseSuppression),
                echo_cancel: controls.get(Setting::EchoCancel),
            }
            .write_to(&mut writer)
            .context("sending STATE")?;
        }

        // A peer that closes its end — which is what every sender does when it finishes — surfaces
        // as a reset on Windows as often as as an EOF, and neither is an error worth a stack trace.
        let read = match control::read_line(&mut reader, &mut line) {
            Ok(read) => read,
            Err(control::ControlError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if idle_since.elapsed() > IDLE_TIMEOUT {
                    break;
                }
                continue;
            }
            Err(control::ControlError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                ) =>
            {
                break
            }
            Err(error) => bail!("{error}"),
        };
        if read == 0 {
            break;
        }
        idle_since = std::time::Instant::now();
        match ControlMsg::from_line(&line) {
            Ok(ControlMsg::Set {
                paused,
                noise_suppression,
                echo_cancel,
            }) => {
                if let Some(v) = paused {
                    controls.set(Setting::Paused, v);
                }
                if let Some(v) = noise_suppression {
                    controls.set(Setting::NoiseSuppression, v);
                }
                if let Some(v) = echo_cancel {
                    controls.set(Setting::EchoCancel, v);
                }
            }
            Ok(ControlMsg::Ping { ts }) => {
                // The protocol says a peer echoes `ts`, and this is the echo. The guard is what
                // stops the obvious loop: our own echo coming back carries the same timestamp, so
                // it is not answered again.
                if echoed != Some(ts) {
                    echoed = Some(ts);
                    ControlMsg::Ping { ts }.write_to(&mut writer)?;
                }
            }
            Ok(ControlMsg::Stats {
                frames_sent,
                frames_dropped,
                battery_pct,
            }) => println!(
                "talkee: phone: {frames_sent} frames sent, {frames_dropped} dropped, \
                 battery {battery_pct}%"
            ),
            Ok(ControlMsg::Stop) => {
                controls.set(Setting::Paused, true);
            }
            Ok(ControlMsg::Start) => {
                controls.set(Setting::Paused, false);
            }
            Ok(other) => println!("talkee: ignoring {other:?} from {peer}"),
            Err(error) => bail!("{error}"),
        }
    }

    // The phone is gone. Drop the session, but only if it is still ours: a phone that reconnected —
    // installing a new session — and then closed its old connection must not have the new session
    // taken away with it.
    if shared.clear_if(session_id) {
        controls.set_phone(None);
        println!("talkee: {peer} disconnected; its session was dropped");
    } else {
        println!("talkee: {peer} disconnected; a newer session is in place and stays");
    }
    Ok(())
}
