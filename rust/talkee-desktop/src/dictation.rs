//! Dictation in the desktop app: hotkey or phone → record → transcribe and clean up → type.
//!
//! One worker thread owns a dictation from start to finish. It opens the microphone itself because
//! a cpal stream must be dropped on the thread that opened it, and it blocks on the network calls,
//! which take about a second, so the app's 20 Hz loop never waits for either.
//!
//! Settings, keys, and the vocabulary are read again at the start of every dictation, so whatever
//! the settings window saved applies to the next one without a restart.
//!
//! The log records lengths and timings, never the dictated words: the log is a file people send
//! when something breaks, and what someone dictates is theirs.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use talkee_dictate::{config, Keys, Settings};

use crate::typing::{self, Delivery, Target};

/// The longest recording; a held key past this is treated as a release.
const MAX_RECORDING: Duration = Duration::from_secs(300);

/// Where the audio comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// This computer's default microphone (the hotkey).
    Computer,
    /// The paired phone's stream (its hold-to-talk button).
    Phone,
}

/// What the worker is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Start recording from a source.
    Start(Source),
    /// Stop recording and deliver the text.
    Stop,
    /// Throw the recording away.
    Cancel,
}

/// Where a dictation is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Nothing happening.
    Idle,
    /// Recording.
    Recording(Source),
    /// Transcribing and cleaning up.
    Working,
    /// Delivered; carries how.
    Done(String),
    /// Failed; carries why, in words for the user.
    Failed(String),
}

/// What the rest of the app can see.
#[derive(Debug, Clone)]
pub struct Status {
    /// Current phase.
    pub phase: Phase,
    /// When the phase last changed.
    pub since: Instant,
    /// The last delivered text, for the window's history and the phone. Never logged.
    pub last_text: String,
    /// Increments on every change, so pollers can tell.
    pub version: u64,
}

/// The phone's processed audio, collected while a phone dictation records.
///
/// The receiver hands every processed block (after echo cancellation, noise suppression, and
/// level) to [`PhoneTap::push`]; it is kept only while a collection is open.
#[derive(Clone, Default)]
pub struct PhoneTap(Arc<Mutex<Option<Collected>>>);

/// What a phone dictation has collected so far: the samples and their rate.
type Collected = (Vec<f32>, u32);

impl PhoneTap {
    /// Starts collecting.
    pub fn open(&self) {
        if let Ok(mut held) = self.0.lock() {
            *held = Some((Vec::with_capacity(48_000 * 30), 48_000));
        }
    }

    /// Stops collecting and returns what arrived, with its sample rate.
    pub fn close(&self) -> (Vec<f32>, u32) {
        self.0
            .lock()
            .ok()
            .and_then(|mut held| held.take())
            .unwrap_or((Vec::new(), 48_000))
    }

    /// The tap to give the receiver.
    pub fn receiver_tap(&self) -> Arc<dyn talkee_core::AudioTap> {
        Arc::new(self.clone())
    }
}

impl talkee_core::AudioTap for PhoneTap {
    fn collecting(&self) -> bool {
        self.0.lock().map(|held| held.is_some()).unwrap_or(false)
    }

    fn push(&self, samples: &[f32], rate: u32) {
        let Ok(mut held) = self.0.lock() else {
            return;
        };
        let Some((buffer, buffer_rate)) = held.as_mut() else {
            return;
        };
        *buffer_rate = rate;
        if buffer.len() < rate as usize * MAX_RECORDING.as_secs() as usize {
            buffer.extend_from_slice(samples);
        }
    }
}

/// The dictation worker, as the app holds it.
#[derive(Clone)]
pub struct Dictator {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
}

impl Dictator {
    /// Starts the worker. `dir` is Talkee's data directory (settings, vocabulary, keys).
    pub fn start(dir: PathBuf, store: talkee_store::Store, phone: PhoneTap) -> Self {
        let (commands, inbox) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status {
            phase: Phase::Idle,
            since: Instant::now(),
            last_text: String::new(),
            version: 0,
        }));
        let shared = Arc::clone(&status);
        let _ = std::thread::Builder::new()
            .name("talkee-dictation".into())
            .spawn(move || {
                Worker {
                    dir,
                    store,
                    phone,
                    status: shared,
                }
                .run(inbox)
            });
        Dictator { commands, status }
    }

    /// Asks the worker to do something. Never blocks.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// A copy of the current status.
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }
}

/// Reads the API keys: saved in the app (encrypted), then the environment, then a `.env` in
/// Talkee's data directory or the current directory or its parents. Earlier sources win.
pub fn load_keys(store: &talkee_store::Store) -> Keys {
    let mut keys = Keys::default();
    if let Ok(Some(text)) = store.load_keys() {
        keys.fill_from(&text);
    }
    let (from_data_dir, _) = Keys::load(store.root());
    keys.fill_gaps(&from_data_dir);
    if let Ok(cwd) = std::env::current_dir() {
        let (from_cwd, _) = Keys::load(&cwd);
        keys.fill_gaps(&from_cwd);
    }
    keys
}

/// Whether the settings can build a pipeline, and if not, why. For the window.
pub fn readiness(dir: &std::path::Path, store: &talkee_store::Store) -> Result<(), String> {
    Settings::load(dir)
        .dictation(&load_keys(store), Vec::new(), &config::device_id(dir))
        .map(|_| ())
}

/// The dictation settings as the phone is shown them, read from the files.
pub fn current_config(dir: &std::path::Path) -> talkee_core::live::DictateConfig {
    let settings = Settings::load(dir);
    talkee_core::live::DictateConfig {
        enabled: settings.enabled,
        cleanup: settings.cleanup,
        vocabulary: config::load_vocabulary(dir),
        usage: load_usage(dir),
    }
}

/// Writes a change the phone asked for into the settings and vocabulary files.
pub fn apply_configure(dir: &std::path::Path, change: &talkee_core::live::DictateConfigure) {
    if change.enabled.is_some() || change.cleanup.is_some() {
        let mut settings = Settings::load(dir);
        if let Some(enabled) = change.enabled {
            settings.enabled = enabled;
        }
        if let Some(cleanup) = change.cleanup {
            settings.cleanup = cleanup;
        }
        if let Err(e) = settings.save(dir) {
            log(&format!("could not save the phone's settings: {e}"));
        }
    }
    if let Some(vocabulary) = &change.vocabulary {
        // Through the same parser as the file, so the phone gets the same trimming and limits.
        let terms = config::parse_vocabulary(&vocabulary.join("\n"));
        if let Err(e) = config::save_vocabulary(dir, &terms) {
            log(&format!("could not save the phone's vocabulary: {e}"));
        }
    }
}

/// Today's dictation minutes, as the relay last reported them: `usage.json` in the data directory,
/// written by the worker and read by the window (a separate process).
pub fn save_usage(dir: &std::path::Path, used_seconds: u32, limit_seconds: u32) {
    let text = format!(
        "{{\"day\":\"{}\",\"used\":{used_seconds},\"limit\":{limit_seconds}}}\n",
        utc_day()
    );
    let _ = std::fs::write(dir.join("usage.json"), text);
}

/// Today's (used, limit) seconds, if a dictation has reported them today.
pub fn load_usage(dir: &std::path::Path) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(dir.join("usage.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let limit = value.get("limit")?.as_u64()? as u32;
    if value.get("day")?.as_str()? != utc_day() {
        return Some((0, limit)); // the relay's day has turned over
    }
    Some((value.get("used")?.as_u64()? as u32, limit))
}

/// The UTC date, which is when the relay's daily allowance resets.
fn utc_day() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0) as i64;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

struct Recording {
    source: Source,
    target: Target,
    started: Instant,
    mic: Option<crate::mic::Capture>,
}

struct Worker {
    dir: PathBuf,
    store: talkee_store::Store,
    phone: PhoneTap,
    status: Arc<Mutex<Status>>,
}

impl Worker {
    fn set(&self, phase: Phase, text: Option<String>) {
        if let Ok(mut status) = self.status.lock() {
            status.phase = phase;
            status.since = Instant::now();
            if let Some(text) = text {
                status.last_text = text;
            }
            status.version += 1;
        }
    }

    fn run(self, inbox: Receiver<Command>) {
        let mut recording: Option<Recording> = None;
        loop {
            let command = match inbox.recv_timeout(Duration::from_millis(250)) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            // A key held past the limit ends the recording as if released.
            let overdue = recording
                .as_ref()
                .is_some_and(|r| r.started.elapsed() >= MAX_RECORDING);
            match (command, overdue) {
                (Some(Command::Start(source)), _) => {
                    if recording.is_some() {
                        continue;
                    }
                    if !Settings::load(&self.dir).enabled {
                        continue;
                    }
                    let target = typing::foreground();
                    let mic = match source {
                        Source::Computer => match crate::mic::Capture::open() {
                            Ok(mic) => Some(mic),
                            Err(error) => {
                                log(&format!("could not open the microphone: {error:#}"));
                                self.set(
                                    Phase::Failed(format!(
                                        "Could not open the microphone: {error:#}"
                                    )),
                                    None,
                                );
                                continue;
                            }
                        },
                        Source::Phone => {
                            self.phone.open();
                            None
                        }
                    };
                    log(&format!("recording from the {}", source_name(source)));
                    self.set(Phase::Recording(source), None);
                    recording = Some(Recording {
                        source,
                        target,
                        started: Instant::now(),
                        mic,
                    });
                }
                (Some(Command::Cancel), _) => {
                    if let Some(r) = recording.take() {
                        if r.source == Source::Phone {
                            self.phone.close();
                        }
                        log("cancelled");
                        self.set(Phase::Idle, None);
                    }
                }
                (Some(Command::Stop), _) | (None, true) => {
                    if let Some(r) = recording.take() {
                        self.finish(r);
                    }
                }
                (None, false) => {}
            }
        }
    }

    fn finish(&self, recording: Recording) {
        let (samples, rate) = match recording.mic {
            Some(mic) => mic.finish(),
            None => self.phone.close(),
        };
        let seconds = samples.len() as f32 / rate.max(1) as f32;
        self.set(Phase::Working, None);

        let settings = Settings::load(&self.dir);
        let vocabulary = config::load_vocabulary(&self.dir);
        let device = config::device_id(&self.dir);
        let dictation = match settings.dictation(&load_keys(&self.store), vocabulary, &device) {
            Ok(dictation) => dictation,
            Err(message) => {
                log(&format!("not set up: {message}"));
                self.set(Phase::Failed(message), None);
                return;
            }
        };
        match dictation.run(&samples, rate) {
            Ok(outcome) => {
                if let Some((used, limit)) = outcome.usage {
                    save_usage(&self.dir, used, limit);
                }
                let delivery = if recording.source == Source::Phone && outcome.text.is_empty() {
                    Delivery::Typed
                } else {
                    typing::deliver(&outcome.text, recording.target)
                };
                log(&format!(
                    "{:.1} s recorded, {:.1} s sent ({} KB), transcribed in {} ms, cleaned up in {} ms, {} chars {}{}",
                    seconds,
                    outcome.audio_seconds,
                    outcome.upload_bytes / 1024,
                    outcome.transcribe_time.as_millis(),
                    outcome.cleanup_time.as_millis(),
                    outcome.text.chars().count(),
                    delivery.describe(),
                    outcome
                        .cleanup_note
                        .as_deref()
                        .map(|n| format!(" (cleanup skipped: {n})"))
                        .unwrap_or_default(),
                ));
                self.set(
                    Phase::Done(delivery.describe().to_string()),
                    Some(outcome.text),
                );
            }
            Err(error) => {
                let message = format!("{error:#}");
                log(&format!("failed after {seconds:.1} s of audio: {message}"));
                self.set(Phase::Failed(capitalise(&message)), None);
            }
        }
    }
}

/// `talkee-app --dictate-file <wav>`: one dictation from a recording instead of the microphone, typed
/// into whatever window has focus when it starts, with the indicator shown. For testing everything
/// after the hotkey on a real desktop; it uses the saved settings, keys, and vocabulary.
pub fn dictate_file(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let store = talkee_store::Store::open()?;
    let dir = store.root().to_path_buf();
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let decoded = talkee_dictate::audio::read_wav(&bytes)?;
    let target = typing::foreground();
    let dictation = Settings::load(&dir)
        .dictation(&load_keys(&store), config::load_vocabulary(&dir), &config::device_id(&dir))
        .map_err(anyhow::Error::msg)?;

    let mut overlay = crate::overlay::Overlay::new();
    overlay.set(Some(crate::overlay::Shown {
        text: "Transcribing\u{2026}".into(),
        accent: 0xF5A524,
    }));
    let worker = std::thread::spawn(move || dictation.run(&decoded.samples, decoded.rate));
    while !worker.is_finished() {
        crate::message_pump::pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    overlay.set(None);
    let outcome = worker
        .join()
        .map_err(|_| anyhow::anyhow!("the dictation thread panicked"))??;
    let delivery = typing::deliver(&outcome.text, target);
    println!(
        "{} chars, {:.1} s audio ({} KB), transcribed in {} ms, cleaned up in {} ms: {}{}",
        outcome.text.chars().count(),
        outcome.audio_seconds,
        outcome.upload_bytes / 1024,
        outcome.transcribe_time.as_millis(),
        outcome.cleanup_time.as_millis(),
        delivery.describe(),
        outcome
            .cleanup_note
            .map(|n| format!(" ({n})"))
            .unwrap_or_default(),
    );
    Ok(())
}

fn source_name(source: Source) -> &'static str {
    match source {
        Source::Computer => "computer's microphone",
        Source::Phone => "phone",
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn log(line: &str) {
    println!("dictation: {line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phone_tap_only_collects_while_open() {
        use talkee_core::AudioTap;
        let tap = PhoneTap::default();
        assert!(!tap.collecting());
        tap.push(&[0.1; 10], 48_000);
        tap.open();
        assert!(tap.collecting());
        tap.push(&[0.2; 10], 16_000);
        tap.push(&[0.3; 5], 16_000);
        let (samples, rate) = tap.close();
        assert_eq!((samples.len(), rate), (15, 16_000));
        assert!(!tap.collecting());
        tap.push(&[0.4; 10], 48_000);
        assert!(tap.close().0.is_empty());
    }
}
