//! Dictation: recorded speech in, clean text out.
//!
//! ```text
//! samples (any rate) ─► 16 kHz mono WAV ─► speech-to-text API ─► raw transcript
//!                                                                    │
//!                              cleanup model (optional) ◄────────────┘
//!                                    │
//!                                    ▼
//!                                  text
//! ```
//!
//! Speech-to-text runs on a cloud API (Groq or OpenRouter). The cleanup step is a separate call to
//! a text model that turns spoken rambling into written text: punctuation, paragraphs, lists,
//! filler words gone, spoken corrections applied. Only text goes to the cleanup model, never audio.
//!
//! Everything here is synchronous and blocking: a dictation is one request, then maybe a second,
//! and the caller runs it on a worker thread. That keeps an async runtime out of the app.

pub mod audio;
pub mod cleanup;
pub mod config;
pub mod keys;
pub mod ogg_opus;
pub mod pipeline;
pub mod provider;

pub use config::Settings;
pub use keys::Keys;
pub use pipeline::{Dictation, Outcome};
pub use provider::{AudioFormat, Client, Provider};
