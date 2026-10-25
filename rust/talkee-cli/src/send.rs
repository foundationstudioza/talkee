//! `talkee send` — a stand-in for the phone.
//!
//! This exists so the pipeline can be validated with zero mobile toolchain work.
//! It pairs the way the phone will — `HELLO`, `CHALLENGE`, `PAIR` — and then seals
//! every audio datagram with AES-256-GCM exactly as the phone must: a 12-byte nonce
//! derived from the sequence number, the 24-byte header as associated data, and the
//! `ENCRYPTED` flag set. If `talkee send | talkee host` works, the only unknown left
//! is Android capture, which is precisely the thing M0 set out to measure.
//!
//! It also prints the wire bitrate, which is the number that motivates the move to
//! Opus: raw 48 kHz mono f32 is about 1.6 Mbps, and 32 bytes of AEAD overhead per
//! frame is 26 kbps on top of it.

use anyhow::{bail, Context, Result};
use clap::Args;
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use talkee_proto::crypto::pair::PairLink;
use talkee_proto::crypto::psk::Psk;
use talkee_proto::crypto::session::{Direction, Session};
use talkee_proto::crypto::{proof_hex, Challenge, SessionId, AEAD_OVERHEAD};
use talkee_proto::{control, flags, AudioHeader, ControlMsg, SampleFormat, HEADER_LEN};

/// How long a click burst lasts when `--click` is used.
const CLICK_MS: u64 = 2;

/// How often to print a summary line.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// How long to wait for each step of the pairing handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Where audio goes when neither `--to` nor a deep link says.
const DEFAULT_AUDIO_ADDR: &str = "127.0.0.1:47801";
/// Arguments for `talkee send`.
#[derive(Args, Debug)]
pub struct SendArgs {
    /// Where to send audio. Omit to use the address in the pairing link, which is
    /// almost always what you want.
    #[arg(long)]
    pub to: Option<SocketAddr>,

    /// The pairing link (or bare key) that `talkee host` printed.
    ///
    /// A link is preferred because it also says where to connect; a bare key needs
    /// `--to`, and the control port is then taken to be the audio port minus one.
    #[arg(long)]
    pub key: Option<String>,

    /// Sample rate to synthesise and claim, in Hz.
    #[arg(long, default_value_t = 48_000)]
    pub rate: u32,

    /// Frame duration in milliseconds. 10 ms matches the planned phone cadence.
    #[arg(long, default_value_t = 10)]
    pub frame_ms: u32,

    /// Tone frequency in Hz. Ignored when `--click` is set.
    #[arg(long, default_value_t = 440.0)]
    pub tone: f64,

    /// Emit a short click every `--click-interval-ms` instead of a continuous
    /// tone. This is the signal for the clap test in docs/LATENCY.md, because a
    /// transient is far easier to line up in a waveform editor than a sine wave.
    #[arg(long, default_value_t = false)]
    pub click: bool,

    /// Interval between clicks, in milliseconds.
    #[arg(long, default_value_t = 2000)]
    pub click_interval_ms: u32,

    /// Peak amplitude, 0.0 to 1.0.
    #[arg(long, default_value_t = 0.25)]
    pub amplitude: f32,

    /// Stop after this many seconds. 0 means run until interrupted.
    #[arg(long, default_value_t = 0)]
    pub seconds: u32,

    /// Pace frames faster (positive) or slower (negative) than real time, in parts
    /// per million.
    ///
    /// A stand-in for a clock difference, and the only way to test the receiver"s
    /// drift controller: this sender paces itself on `Instant`, so its rate is
    /// *exactly* nominal by construction and there is otherwise nothing to correct.
    #[arg(long, default_value_t = 0)]
    pub pace_ppm: i32,
}

/// A live pairing: the session to seal with, and whether it is still alive.
struct Pairing {
    session: Session,
    alive: Arc<AtomicBool>,
}

impl Pairing {
    /// Runs the handshake and starts watching the connection.
    ///
    /// The watch matters after a crash: without it, a sender whose host restarted
    /// would keep sealing frames with a key the host no longer has and the user
    /// would see a sender that reports success into a silent microphone.
    fn establish(control: SocketAddr, psk: &Psk) -> Result<Self> {
        let (session, stream) = handshake(control, psk)?;
        let alive = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&alive);
        thread::Builder::new()
            .name("talkee-send-control".into())
            .spawn(move || {
                // Nothing is expected on this connection except the host"s
                // acknowledgements; the thread exists to notice the moment it dies.
                let mut reader = control::buffered_reader(stream);
                let mut line = String::new();
                while let Ok(read) = control::read_line(&mut reader, &mut line) {
                    if read == 0 {
                        break;
                    }
                }
                flag.store(false, Ordering::Relaxed);
            })
            .context("starting the control-connection watcher")?;
        Ok(Self { session, alive })
    }

    /// False once the host's control connection has gone away.
    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}
/// Loads `--key`, which may be a whole deep link or a bare key.
///
/// Returns the link too when there was one, because it also carries the address to
/// connect to: making a user transcribe an IP address and a port by hand is exactly
/// the kind of step that gets a demo wrong.
fn load_pairing(text: &str) -> Result<(Psk, Option<PairLink>)> {
    let text = text.trim();
    if text.starts_with("talkee://") {
        let link = PairLink::parse(text).context("reading the pairing link in --key")?;
        return Ok((link.psk.clone(), Some(link)));
    }
    Ok((Psk::from_text(text).context("reading the pairing key in --key")?, None))
}

/// The control address that belongs to an audio address.
///
/// The two ports are adjacent by convention and in the deep link: 47800 carries
/// control, 47801 carries audio. A link always says the control port explicitly, so
/// this is only the fallback for a bare key plus `--to`.
fn control_address_for(audio: SocketAddr) -> SocketAddr {
    SocketAddr::new(audio.ip(), audio.port().saturating_sub(1))
}

/// Runs `HELLO` / `CHALLENGE` / `PAIR` / `CONFIG` and returns what to seal with.
///
/// The key itself is never sent. The host picks the nonce, and the proof is
/// `HMAC-SHA256(psk, "talkee-pair-v1" || nonce || session_id)`, so an eavesdropper
/// records something that is worthless against the next challenge.
fn handshake(control: SocketAddr, psk: &Psk) -> Result<(Session, TcpStream)> {
    let stream = TcpStream::connect_timeout(&control, HANDSHAKE_TIMEOUT)
        .with_context(|| format!("connecting to the host's control channel at {control}"))?;
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .context("setting the handshake timeout")?;
    stream
        .set_write_timeout(Some(HANDSHAKE_TIMEOUT))
        .context("setting the handshake timeout")?;
    let mut writer = stream.try_clone().context("cloning the control socket")?;
    let mut reader =
        control::buffered_reader(stream.try_clone().context("cloning the control socket")?);
    let mut line = String::new();

    // The session id is ours to choose, and it is fresh for every run: it selects a
    // new session key, which is what makes "the nonce is just seq" safe even though
    // this sender will start again at zero next time.
    let session_id = SessionId::generate()?;
    let device = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "talkee-send".to_string());

    // 1. HELLO.
    ControlMsg::hello(device, "talkee send", vec!["aes-gcm".into(), "pcm-f32".into()])
        .write_to(&mut writer)
        .context("sending HELLO")?;

    // 2. CHALLENGE.
    control::read_line(&mut reader, &mut line).context("reading CHALLENGE")?;
    let nonce = match ControlMsg::from_line(&line).context("reading CHALLENGE")? {
        ControlMsg::Challenge { nonce } => {
            Challenge::parse(&nonce).context("reading the challenge nonce")?
        }
        ControlMsg::Rejected { reason } => {
            bail!("the host refused us: {}", printable(&reason))
        }
        other => bail!("expected CHALLENGE, got {other:?}"),
    };

    // 3. PAIR.
    ControlMsg::Pair {
        proof: proof_hex(psk, nonce.as_bytes(), &session_id),
        session: session_id.to_hex(),
    }
    .write_to(&mut writer)
    .context("sending PAIR")?;

    // 4. CONFIG, or a refusal we can explain.
    control::read_line(&mut reader, &mut line).context("reading CONFIG")?;
    match ControlMsg::from_line(&line).context("reading CONFIG")? {
        ControlMsg::Config { sample_rate, channels, format, .. } => println!(
            "  paired      : session {} — the host asks for {} Hz, {} ch, {:?}",
            session_id.to_hex(),
            sample_rate,
            channels,
            format
        ),
        ControlMsg::Rejected { reason } => {
            bail!("the host rejected the pairing: {}", printable(&reason))
        }
        other => bail!("expected CONFIG, got {other:?}"),
    }

    // No read timeout from here on: the watcher thread parks on this connection and
    // should not wake up just because the phone is quiet.
    stream_reset_timeout(&stream)?;
    Ok((Session::new(psk, session_id, Direction::PhoneToDesktop), stream))
}

/// Clears the read timeout, so a blocking read means "wait for the peer".
fn stream_reset_timeout(stream: &TcpStream) -> Result<()> {
    stream
        .set_read_timeout(None)
        .context("clearing the control read timeout")
}
/// Replaces anything that is not a printable ASCII character.
///
/// A hostile peer chooses the strings it sends us, and we print them. Without this,
/// a `REJECTED` reason is a terminal-injection primitive: escape sequences that
/// rewrite the screen, or write to the scrollback, of the person who ran the command.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .collect()
}

/// Resolves a host and port from a pairing link, bracketing an IPv6 literal.
///
/// A name as well as an address: the link may say `desk.local`, and a link that only
/// worked for an address would be a link that only worked on one network.
fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    use std::net::ToSocketAddrs;

    let text = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    text.to_socket_addrs()
        .with_context(|| format!("resolving {text} from the pairing link"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("{text} resolved to no address"))
}

/// Runs the synthetic sender until it is stopped.
pub fn run(args: SendArgs) -> Result<()> {
    if args.frame_ms == 0 || args.frame_ms > 1000 {
        bail!("--frame-ms must be between 1 and 1000");
    }
    if !(0.0..=1.0).contains(&args.amplitude) {
        bail!("--amplitude must be between 0.0 and 1.0");
    }
    if args.click && args.click_interval_ms < CLICK_MS as u32 * 2 {
        bail!("--click-interval-ms must be larger than the click itself");
    }
    if args.rate == 0 {
        bail!("--rate must be greater than zero");
    }
    if args.pace_ppm.unsigned_abs() > 50_000 {
        bail!("--pace-ppm must be within ±50 000 ppm (±5%)");
    }

    let frame_samples = (args.rate as u64 * args.frame_ms as u64 / 1000) as u16;
    if frame_samples == 0 {
        bail!("--rate and --frame-ms together produce zero samples per frame");
    }

    // The key decides whether there is any cryptography at all, so it is settled
    // before a socket is opened. No key means plaintext, matching the host's default:
    // the phone cannot encrypt yet, so the path that works has to be the one you get by
    // not asking for anything.
    let (psk, link) = match &args.key {
        Some(text) => {
            let (psk, link) = load_pairing(text)?;
            (Some(psk), link)
        }
        None => (None, None),
    };

    let audio_addr: SocketAddr = match args.to {
        Some(addr) => addr,
        None => match &link {
            Some(link) => resolve(&link.host, link.audio_port())?,
            None => DEFAULT_AUDIO_ADDR.parse().expect("the default address is valid"),
        },
    };
    let control_addr = match &link {
        Some(link) => resolve(&link.host, link.control_port)?,
        None => control_address_for(audio_addr),
    };

    let socket = UdpSocket::bind("0.0.0.0:0").context("binding a local UDP port")?;
    // Connecting a UDP socket makes `send` available and, more usefully, lets the
    // OS report ICMP "port unreachable" as an error instead of silently dropping
    // packets when nothing is listening.
    socket
        .connect(audio_addr)
        .with_context(|| format!("connecting to {audio_addr}"))?;

    if let Some(psk) = &psk {
        println!("  security    : AES-256-GCM, pairing with {control_addr} first");
        println!("  key         : {psk}");
    } else {
        println!("  security    : NONE — plaintext audio, no pairing (pass --key to encrypt)");
    }
    let bytes_per_sample = SampleFormat::F32
        .bytes_per_sample()
        .expect("F32 is four bytes by definition");
    let payload_bytes = frame_samples as usize * bytes_per_sample;
    let overhead = if psk.is_some() { AEAD_OVERHEAD } else { 0 };
    let wire_bytes = HEADER_LEN + payload_bytes + overhead;
    let frames_per_second = 1000.0 / args.frame_ms as f64;
    let bits_per_second = wire_bytes as f64 * frames_per_second * 8.0;

    println!("talkee send -> {audio_addr}");
    println!(
        "  {} Hz, {} ms frames ({} samples), {} - {:.0} kbps on the wire",
        args.rate,
        args.frame_ms,
        frame_samples,
        if args.click {
            format!("click every {} ms", args.click_interval_ms)
        } else {
            format!("{:.0} Hz tone", args.tone)
        },
        bits_per_second / 1000.0,
    );
    if args.click {
        println!("  Use this with the clap test in docs/LATENCY.md.");
    }
    if args.pace_ppm != 0 {
        println!(
            "  pacing      : {:+} ppm — {} than real time, standing in for a clock difference",
            args.pace_ppm,
            if args.pace_ppm > 0 { "faster" } else { "slower" }
        );
    }

    // Pair before sending a single datagram: the host drops everything from an
    // unpaired sender, so sending first would only look like packet loss.
    let mut pairing = match &psk {
        Some(psk) => Some(Pairing::establish(control_addr, psk)?),
        None => None,
    };

    // Absolute-deadline pacing, so a skewed `--pace-ppm` applies the same factor to
    // every deadline instead of accumulating rounding error.
    let pace = 1.0 - args.pace_ppm as f64 / 1e6;
    let frame_duration = Duration::from_secs_f64(args.frame_ms as f64 / 1000.0 * pace);
    let started = Instant::now();
    let deadline_total = if args.seconds == 0 {
        None
    } else {
        Some(Duration::from_secs(args.seconds as u64))
    };

    let mut datagram = Vec::with_capacity(wire_bytes);
    let mut plaintext = Vec::with_capacity(payload_bytes);
    let mut sealed = Vec::with_capacity(payload_bytes + AEAD_OVERHEAD);
    let mut seq: u32 = 0;
    let mut phase: f64 = 0.0;
    let mut sample_index: u64 = 0;
    let mut last_report = Instant::now();
    let mut frames_since_report: u64 = 0;
    loop {
        // Absolute-deadline pacing. Sleeping for a fixed duration each iteration
        // would accumulate the sleep overshoot and drift away from real time.
        let target = started + frame_duration * seq;
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        }

        let elapsed = started.elapsed();
        if let Some(total) = deadline_total {
            if elapsed >= total {
                break;
            }
        }

        // If the host went away — restarted, or crashed and came back — its control
        // connection is gone and the key we hold is no longer one it accepts.
        // Pairing again here is the difference between a five-second hiccup and a
        // run that reports success into a silent microphone.
        if let (Some(current), Some(psk)) = (pairing.as_ref(), psk.as_ref()) {
            if !current.is_alive() {
                println!("talkee: the host control connection dropped; pairing again");
                pairing = Some(Pairing::establish(control_addr, psk)?);
            }
        }

        let click_period = args.rate as u64 * args.click_interval_ms as u64 / 1000;
        let click_len = (args.rate as u64 * CLICK_MS / 1000).max(1);
        let tone_step = args.tone / args.rate as f64;

        plaintext.clear();
        for _ in 0..frame_samples {
            let value = if args.click {
                if sample_index % click_period < click_len {
                    // Alternate the sign so the burst has no DC offset, which
                    // would otherwise land as a thump on some playback paths.
                    if (sample_index / 2) % 2 == 0 {
                        args.amplitude
                    } else {
                        -args.amplitude
                    }
                } else {
                    0.0
                }
            } else {
                let v = (phase * std::f64::consts::TAU).sin() as f32 * args.amplitude;
                phase += tone_step;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
                v
            };
            plaintext.extend_from_slice(&value.to_le_bytes());
            sample_index += 1;
        }

        let header = AudioHeader::new(
            seq,
            elapsed.as_millis() as u32,
            args.rate,
            frame_samples,
            1,
            SampleFormat::F32,
            if psk.is_some() { flags::ENCRYPTED } else { 0 },
        );
        datagram.clear();
        datagram.extend_from_slice(&header.to_bytes());
        match pairing.as_mut() {
            Some(pairing) => {
                // The header is the associated data, so the host authenticates every
                // field in it along with the audio: a frame whose sample rate, format
                // or payload length was edited in flight does not open.
                sealed.clear();
                pairing.session.seal(seq, &datagram, &plaintext, &mut sealed)?;
                datagram.extend_from_slice(&sealed);
            }
            None => datagram.extend_from_slice(&plaintext),
        }

        socket
            .send(&datagram)
            .with_context(|| format!("sending frame {seq} to {audio_addr}"))?;
        seq = seq.wrapping_add(1);
        frames_since_report += 1;

        if last_report.elapsed() >= REPORT_EVERY {
            let seconds = last_report.elapsed().as_secs_f64();
            println!(
                "sent {:>7} frames | {:.0} fps | {} bytes/frame | {:.0} kbps",
                seq,
                frames_since_report as f64 / seconds,
                wire_bytes,
                bits_per_second / 1000.0,
            );
            last_report = Instant::now();
            frames_since_report = 0;
        }
    }

    println!("talkee send: sent {seq} frames, stopping");
    Ok(())
}