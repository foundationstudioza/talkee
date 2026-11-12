//! `talkee dictate`: try the dictation pipeline from a terminal.
//!
//! Records from this computer's default microphone until Enter is pressed (or for `--seconds`), or
//! reads a WAV file, then prints the raw transcript, the cleaned text, and how long each step took.
//! Nothing is typed anywhere; this is for testing providers, models, and the cleanup prompt.

use anyhow::{bail, Context, Result};
use clap::Args;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, InputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use talkee_dictate::{audio, AudioFormat, Client, Dictation, Keys, Provider};

/// The longest recording a dictation may be.
const MAX_SECONDS: f32 = 300.0;

#[derive(Args)]
pub struct DictateArgs {
    /// Transcribe this 16-bit PCM WAV file instead of recording.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Record for this many seconds instead of until Enter.
    #[arg(long)]
    seconds: Option<f32>,
    /// Speech-to-text provider: groq or openrouter.
    #[arg(long, default_value = "groq")]
    provider: String,
    /// Speech-to-text model (default: the provider's Whisper Large V3 Turbo).
    #[arg(long)]
    model: Option<String>,
    /// Cleanup provider (default: the speech-to-text provider).
    #[arg(long)]
    cleanup_provider: Option<String>,
    /// Cleanup model (default: the provider's default cleanup model).
    #[arg(long)]
    cleanup_model: Option<String>,
    /// Print the raw transcript only.
    #[arg(long)]
    no_cleanup: bool,
    /// Extra cleanup instructions.
    #[arg(long)]
    instructions: Option<String>,
    /// Language code such as "en" (default: detect).
    #[arg(long)]
    language: Option<String>,
    /// A name or term to spell right; repeat for several.
    #[arg(long = "word")]
    vocabulary: Vec<String>,
    /// Upload WAV instead of Ogg Opus (to compare speed and accuracy).
    #[arg(long)]
    wav: bool,
    /// Also save the recording here as a WAV file, to replay with --file.
    #[arg(long)]
    save: Option<PathBuf>,
}

pub fn run(args: DictateArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let (keys, env_file) = Keys::load(&cwd);
    let client_for = |name: &str| -> Result<Client> {
        let provider = Provider::parse(name)
            .with_context(|| format!("unknown provider {name:?}; use groq or openrouter"))?;
        let key = match provider {
            Provider::Groq => keys.groq.clone(),
            Provider::OpenRouter => keys.openrouter.clone(),
        };
        let Some(key) = key else {
            let var = match provider {
                Provider::Groq => talkee_dictate::keys::GROQ,
                Provider::OpenRouter => talkee_dictate::keys::OPENROUTER,
            };
            let place = env_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "a .env file in this folder".into());
            bail!("no {} key: set {var} in {place}", provider.name());
        };
        Ok(Client::new(provider, key))
    };

    let transcriber = client_for(&args.provider)?;
    let cleanup = if args.no_cleanup {
        None
    } else {
        let client = client_for(args.cleanup_provider.as_deref().unwrap_or(&args.provider))?;
        let model = args
            .cleanup_model
            .clone()
            .unwrap_or_else(|| client.provider().default_cleanup_model().to_string());
        Some((client, model))
    };
    let dictation = Dictation {
        transcribe_model: args.model.clone().unwrap_or_else(|| {
            transcriber
                .provider()
                .default_transcribe_model()
                .to_string()
        }),
        transcriber,
        cleanup,
        cleanup_instructions: args.instructions.clone(),
        language: args.language.clone(),
        // The saved vocabulary (the one the app's settings edit), plus any --word given here.
        vocabulary: {
            let mut terms = talkee_store::Store::open()
                .map(|store| talkee_dictate::config::load_vocabulary(store.root()))
                .unwrap_or_default();
            terms.extend(args.vocabulary.iter().cloned());
            terms
        },
        format: if args.wav {
            AudioFormat::Wav
        } else {
            AudioFormat::OggOpus
        },
    };

    let (samples, rate) = match &args.file {
        Some(path) => {
            let bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            let decoded =
                audio::read_wav(&bytes).with_context(|| format!("decoding {}", path.display()))?;
            (decoded.samples, decoded.rate)
        }
        None => record(args.seconds)?,
    };
    if let Some(path) = &args.save {
        std::fs::write(path, audio::wav_16k(&audio::to_16k(&samples, rate)))
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!("saved the recording to {}", path.display());
    }

    eprintln!(
        "transcribing with {} ({}){}",
        dictation.transcriber.provider().name(),
        dictation.transcribe_model,
        match &dictation.cleanup {
            Some((client, model)) =>
                format!(", cleaning up with {} ({model})", client.provider().name()),
            None => String::new(),
        }
    );
    let outcome = dictation.run(&samples, rate)?;
    println!(
        "--- raw ({:.1} s audio, {} KB, {} ms) ---",
        outcome.audio_seconds,
        outcome.upload_bytes / 1024,
        outcome.transcribe_time.as_millis()
    );
    println!("{}", outcome.raw);
    if dictation.cleanup.is_some() {
        println!("--- cleaned ({} ms) ---", outcome.cleanup_time.as_millis());
        println!("{}", outcome.text);
        if let Some(note) = &outcome.cleanup_note {
            eprintln!("note: {note}");
        }
    }
    Ok(())
}

/// Records mono samples from the default input device. Returns the samples and their rate.
fn record(seconds: Option<f32>) -> Result<(Vec<f32>, u32)> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("this computer has no default microphone")?;
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "the default microphone".into());
    let supported = device
        .default_input_config()
        .with_context(|| format!("querying the input format of {name}"))?;
    let format = supported.sample_format();
    let channels = supported.channels();
    let rate = supported.sample_rate();
    let config: StreamConfig = supported.into();

    let buffer = Arc::new(Mutex::new(Vec::<f32>::new()));
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, config, channels, Arc::clone(&buffer)),
        SampleFormat::I16 => build::<i16>(&device, config, channels, Arc::clone(&buffer)),
        SampleFormat::U16 => build::<u16>(&device, config, channels, Arc::clone(&buffer)),
        SampleFormat::I32 => build::<i32>(&device, config, channels, Arc::clone(&buffer)),
        other => bail!("{name} uses sample format {other:?}, which is not supported"),
    }?;
    stream.play().with_context(|| format!("starting {name}"))?;

    let limit = seconds.unwrap_or(MAX_SECONDS).clamp(0.5, MAX_SECONDS);
    match seconds {
        Some(s) => {
            eprintln!("recording from {name} for {s} s…");
            std::thread::sleep(Duration::from_secs_f32(limit));
        }
        None => {
            eprintln!("recording from {name}; press Enter to stop (max {MAX_SECONDS} s)");
            let started = Instant::now();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(Duration::from_secs_f32(limit));
            eprintln!("recorded {:.1} s", started.elapsed().as_secs_f32());
        }
    }
    drop(stream);
    let samples = std::mem::take(&mut *buffer.lock().unwrap());
    Ok((samples, rate))
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    channels: u16,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = channels.max(1) as usize;
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _info: &InputCallbackInfo| {
            let mut out = buffer.lock().unwrap();
            for frame in data.chunks_exact(channels) {
                let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                out.push(sum / channels as f32);
            }
        },
        move |error| eprintln!("talkee: microphone error: {error}"),
        None,
    )?;
    Ok(stream)
}
