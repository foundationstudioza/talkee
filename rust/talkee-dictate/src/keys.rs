//! API keys.
//!
//! For now keys come from the process environment or a `.env` file (`KEY=value` lines). The desktop
//! app will keep its own copy encrypted with DPAPI, like the pairing key; this is the development
//! path and the one the Linux server uses.
//!
//! A key is never printed, logged, or put in an error message. [`Keys`]'s `Debug` shows only which
//! keys are present.

use std::path::{Path, PathBuf};

/// Environment variable for the Groq key.
pub const GROQ: &str = "GROQ_API_KEY";
/// Environment variable for the OpenRouter key.
pub const OPENROUTER: &str = "OPENROUTER_API_KEY";

/// The API keys this machine has.
#[derive(Clone, Default)]
pub struct Keys {
    /// Groq key, if set.
    pub groq: Option<String>,
    /// OpenRouter key, if set.
    pub openrouter: Option<String>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys")
            .field("groq", &self.groq.is_some())
            .field("openrouter", &self.openrouter.is_some())
            .finish()
    }
}

impl Keys {
    /// Loads keys from the environment, then fills gaps from the first `.env` found in `start` or
    /// one of its parents. A variable already set in the environment wins over the file.
    pub fn load(start: &Path) -> (Self, Option<PathBuf>) {
        let mut keys = Keys {
            groq: non_empty(std::env::var(GROQ).ok()),
            openrouter: non_empty(std::env::var(OPENROUTER).ok()),
        };
        let file = start
            .ancestors()
            .map(|dir| dir.join(".env"))
            .find(|path| path.is_file());
        if let Some(path) = &file {
            if let Ok(text) = std::fs::read_to_string(path) {
                keys.fill_from(&text);
            }
        }
        (keys, file)
    }

    /// Fills keys this set lacks from `other`.
    pub fn fill_gaps(&mut self, other: &Keys) {
        if self.groq.is_none() {
            self.groq = other.groq.clone();
        }
        if self.openrouter.is_none() {
            self.openrouter = other.openrouter.clone();
        }
    }

    /// Fills unset keys from `.env` text.
    pub fn fill_from(&mut self, text: &str) {
        for (name, value) in parse_env(text) {
            let slot = match name.as_str() {
                GROQ => &mut self.groq,
                OPENROUTER => &mut self.openrouter,
                _ => continue,
            };
            if slot.is_none() {
                *slot = non_empty(Some(value));
            }
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Parses `.env` lines: `NAME=value`, optional `export `, optional matching quotes, `#` comments.
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (name, value) = line.split_once('=')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            Some((name.trim().to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_lines() {
        let text = "# c\nGROQ_API_KEY=gsk_1\nexport OPENROUTER_API_KEY=\"sk-or-2\"\nEMPTY=\n";
        let mut keys = Keys::default();
        keys.fill_from(text);
        assert_eq!(keys.groq.as_deref(), Some("gsk_1"));
        assert_eq!(keys.openrouter.as_deref(), Some("sk-or-2"));
    }

    #[test]
    fn empty_values_count_as_missing() {
        let mut keys = Keys::default();
        keys.fill_from("GROQ_API_KEY=\nOPENROUTER_API_KEY=   \n");
        assert!(keys.groq.is_none() && keys.openrouter.is_none());
    }

    #[test]
    fn debug_never_shows_a_key() {
        let keys = Keys {
            groq: Some("gsk_secret".into()),
            openrouter: None,
        };
        let shown = format!("{keys:?}");
        assert!(!shown.contains("secret"));
        assert!(shown.contains("groq: true"));
    }
}
