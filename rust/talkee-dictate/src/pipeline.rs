//! One dictation, start to finish.

use anyhow::{anyhow, Result};
use std::time::{Duration, Instant};

use crate::audio;
use crate::cleanup;
use crate::ogg_opus;
use crate::provider::{AudioFile, AudioFormat, Client, TranscribeOptions};
use crate::relay::{self, Relay};

/// Where the recording goes.
#[derive(Debug, Clone)]
pub enum Backend {
    /// The Talkee relay, which holds the API keys: what every installed app uses.
    Relay {
        /// The relay.
        relay: Relay,
        /// Ask the relay to clean the transcript up.
        cleanup: bool,
    },
    /// Straight to Groq or OpenRouter with this machine's own keys. For development.
    Direct {
        /// Speech-to-text provider.
        transcriber: Client,
        /// Speech-to-text model.
        transcribe_model: String,
        /// Cleanup provider and model, or `None` to type the raw transcript.
        cleanup: Option<(Client, String)>,
    },
}

/// How a dictation is turned into text. Built from settings at the start of each dictation.
#[derive(Debug, Clone)]
pub struct Dictation {
    /// Where the recording goes.
    pub backend: Backend,
    /// Extra cleanup instructions from the user.
    pub cleanup_instructions: Option<String>,
    /// ISO-639-1 language, or `None` to detect.
    pub language: Option<String>,
    /// Names and terms to spell right.
    pub vocabulary: Vec<String>,
    /// How the recording is uploaded. Ogg Opus unless testing (the relay only takes Ogg Opus).
    pub format: AudioFormat,
}

/// What a dictation produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The transcript as the speech-to-text model returned it.
    pub raw: String,
    /// The text to deliver: cleaned, or the raw transcript when cleanup is off or failed. One line.
    pub text: String,
    /// Why cleanup was skipped or discarded, when it was on. The raw transcript was used instead.
    pub cleanup_note: Option<String>,
    /// Seconds of audio sent, after trimming silence.
    pub audio_seconds: f32,
    /// Bytes uploaded.
    pub upload_bytes: usize,
    /// Time spent in speech-to-text (with the relay: the whole round trip).
    pub transcribe_time: Duration,
    /// Time spent in cleanup (zero with the relay, which does both in one call).
    pub cleanup_time: Duration,
    /// Seconds used today and the daily allowance, when the relay reports them.
    pub usage: Option<(u32, u32)>,
}

impl Dictation {
    /// Transcribes mono samples at `rate`, then cleans the result up.
    ///
    /// Fails before any network request when the recording is too short or silent, so silence is
    /// never turned into invented text.
    pub fn run(&self, mono: &[f32], rate: u32) -> Result<Outcome> {
        let resampled = audio::to_16k(mono, rate);
        let samples = audio::trim_silence(&resampled);
        audio::check(samples).map_err(|reason| anyhow!("{reason}"))?;
        let format = match self.backend {
            Backend::Relay { .. } => AudioFormat::OggOpus,
            Backend::Direct { .. } => self.format,
        };
        let file = match format {
            AudioFormat::Wav => AudioFile {
                bytes: audio::wav_16k(samples),
                format: AudioFormat::Wav,
            },
            AudioFormat::OggOpus => AudioFile {
                bytes: ogg_opus::encode(samples)?,
                format: AudioFormat::OggOpus,
            },
        };
        let audio_seconds = samples.len() as f32 / audio::TARGET_RATE as f32;

        let (transcriber, transcribe_model, cleanup) = match &self.backend {
            Backend::Relay { relay, cleanup } => {
                let started = Instant::now();
                let reply = relay.dictate(
                    &file.bytes,
                    &relay::Options {
                        language: self.language.as_deref(),
                        vocabulary: &self.vocabulary,
                        cleanup: *cleanup,
                        instructions: self.cleanup_instructions.as_deref(),
                    },
                )?;
                return Ok(Outcome {
                    raw: reply.raw,
                    text: cleanup::single_line(&reply.text),
                    cleanup_note: reply.cleanup_note,
                    audio_seconds,
                    upload_bytes: file.bytes.len(),
                    transcribe_time: started.elapsed(),
                    cleanup_time: Duration::ZERO,
                    usage: Some((reply.used_seconds, reply.limit_seconds)),
                });
            }
            Backend::Direct {
                transcriber,
                transcribe_model,
                cleanup,
            } => (transcriber, transcribe_model, cleanup),
        };

        let hint = (!self.vocabulary.is_empty()).then(|| self.vocabulary.join(", "));
        let started = Instant::now();
        let raw = transcriber.transcribe(
            &file,
            transcribe_model,
            &TranscribeOptions {
                language: self.language.as_deref(),
                hint: hint.as_deref(),
            },
        )?;
        let transcribe_time = started.elapsed();

        let started = Instant::now();
        let (text, cleanup_note) = match cleanup {
            None => (raw.clone(), None),
            Some(_) if raw.is_empty() => (raw.clone(), None),
            Some((client, model)) => {
                let (system, user) =
                    cleanup::messages(&raw, &self.vocabulary, self.cleanup_instructions.as_deref());
                // Cleanup is an improvement, not a dependency: if it fails, the user still gets
                // their words.
                match client.chat(model, &system, &user, cleanup::max_tokens(&raw)) {
                    Ok(cleaned) => match cleanup::accept(&raw, &cleaned) {
                        Some(text) => (text, None),
                        None => (
                            raw.clone(),
                            Some(
                                "cleanup changed the text too much; used the raw transcript".into(),
                            ),
                        ),
                    },
                    Err(error) => (raw.clone(), Some(format!("{error:#}"))),
                }
            }
        };
        // Always one line, whichever path the text took: see `cleanup::single_line`.
        let text = cleanup::single_line(&text);
        Ok(Outcome {
            raw,
            text,
            cleanup_note,
            audio_seconds,
            upload_bytes: file.bytes.len(),
            transcribe_time,
            cleanup_time: started.elapsed(),
            usage: None,
        })
    }
}
