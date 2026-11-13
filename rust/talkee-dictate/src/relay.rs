//! The Talkee relay: how installed apps dictate without carrying any API key.
//!
//! The relay (`relay/` in this repository, on Netlify) holds the Groq and OpenRouter keys. An app
//! sends it one Ogg Opus recording and gets back the cleaned text in one round trip; the relay does
//! the transcription and the cleanup, charges the seconds against the device's daily allowance, and
//! falls back to OpenRouter when Groq is unavailable.
//!
//! A device is a random id made once per install (see [`crate::config::device_id`]). It is not a
//! secret and proves nothing; it only lets the relay count one person's minutes.

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::time::Duration;

/// Where installed apps send dictations unless `dictation.json` says otherwise.
pub const DEFAULT_URL: &str = "https://talkee-relay.netlify.app";

/// Whole-request deadline: transcription plus cleanup, for up to five minutes of audio.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A relay endpoint and this install's id.
#[derive(Clone)]
pub struct Relay {
    url: String,
    device: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay").field("url", &self.url).finish_non_exhaustive()
    }
}

/// What the relay sent back.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The transcript before cleanup.
    pub raw: String,
    /// The text to deliver.
    pub text: String,
    /// Why cleanup was skipped, if it was.
    pub cleanup_note: Option<String>,
    /// Seconds this device has used today, including this dictation.
    pub used_seconds: u32,
    /// Seconds a device gets per day.
    pub limit_seconds: u32,
}

/// What the relay is told besides the audio.
#[derive(Debug, Clone, Default)]
pub struct Options<'a> {
    /// ISO-639-1 language, or `None` to detect.
    pub language: Option<&'a str>,
    /// Names and terms to spell right.
    pub vocabulary: &'a [String],
    /// Rewrite the transcript as finished text.
    pub cleanup: bool,
    /// Extra cleanup instructions.
    pub instructions: Option<&'a str>,
}

impl Relay {
    /// A relay at `url` (no trailing path), for the install with id `device`.
    pub fn new(url: impl Into<String>, device: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            // The recording goes to the relay the app was built for, or nowhere.
            .redirects(0)
            .user_agent(concat!("talkee/", env!("CARGO_PKG_VERSION")))
            .build();
        Relay {
            url: url.into().trim_end_matches('/').to_string(),
            device: device.into(),
            agent,
        }
    }

    /// Sends one Ogg Opus recording; returns the text.
    pub fn dictate(&self, ogg: &[u8], options: &Options) -> Result<Reply> {
        let header = json!({
            "language": options.language,
            "vocabulary": options.vocabulary,
            "cleanup": options.cleanup,
            "instructions": options.instructions.unwrap_or(""),
        });
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header.to_string());
        let response = self
            .agent
            .post(&format!("{}/api/dictate", self.url))
            .timeout(TIMEOUT)
            .set("Content-Type", "audio/ogg")
            .set("X-Talkee-Device", &self.device)
            .set("X-Talkee-Options", &header)
            .send_bytes(ogg);
        let value: Value = match response {
            Ok(response) => serde_json::from_str(
                &response.into_string().context("reading the relay's answer")?,
            )
            .context("the relay's answer is not JSON")?,
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                let message = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| format!("the dictation service answered {code}"));
                bail!("{message}");
            }
            Err(ureq::Error::Transport(error)) => {
                bail!("could not reach the dictation service: {error}")
            }
        };
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let number = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
        Ok(Reply {
            raw: text("raw").unwrap_or_default(),
            text: text("text").context("the relay sent no text")?,
            cleanup_note: text("cleanupNote"),
            used_seconds: number("usedSeconds"),
            limit_seconds: number("limitSeconds"),
        })
    }
}
