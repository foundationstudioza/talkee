//! Command line parsing.
//!
//! Hand-rolled rather than pulled from `clap`: this is one binary with eight flags, and
//! keeping the parser here means the whole surface — including the error messages a user
//! will actually read — is testable without spawning a process.

use std::net::SocketAddr;
use std::time::Duration;

use crate::engine::EngineConfig;

/// What the app was told on the command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    /// Address to receive audio on.
    pub listen: SocketAddr,
    /// Render endpoint to play into, overriding the detected cable.
    pub device: Option<String>,
    /// Depth of the output queue, in milliseconds.
    pub buffer_ms: u32,
    /// Linear gain applied to incoming audio.
    pub gain: f32,
    /// Requested device buffer size, if any.
    pub buffer_frames: Option<u32>,
    /// Whether to announce this PC over mDNS.
    pub advertise: bool,
    /// Whether to try to create a tray icon.
    pub tray: bool,
    /// Refuse unencrypted audio and require a paired phone — which is the default.
    ///
    /// The flag exists *inverted* so that the secure path is what you get by saying nothing: a
    /// default that has to be opted into is a default that nobody uses.
    pub no_pairing: bool,
    /// Show a console with the live log instead of writing it to a file.
    pub console: bool,
    /// Do not look for updates.
    pub no_update_check: bool,
    /// Leave the speakers' sound in the microphone. Inverted for the same reason as `no_pairing`.
    pub no_echo_cancel: bool,
    /// Skip noise suppression. Inverted like the other opt-outs: on is what you get by saying nothing.
    pub no_noise_suppression: bool,
    /// Stop cleanly after this long. For scripted verification, which has no way to click
    /// a tray item; not something a user needs.
    pub exit_after: Option<Duration>,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run the app with these settings.
    Run(Box<Args>),
    /// Print the usage and exit successfully.
    Help,
}

/// The usage text, which is also what `--help` prints.
pub const USAGE: &str = "\
talkee-app — run the Talkee receiver with a tray icon

USAGE:
    talkee-app [OPTIONS]

OPTIONS:
    --device <NAME>        render endpoint to play into. Defaults to the first
                           virtual cable found, which is right almost always.
    --listen <ADDR>        address to receive audio on [default: 0.0.0.0:47801]
    --buffer-ms <MS>       audio buffered before playback starts [default: 60]
    --gain <FACTOR>        linear gain applied to incoming audio [default: 1]
    --buffer-frames <N>    device buffer size; omit for the device default
    --no-advertise         do not announce this PC over mDNS
    --no-tray              run headless and print state transitions only
    --no-noise-suppression keep background sound; use when loud music is playing, where suppression can cut quiet speech
    --no-update-check      do not look for new versions
    --console              show the live log in a console instead of talkee.log
    --no-echo-cancel       do not remove the PC's own speaker sound from the microphone
    --no-pairing           accept unencrypted audio from anyone who can reach this
    --exit-after <SECS>    stop cleanly after this long (for scripted runs)
    -h, --help             print this";

impl Args {
    /// Parses the command line.
    pub fn parse(argv: &[String]) -> std::result::Result<Command, String> {
        let mut args = Args {
            listen: "0.0.0.0:47801".parse().expect("static address is valid"),
            device: None,
            buffer_ms: 60,
            gain: 1.0,
            buffer_frames: None,
            advertise: true,
            tray: true,
            no_pairing: false,
            console: false,
            no_update_check: false,
            no_echo_cancel: false,
            no_noise_suppression: false,
            exit_after: None,
        };

        let mut index = 0;
        while index < argv.len() {
            match argv[index].as_str() {
                "-h" | "--help" => return Ok(Command::Help),
                "--listen" => {
                    let raw = take_value(argv, &mut index, "--listen")?;
                    args.listen = raw
                        .parse()
                        .map_err(|e| format!("--listen {raw} is not an address: {e}"))?;
                }
                "--device" => {
                    let raw = take_value(argv, &mut index, "--device")?;
                    if raw.trim().is_empty() {
                        return Err("--device needs a name".to_string());
                    }
                    args.device = Some(raw);
                }
                "--buffer-ms" => {
                    let raw = take_value(argv, &mut index, "--buffer-ms")?;
                    let ms: u32 = raw
                        .parse()
                        .map_err(|e| format!("--buffer-ms {raw} is not a number: {e}"))?;
                    if ms == 0 {
                        // A zero-depth queue never opens its prefill gate, so it is a
                        // receiver that produces silence forever. Rejected here, where the
                        // message can still say which flag caused it.
                        return Err("--buffer-ms must be greater than zero".to_string());
                    }
                    args.buffer_ms = ms;
                }
                "--gain" => {
                    let raw = take_value(argv, &mut index, "--gain")?;
                    let gain: f32 = raw
                        .parse()
                        .map_err(|e| format!("--gain {raw} is not a number: {e}"))?;
                    // A non-finite gain turns every sample into NaN, which plays as silence
                    // and looks exactly like a broken microphone.
                    if !gain.is_finite() || gain < 0.0 {
                        return Err("--gain must be a finite value >= 0".to_string());
                    }
                    args.gain = gain;
                }
                "--buffer-frames" => {
                    let raw = take_value(argv, &mut index, "--buffer-frames")?;
                    let frames: u32 = raw
                        .parse()
                        .map_err(|e| format!("--buffer-frames {raw} is not a number: {e}"))?;
                    if frames == 0 {
                        return Err("--buffer-frames must be greater than zero".to_string());
                    }
                    args.buffer_frames = Some(frames);
                }
                "--no-advertise" => args.advertise = false,
                "--no-tray" => args.tray = false,
                "--no-pairing" => args.no_pairing = true,
                "--no-echo-cancel" => args.no_echo_cancel = true,
                "--console" => args.console = true,
                "--no-update-check" => args.no_update_check = true,
                "--no-noise-suppression" => args.no_noise_suppression = true,
                "--exit-after" => {
                    let raw = take_value(argv, &mut index, "--exit-after")?;
                    let seconds: f64 = raw
                        .parse()
                        .map_err(|e| format!("--exit-after {raw} is not a number: {e}"))?;
                    if !seconds.is_finite() || seconds <= 0.0 {
                        return Err("--exit-after must be a positive number of seconds".to_string());
                    }
                    args.exit_after = Some(Duration::from_secs_f64(seconds));
                }
                other => return Err(format!("unknown option {other}\n\n{USAGE}")),
            }
            index += 1;
        }
        Ok(Command::Run(Box::new(args)))
    }

    /// The engine configuration this command line asks for.
    ///
    /// A copy rather than a shared struct: the command line is what the user typed, the
    /// config is what the engine needs, and keeping them apart means neither has to grow a
    /// field it does not understand. The `..config` keeps the defaults that have no flag.
    pub fn apply_to(&self, config: EngineConfig) -> EngineConfig {
        EngineConfig {
            listen: self.listen,
            device: self.device.clone(),
            gain: self.gain,
            buffer_ms: self.buffer_ms,
            buffer_frames: self.buffer_frames,
            advertise: self.advertise,
            no_pairing: self.no_pairing,
            echo_cancel: !self.no_echo_cancel,
            noise_suppression: !self.no_noise_suppression,
            ..config
        }
    }
}

/// Reads the value that follows a flag and steps past it.
fn take_value(
    argv: &[String],
    index: &mut usize,
    flag: &str,
) -> std::result::Result<String, String> {
    *index += 1;
    argv.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn run_args(items: &[&str]) -> Args {
        match Args::parse(&argv(items)).expect("these arguments are valid") {
            Command::Run(args) => *args,
            Command::Help => panic!("expected a run, not help"),
        }
    }

    #[test]
    fn the_defaults_match_what_talkee_host_does() {
        // The tray app replaces `talkee host`, so someone who just double-clicks it has to
        // get the same pipeline the CLI gives them.
        let args = run_args(&[]);
        assert_eq!(args.listen, "0.0.0.0:47801".parse().expect("valid"));
        assert_eq!(args.buffer_ms, 60);
        assert_eq!(args.gain, 1.0);
        assert!(args.device.is_none(), "the first virtual cable is found for us");
        assert!(args.advertise, "announcing is what makes the phone side zero-config");
        assert!(args.tray);
        assert!(args.exit_after.is_none());
    }

    #[test]
    fn every_flag_parses() {
        let args = run_args(&[
            "--listen",
            "127.0.0.1:5000",
            "--device",
            "CABLE Input",
            "--buffer-ms",
            "80",
            "--gain",
            "0.5",
            "--buffer-frames",
            "256",
            "--no-advertise",
            "--no-tray",
            "--no-pairing",
            "--exit-after",
            "2.5",
        ]);
        assert_eq!(args.listen, "127.0.0.1:5000".parse().expect("valid"));
        assert_eq!(args.device.as_deref(), Some("CABLE Input"));
        assert_eq!(args.buffer_ms, 80);
        assert_eq!(args.gain, 0.5);
        assert_eq!(args.buffer_frames, Some(256));
        assert!(!args.advertise);
        assert!(!args.tray);
        assert!(args.no_pairing, "the opt-in to accepting unencrypted audio");
        assert_eq!(args.exit_after, Some(Duration::from_millis(2500)));
    }

    #[test]
    fn the_private_path_is_what_you_get_by_saying_nothing() {
        // The one default that matters: somebody who launches this app and scans nothing must not be
        // accepting audio from anyone who can reach the port. Pairing is what the tray's code is for,
        // so it has to be the default — a default that has to be opted into is one nobody finds.
        let args = run_args(&[]);
        assert!(!args.no_pairing);
        let config = args.apply_to(crate::engine::EngineConfig::default());
        assert!(!config.no_pairing);
        assert_eq!(
            config.security(),
            talkee_core::stream::SecurityPolicy::RequireEncrypted
        );
    }

    #[test]
    fn help_is_its_own_command() {
        assert_eq!(Args::parse(&argv(&["--help"])).expect("ok"), Command::Help);
        assert_eq!(Args::parse(&argv(&["-h"])).expect("ok"), Command::Help);
    }

    #[test]
    fn a_bad_option_is_rejected_with_the_flag_in_the_message() {
        // Someone who mistypes a flag should be told which one, not handed a wall of text.
        let error = Args::parse(&argv(&["--nope"])).expect_err("unknown flag");
        assert!(error.contains("--nope"), "{error}");
        assert!(error.contains("USAGE"), "{error}");

        let error = Args::parse(&argv(&["--listen"])).expect_err("missing value");
        assert!(error.contains("--listen"), "{error}");
    }

    #[test]
    fn values_that_would_produce_silent_audio_are_rejected_at_the_door() {
        for bad in [
            vec!["--buffer-ms", "0"],
            vec!["--gain", "nan"],
            vec!["--gain", "-1"],
            vec!["--gain", "inf"],
            vec!["--buffer-frames", "0"],
            vec!["--exit-after", "0"],
            vec!["--listen", "not-an-address"],
            vec!["--buffer-ms", "many"],
            vec!["--device", "   "],
        ] {
            assert!(
                Args::parse(&argv(&bad)).is_err(),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn the_command_line_and_the_engine_config_never_disagree() {
        let args = run_args(&["--buffer-ms", "100", "--no-advertise"]);
        let config = args.apply_to(crate::engine::EngineConfig::default());
        assert_eq!(config.listen, args.listen);
        assert_eq!(config.buffer_ms, 100);
        assert!(!config.advertise);
        assert!(
            !config.instance.trim().is_empty(),
            "the mDNS name comes from the default and is never empty"
        );
    }
}

