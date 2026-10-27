//! The switches, remembered between runs.
//!
//! Noise suppression, echo cancellation, auto level and mute used to start from their defaults every
//! time the app did, so a person who turned suppression off for a music session found it back on after
//! the next reboot. They are written to `%APPDATA%\Talkee\settings.json` whenever one changes and read
//! back at startup.
//!
//! # Forgiving on purpose
//!
//! A missing file, an unreadable one, one with a field missing and one with a field this build has
//! never heard of all load without complaint — the missing or unknown parts fall back to the defaults.
//! A settings file that could stop the app from starting would be a worse bug than any it could fix,
//! and a newer version of Talkee may have written fields an older one does not know.

use std::path::PathBuf;

use serde_json::{json, Value};

/// What is remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Saved {
    /// WebRTC noise suppression.
    pub noise: bool,
    /// Echo cancellation.
    pub echo: bool,
    /// Automatic level.
    pub auto_level: bool,
    /// Muted. Remembered too, because a person who muted the microphone and restarted expects it to
    /// still be muted — the alternative is a live microphone they believed was off.
    pub paused: bool,
    /// The setup guide has been dismissed.
    pub guide_done: bool,
}

impl Default for Saved {
    fn default() -> Self {
        Self {
            noise: true,
            echo: true,
            auto_level: true,
            paused: false,
            guide_done: false,
        }
    }
}

/// Where the file lives.
pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Talkee").join("settings.json")
}

impl Saved {
    /// Reads settings from JSON text. Anything missing or unreadable is the default.
    pub fn parse(text: &str) -> Self {
        let defaults = Self::default();
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return defaults;
        };
        let flag = |key: &str, default: bool| value.get(key).and_then(Value::as_bool).unwrap_or(default);
        Self {
            noise: flag("noise_suppression", defaults.noise),
            echo: flag("echo_cancel", defaults.echo),
            auto_level: flag("auto_level", defaults.auto_level),
            paused: flag("paused", defaults.paused),
            guide_done: flag("guide_done", defaults.guide_done),
        }
    }

    /// The JSON text that [`Saved::parse`] reads back.
    pub fn to_json(self) -> String {
        let mut text = serde_json::to_string_pretty(&json!({
            "noise_suppression": self.noise,
            "echo_cancel": self.echo,
            "auto_level": self.auto_level,
            "paused": self.paused,
            "guide_done": self.guide_done,
        }))
        .unwrap_or_default();
        text.push('\n');
        text
    }
}

/// Loads the saved settings, or the defaults when there are none.
pub fn load() -> Saved {
    std::fs::read_to_string(path())
        .map(|text| Saved::parse(&text))
        .unwrap_or_default()
}

/// Writes the settings. Written to a temporary file and renamed, so a crash or a power cut halfway
/// through leaves the old file rather than half of a new one.
pub fn save(saved: Saved) {
    let path = path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let temporary = path.with_extension("json.tmp");
    if std::fs::write(&temporary, saved.to_json()).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_is_what_is_read() {
        let saved = Saved {
            noise: false,
            echo: true,
            auto_level: false,
            paused: true,
            guide_done: true,
        };
        assert_eq!(Saved::parse(&saved.to_json()), saved);
    }

    #[test]
    fn nonsense_loads_as_the_defaults() {
        for text in ["", "not json", "[]", "null", "{"] {
            assert_eq!(Saved::parse(text), Saved::default(), "{text:?}");
        }
    }

    #[test]
    fn a_missing_field_takes_its_default_and_the_rest_are_kept() {
        let saved = Saved::parse(r#"{"noise_suppression": false}"#);
        assert!(!saved.noise);
        assert_eq!(saved.echo, Saved::default().echo);
        assert_eq!(saved.auto_level, Saved::default().auto_level);
    }

    #[test]
    fn a_field_from_a_newer_version_is_ignored() {
        let saved = Saved::parse(r#"{"echo_cancel": false, "something_new": 42}"#);
        assert!(!saved.echo);
    }

    #[test]
    fn a_field_of_the_wrong_type_is_ignored_rather_than_fatal() {
        let saved = Saved::parse(r#"{"noise_suppression": "yes"}"#);
        assert_eq!(saved.noise, Saved::default().noise);
    }
}
