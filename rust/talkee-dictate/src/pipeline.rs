//! One dictation, start to finish.

use anyhow::{anyhow, Result};
use std::time::{Duration, Instant};

use crate::audio;
use crate::cleanup;
use crate::ogg_opus;
use crate::provider::{AudioFile, AudioFormat, Client, TranscribeOptions};

/// How a dictation is turned into text. Built once from settings and reused.
#[derive(Debug, Clone)]
pub struct Dictation {
    /// Speech-to-text provider.
    pub transcriber: Client,
    /// Speech-to-text model.
    pub transcribe_model: String,
    /// Cleanup provider and model, or `None` to type the raw transcript.
    pub cleanup: Option<(Client, String)>,
    /// Extra cleanup instructions from the user.
    pub cleanup_instructions: Option<String>,
    /// ISO-639-1 language, or `None` to detect.
    pub language: Option<String>,
    /// Names and terms to spell right.
    pub vocabulary: Vec<String>,
    /// How the recording is uploaded. Ogg Opus unless testing.
    pub format: AudioFormat,
}

/// What a dictation produced.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The transcript as the speech-to-text model returned it.
    pub raw: String,
    /// The text to deliver: cleaned, or the raw transcript when cleanup is off or failed.
    pub text: String,
    /// Why cleanup was skipped or discarded, when it was on. The raw transcript was used instead.
    pub cleanup_note: Option<String>,
    /// Seconds of audio sent, after trimming silence.
    pub audio_seconds: f32,
    /// Bytes uploaded.
    pub upload_bytes: usize,
    /// Time spent in speech-to-text.
    pub transcribe_time: Duration,
    /// Time spent in cleanup.
    pub cleanup_time: Duration,
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
        let file = match self.format {
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

        let hint = (!self.vocabulary.is_empty()).then(|| self.vocabulary.join(", "));
        let started = Instant::now();
        let raw = self.transcriber.transcribe(
            &file,
            &self.transcribe_model,
            &TranscribeOptions {
                language: self.language.as_deref(),
                hint: hint.as_deref(),
            },
        )?;
        let transcribe_time = started.elapsed();

        let started = Instant::now();
        let (text, cleanup_note) = match &self.cleanup {
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
        })
    }
}
