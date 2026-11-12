//! The two cloud providers: speech-to-text and chat completions.
//!
//! Groq takes the OpenAI-style multipart upload; OpenRouter takes JSON with base64 audio. Both
//! answer chat completions in the OpenAI shape.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::time::Duration;

/// Whole-request deadline for a transcription. Five minutes of audio is about 10 MB.
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(90);
/// Whole-request deadline for a cleanup.
const CHAT_TIMEOUT: Duration = Duration::from_secs(45);
/// How much of an error body is kept for the message the user sees.
const ERROR_BODY_LIMIT: usize = 400;

/// A cloud provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// api.groq.com
    Groq,
    /// openrouter.ai
    OpenRouter,
}

impl Provider {
    /// Parses `groq` or `openrouter`.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "groq" => Some(Provider::Groq),
            "openrouter" => Some(Provider::OpenRouter),
            _ => None,
        }
    }

    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Groq => "Groq",
            Provider::OpenRouter => "OpenRouter",
        }
    }

    fn base(self) -> &'static str {
        match self {
            Provider::Groq => "https://api.groq.com/openai/v1",
            Provider::OpenRouter => "https://openrouter.ai/api/v1",
        }
    }

    /// The default speech-to-text model.
    pub fn default_transcribe_model(self) -> &'static str {
        match self {
            Provider::Groq => "whisper-large-v3-turbo",
            Provider::OpenRouter => "openai/whisper-large-v3-turbo",
        }
    }

    /// The default cleanup model.
    ///
    /// Chosen on 2026-11-12 from what Groq serves, on test dictations with corrections, a spoken
    /// list, a question, and an email address: Qwen3.8 27B with thinking off was the fastest
    /// (350–550 ms) and the only one that kept every sentence; gpt-oss-20b and -120b dropped clauses.
    pub fn default_cleanup_model(self) -> &'static str {
        match self {
            Provider::Groq => "qwen/qwen3.8-27b",
            Provider::OpenRouter => "qwen/qwen3.8-27b",
        }
    }

    /// Whether the speech-to-text endpoint uses a spelling `prompt`. OpenRouter accepts the field
    /// but ignores it.
    pub fn uses_transcribe_prompt(self) -> bool {
        self == Provider::Groq
    }
}

/// A provider plus the key to use it.
#[derive(Clone)]
pub struct Client {
    provider: Provider,
    key: String,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// A container format the speech-to-text APIs accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// 16-bit PCM WAV.
    Wav,
    /// Opus in Ogg.
    OggOpus,
}

impl AudioFormat {
    /// File extension, which is also OpenRouter's `format` value.
    pub fn extension(self) -> &'static str {
        match self {
            AudioFormat::Wav => "wav",
            AudioFormat::OggOpus => "ogg",
        }
    }

    /// MIME type for the multipart upload.
    pub fn mime(self) -> &'static str {
        match self {
            AudioFormat::Wav => "audio/wav",
            AudioFormat::OggOpus => "audio/ogg",
        }
    }
}

/// An encoded recording, ready to upload.
#[derive(Debug, Clone)]
pub struct AudioFile {
    /// The file's bytes.
    pub bytes: Vec<u8>,
    /// Its format.
    pub format: AudioFormat,
}

/// What a speech-to-text call is told besides the audio.
#[derive(Debug, Clone, Default)]
pub struct TranscribeOptions<'a> {
    /// ISO-639-1 code, or `None` to let the model detect it.
    pub language: Option<&'a str>,
    /// Spellings to prefer: names, jargon. Ignored by providers that do not use it.
    pub hint: Option<&'a str>,
}

impl Client {
    /// A client for `provider` that authenticates with `key`.
    pub fn new(provider: Provider, key: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            // Never follow a redirect: the request carries the key and the user's voice, and both
            // go to the host the user chose or nowhere.
            .redirects(0)
            .user_agent(concat!("talkee/", env!("CARGO_PKG_VERSION")))
            .build();
        Client {
            provider,
            key: key.into(),
            agent,
        }
    }

    /// The provider this client talks to.
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// Transcribes an audio file.
    pub fn transcribe(
        &self,
        audio: &AudioFile,
        model: &str,
        opts: &TranscribeOptions,
    ) -> Result<String> {
        let url = format!("{}/audio/transcriptions", self.provider.base());
        let request = self
            .agent
            .post(&url)
            .timeout(TRANSCRIBE_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.key));
        let response = match self.provider {
            Provider::Groq => {
                let mut form = Multipart::new();
                form.text("model", model);
                form.text("response_format", "json");
                form.text("temperature", "0");
                if let Some(language) = opts.language {
                    form.text("language", language);
                }
                if let Some(hint) = opts.hint.filter(|h| !h.trim().is_empty()) {
                    form.text("prompt", hint);
                }
                form.file(
                    "file",
                    &format!("dictation.{}", audio.format.extension()),
                    audio.format.mime(),
                    &audio.bytes,
                );
                let (content_type, body) = form.finish();
                request.set("Content-Type", &content_type).send_bytes(&body)
            }
            Provider::OpenRouter => {
                let mut body = json!({
                    "model": model,
                    "input_audio": {
                        "data": base64::engine::general_purpose::STANDARD.encode(&audio.bytes),
                        "format": audio.format.extension(),
                    },
                    "temperature": 0,
                });
                if let Some(language) = opts.language {
                    body["language"] = json!(language);
                }
                request
                    .set("Content-Type", "application/json")
                    .send_string(&body.to_string())
            }
        };
        let value = self.read(response, "transcription")?;
        let text = value
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{} returned no transcript", self.provider.name()))?;
        Ok(text.trim().to_string())
    }

    /// One chat completion: a system message, a user message, the reply text.
    ///
    /// `max_tokens` caps the reply. Set it close to what is needed: Groq's free tier counts the
    /// cap, not the reply, against a per-minute output budget (1,000 tokens a minute for
    /// `qwen/qwen3.8-27b` on 2026-11-12), and the model's default cap alone exceeded it.
    pub fn chat(&self, model: &str, system: &str, user: &str, max_tokens: u32) -> Result<String> {
        let url = format!("{}/chat/completions", self.provider.base());
        let mut body = json!({
            "model": model,
            "temperature": 0,
            "max_tokens": max_tokens,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });
        // Cleanup is a rewrite, not a puzzle: thinking time is pure latency. Ask reasoning models
        // for as little as they allow and keep the reasoning out of the reply.
        match self.provider {
            Provider::Groq if model.starts_with("openai/gpt-oss") => {
                body["reasoning_effort"] = json!("low");
                body["include_reasoning"] = json!(false);
            }
            Provider::Groq if model.starts_with("qwen/") => {
                body["reasoning_effort"] = json!("none");
            }
            Provider::OpenRouter if model.starts_with("qwen/") => {
                body["reasoning"] = json!({ "effort": "none" });
            }
            Provider::OpenRouter => {
                body["reasoning"] = json!({ "effort": "low", "exclude": true });
            }
            Provider::Groq => {}
        }
        let response = self
            .agent
            .post(&url)
            .timeout(CHAT_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.key))
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        let value = self.read(response, "cleanup")?;
        let text = value
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{} returned no cleanup text", self.provider.name()))?;
        Ok(text.to_string())
    }

    /// Turns a response into JSON, or into an error a user can read. Never includes the key.
    fn read(&self, response: Result<ureq::Response, ureq::Error>, what: &str) -> Result<Value> {
        let name = self.provider.name();
        match response {
            Ok(response) => {
                let text = response
                    .into_string()
                    .with_context(|| format!("reading the {name} {what} response"))?;
                serde_json::from_str(&text)
                    .with_context(|| format!("{name} sent a {what} response that is not JSON"))
            }
            Err(ureq::Error::Status(code, response)) => {
                let body = response.into_string().unwrap_or_default();
                let detail = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.pointer("/error/message")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or(body);
                let detail: String = detail.chars().take(ERROR_BODY_LIMIT).collect();
                let hint = match code {
                    401 | 403 => " (check the API key)",
                    413 => " (the recording is too large)",
                    429 => " (rate limit or quota reached)",
                    _ => "",
                };
                bail!("{name} {what} failed with HTTP {code}{hint}: {detail}")
            }
            Err(ureq::Error::Transport(error)) => {
                bail!("could not reach {name} for {what}: {error}")
            }
        }
    }
}

/// A `multipart/form-data` body, built by hand: two text fields and a file are all it ever holds.
struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    fn new() -> Self {
        let mut random = [0u8; 12];
        // The boundary must not occur inside the audio. 96 random bits make that practically
        // impossible; if the OS RNG fails we keep zeros, which is still a boundary PCM noise is
        // vanishingly unlikely to contain.
        let _ = getrandom::getrandom(&mut random);
        let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
        Multipart {
            boundary: format!("----talkee{hex}"),
            body: Vec::new(),
        }
    }

    fn text(&mut self, name: &str, value: &str) {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n",
                self.boundary
            )
            .as_bytes(),
        );
    }

    fn file(&mut self, name: &str, filename: &str, content_type: &str, data: &[u8]) {
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(data);
        self.body.extend_from_slice(b"\r\n");
    }

    fn finish(mut self) -> (String, Vec<u8>) {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_is_well_formed() {
        let mut form = Multipart::new();
        form.text("model", "m");
        form.file("file", "a.wav", "audio/wav", b"RIFF");
        let boundary = form.boundary.clone();
        let (content_type, body) = form.finish();
        assert_eq!(
            content_type,
            format!("multipart/form-data; boundary={boundary}")
        );
        let body = String::from_utf8(body).unwrap();
        assert!(body.starts_with(&format!("--{boundary}\r\n")));
        assert!(body.contains("name=\"model\"\r\n\r\nm\r\n"));
        assert!(body.contains("filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n"));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn debug_hides_the_key() {
        let client = Client::new(Provider::Groq, "gsk_secret");
        assert!(!format!("{client:?}").contains("secret"));
    }

    #[test]
    fn parses_provider_names() {
        assert_eq!(Provider::parse(" Groq "), Some(Provider::Groq));
        assert_eq!(Provider::parse("openrouter"), Some(Provider::OpenRouter));
        assert_eq!(Provider::parse("grok"), None);
    }
}
