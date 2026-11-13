//! Dictation settings and the vocabulary, as files in Talkee's data directory.
//!
//! * `dictation.json`: provider, models, cleanup, language, extra instructions.
//! * `device.id`: this install's random id, which the relay counts daily minutes against.
//! * `vocabulary.txt`: one name or term per line; `#` starts a comment. Plain text on purpose, so
//!   it can be edited in Notepad as well as in the app.
//!
//! Like the switches in `settings.json`, both load forgivingly: a missing or damaged file gives
//! the defaults, and unknown fields are ignored, so a bad file can never stop dictation starting.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::keys::Keys;
use crate::pipeline::{Backend, Dictation};
use crate::provider::{AudioFormat, Client, Provider};
use crate::relay::{self, Relay};

/// The settings file.
pub const SETTINGS_FILE: &str = "dictation.json";
/// The vocabulary file.
pub const VOCABULARY_FILE: &str = "vocabulary.txt";
/// The install's id.
pub const DEVICE_FILE: &str = "device.id";

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Dictation answers the hotkey and the phone's button.
    pub enabled: bool,
    /// `relay` (the default: the Talkee relay holds the keys), or `groq` / `openrouter` to call
    /// them directly with this machine's own keys, for development.
    pub provider: String,
    /// The relay's address; empty for [`relay::DEFAULT_URL`].
    pub relay_url: String,
    /// Speech-to-text model; empty for the provider's default.
    pub transcribe_model: String,
    /// Rewrite the transcript as finished text.
    pub cleanup: bool,
    /// Cleanup provider; empty for the same as `provider`.
    pub cleanup_provider: String,
    /// Cleanup model; empty for the provider's default.
    pub cleanup_model: String,
    /// Extra cleanup instructions.
    pub instructions: String,
    /// ISO-639-1 language; empty to detect.
    pub language: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: true,
            provider: "relay".into(),
            relay_url: String::new(),
            transcribe_model: String::new(),
            cleanup: true,
            cleanup_provider: String::new(),
            cleanup_model: String::new(),
            instructions: String::new(),
            language: String::new(),
        }
    }
}

impl Settings {
    /// Reads settings from `dir`, or the defaults.
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(SETTINGS_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes settings to `dir`, through a temporary file so a crash leaves the old file.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(SETTINGS_FILE);
        let temporary = path.with_extension("json.tmp");
        let mut text = serde_json::to_string_pretty(self).unwrap_or_default();
        text.push('\n');
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, &path)
    }

    /// Builds the pipeline these settings describe for the install `device`. Fails, with a message a
    /// user can act on, only on the direct path when a needed key is missing.
    pub fn dictation(
        &self,
        keys: &Keys,
        vocabulary: Vec<String>,
        device: &str,
    ) -> Result<Dictation, String> {
        if Provider::parse(&self.provider).is_none() {
            let url = non_empty(&self.relay_url).unwrap_or_else(|| relay::DEFAULT_URL.to_string());
            return Ok(Dictation {
                backend: Backend::Relay {
                    relay: Relay::new(url, device),
                    cleanup: self.cleanup,
                },
                cleanup_instructions: non_empty(&self.instructions),
                language: non_empty(&self.language),
                vocabulary,
                format: AudioFormat::OggOpus,
            });
        }
        let client = |name: &str| -> Result<Client, String> {
            let provider = Provider::parse(name).unwrap_or(Provider::Groq);
            let key = match provider {
                Provider::Groq => keys.groq.clone(),
                Provider::OpenRouter => keys.openrouter.clone(),
            };
            key.map(|k| Client::new(provider, k)).ok_or_else(|| {
                format!(
                    "no {} key for direct dictation (set it in a .env file)",
                    provider.name()
                )
            })
        };
        let transcriber = client(&self.provider)?;
        let cleanup = if self.cleanup {
            let name = if self.cleanup_provider.trim().is_empty() {
                &self.provider
            } else {
                &self.cleanup_provider
            };
            let client = client(name)?;
            let model = non_empty(&self.cleanup_model)
                .unwrap_or_else(|| client.provider().default_cleanup_model().to_string());
            Some((client, model))
        } else {
            None
        };
        Ok(Dictation {
            backend: Backend::Direct {
                transcribe_model: non_empty(&self.transcribe_model).unwrap_or_else(|| {
                    transcriber
                        .provider()
                        .default_transcribe_model()
                        .to_string()
                }),
                transcriber,
                cleanup,
            },
            cleanup_instructions: non_empty(&self.instructions),
            language: non_empty(&self.language),
            vocabulary,
            format: AudioFormat::OggOpus,
        })
    }
}

fn non_empty(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// This install's id, made on first use: 32 random hex characters in `device.id`.
///
/// Not a secret and not tied to the person or the machine: it only lets the relay count one
/// install's minutes. Deleting the file makes a new one.
pub fn device_id(dir: &Path) -> String {
    let path = dir.join(DEVICE_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let id = text.trim();
        if id.len() >= 16 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return id.to_string();
        }
    }
    let mut bytes = [0u8; 16];
    let _ = getrandom::getrandom(&mut bytes);
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&path, &id);
    id
}

/// The vocabulary file's path in `dir`.
pub fn vocabulary_path(dir: &Path) -> PathBuf {
    dir.join(VOCABULARY_FILE)
}

/// Reads the vocabulary: one term per line, blank lines and `#` comments skipped, duplicates
/// dropped. Capped at 100 terms, which is about what fits Whisper's 224-token prompt.
pub fn load_vocabulary(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(vocabulary_path(dir))
        .map(|text| parse_vocabulary(&text))
        .unwrap_or_default()
}

/// Parses vocabulary text.
pub fn parse_vocabulary(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for line in text.lines() {
        let term = line.split('#').next().unwrap_or("").trim();
        if !term.is_empty() && !terms.iter().any(|t| t.eq_ignore_ascii_case(term)) {
            terms.push(term.to_string());
        }
    }
    terms.truncate(100);
    terms
}

/// Writes the vocabulary, one term per line.
pub fn save_vocabulary(dir: &Path, terms: &[String]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut text = String::from("# Names and terms Talkee should spell right, one per line.\n");
    for term in terms {
        text.push_str(term.trim());
        text.push('\n');
    }
    std::fs::write(vocabulary_path(dir), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("talkee-dictate-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn settings_round_trip_and_tolerate_damage() {
        let dir = temp("settings");
        assert_eq!(Settings::load(&dir), Settings::default());
        let settings = Settings {
            cleanup: false,
            language: "en".into(),
            ..Settings::default()
        };
        settings.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), settings);
        std::fs::write(dir.join(SETTINGS_FILE), "{ not json").unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        std::fs::write(
            dir.join(SETTINGS_FILE),
            r#"{"cleanup": false, "future": 1}"#,
        )
        .unwrap();
        assert!(!Settings::load(&dir).cleanup);
        assert!(
            Settings::load(&dir).enabled,
            "missing fields keep their defaults"
        );
    }

    #[test]
    fn vocabulary_skips_comments_blanks_and_duplicates() {
        let terms = parse_vocabulary("# header\nCompaq\n\n  Talkee  # the app\ncompaq\nGroq\n");
        assert_eq!(terms, vec!["Compaq", "Talkee", "Groq"]);
    }

    #[test]
    fn vocabulary_round_trips() {
        let dir = temp("vocab");
        save_vocabulary(&dir, &["Compaq".into(), "Talkee".into()]).unwrap();
        assert_eq!(load_vocabulary(&dir), vec!["Compaq", "Talkee"]);
    }

    #[test]
    fn the_default_needs_no_key_and_direct_names_the_missing_one() {
        let relay = Settings::default().dictation(&Keys::default(), vec![], "0123456789abcdef");
        assert!(matches!(relay.unwrap().backend, Backend::Relay { .. }));
        let direct = Settings {
            provider: "groq".into(),
            ..Settings::default()
        };
        let error = direct.dictation(&Keys::default(), vec![], "x").unwrap_err();
        assert!(error.contains("Groq"), "{error}");
    }

    #[test]
    fn the_device_id_is_made_once_and_kept() {
        let dir = temp("device");
        let first = device_id(&dir);
        assert_eq!(first.len(), 32);
        assert_eq!(device_id(&dir), first);
    }
}
