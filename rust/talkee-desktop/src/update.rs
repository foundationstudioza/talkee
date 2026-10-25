//! Keeping the app current: look for a newer release, fetch it quietly, and ask before installing.
//!
//! # How it behaves
//!
//! A background thread reads `update.json` from the latest GitHub release a little after startup and
//! every few hours after that. When a newer version exists it downloads the installer to a temporary
//! folder, checks the SHA-256 the manifest promises, and only then tells the user — in the tray menu
//! and in the window — that an update is ready. Nothing is installed without a click: the app may be
//! in the middle of a call.
//!
//! # What is and is not protected
//!
//! The manifest and the installer come from the same repository over HTTPS, and the installer is
//! checked against the manifest's hash, so a truncated or substituted download is refused. These are
//! beta builds and they are not code-signed, so an attacker who controls the repository controls the
//! update; when the app is signed that changes, and the check lives here.
//!
//! # Why it never touches the network on the audio path
//!
//! It is a single low-priority thread that sleeps almost all the time, and a failure of any kind —
//! offline, rate-limited, a malformed manifest — is logged once and retried later, never surfaced as
//! something the user has to deal with.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

/// Where the latest release publishes its manifest. `latest` is GitHub's own redirect to the newest
/// non-draft release, so this address never has to change.
pub const MANIFEST_URL: &str =
    "https://github.com/foundationstudioza/talkee/releases/latest/download/update.json";

/// The version this binary was built as.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// How long after startup the first check waits. The first seconds belong to the audio device.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(25);

/// Time between checks.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// A refusal to download anything larger than this, whatever the manifest claims.
const MAX_INSTALLER_BYTES: u64 = 200 * 1024 * 1024;

/// One downloadable file named in the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// Where to fetch it.
    pub url: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
}

/// The release manifest: the newest version and where its files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Semantic version, `major.minor.patch`.
    pub version: String,
    /// The Windows installer.
    pub windows: Option<Asset>,
}

impl Manifest {
    /// Reads the manifest. Anything unexpected is an error rather than a guess.
    pub fn parse(json: &str) -> Result<Self> {
        let v: serde_json::Value = serde_json::from_str(json).context("update.json is not JSON")?;
        let version = v["version"]
            .as_str()
            .ok_or_else(|| anyhow!("update.json has no version"))?
            .trim_start_matches('v')
            .to_string();
        parse_version(&version).ok_or_else(|| anyhow!("{version:?} is not major.minor.patch"))?;
        let asset = |key: &str| -> Option<Asset> {
            let a = &v[key];
            Some(Asset {
                url: a["url"].as_str()?.to_string(),
                sha256: a["sha256"].as_str()?.to_ascii_lowercase(),
            })
        };
        Ok(Self {
            version,
            windows: asset("windows"),
        })
    }
}

/// `1.2.3` as numbers, or `None` if it is not that.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    // The pre-release or build suffix is dropped first: `1.2.3-beta.1` has a dot in it that is not a
    // fourth version component.
    let core = text.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// True when `candidate` is strictly newer than `current`. An unreadable version is never newer: a
/// garbled manifest must not be able to push an installer at someone.
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (parse_version(current), parse_version(candidate)) {
        (Some(c), Some(n)) => n > c,
        _ => false,
    }
}

/// Where the update is up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Nothing newer, or not checked yet.
    Current,
    /// A newer version is being fetched.
    Downloading(String),
    /// Downloaded and verified: waiting for the user to say go.
    Ready {
        /// The new version.
        version: String,
        /// The installer on disk.
        path: PathBuf,
    },
}

/// A handle on the background updater. Cheap to clone.
#[derive(Clone)]
pub struct Updater {
    state: Arc<Mutex<State>>,
    /// Set when an installer has been started, so the app can quit and get out of its way.
    quit: Arc<AtomicBool>,
}

impl Updater {
    /// Starts the background thread. `enabled == false` makes it a handle that never does anything.
    pub fn start(enabled: bool) -> Self {
        let updater = Self {
            state: Arc::new(Mutex::new(State::Current)),
            quit: Arc::new(AtomicBool::new(false)),
        };
        if enabled {
            let worker = updater.clone();
            let _ = thread::Builder::new().name("talkee-update".into()).spawn(move || {
                thread::sleep(FIRST_CHECK_DELAY);
                loop {
                    if let Err(e) = worker.check_once() {
                        println!("update   : check failed ({e:#}); will try again later");
                    }
                    thread::sleep(CHECK_EVERY);
                }
            });
        }
        updater
    }

    /// Where the update is up to.
    pub fn state(&self) -> State {
        self.state.lock().map(|s| s.clone()).unwrap_or(State::Current)
    }

    /// The version that is ready to install, if one is.
    pub fn ready_version(&self) -> Option<String> {
        match self.state() {
            State::Ready { version, .. } => Some(version),
            _ => None,
        }
    }

    /// True once an installer has been launched and the app should exit.
    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }

    fn set(&self, state: State) {
        if let Ok(mut held) = self.state.lock() {
            *held = state;
        }
    }

    /// One look at the manifest, and the download if there is something new.
    fn check_once(&self) -> Result<()> {
        if matches!(self.state(), State::Ready { .. }) {
            return Ok(());
        }
        let body = ureq::get(MANIFEST_URL)
            .timeout(Duration::from_secs(20))
            .call()
            .context("reading update.json")?
            .into_string()
            .context("update.json was not text")?;
        let manifest = Manifest::parse(&body)?;
        if !is_newer(CURRENT, &manifest.version) {
            self.set(State::Current);
            return Ok(());
        }
        let Some(asset) = manifest.windows.clone() else {
            return Ok(());
        };
        println!("update   : {} is available, downloading", manifest.version);
        self.set(State::Downloading(manifest.version.clone()));
        match download(&asset, &manifest.version) {
            Ok(path) => {
                println!("update   : {} is ready to install", manifest.version);
                self.set(State::Ready {
                    version: manifest.version,
                    path,
                });
                Ok(())
            }
            Err(e) => {
                self.set(State::Current);
                Err(e)
            }
        }
    }

    /// Runs the downloaded installer and asks the app to quit.
    ///
    /// The installer is told to close and restart the app itself, so quitting here is only the polite
    /// half: it keeps the app from being killed mid-write.
    pub fn install(&self) -> Result<()> {
        let State::Ready { path, .. } = self.state() else {
            bail!("no update is ready");
        };
        Command::new(&path)
            .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS", "/RESTARTAPPLICATIONS"])
            .spawn()
            .with_context(|| format!("starting {}", path.display()))?;
        self.quit.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Fetches an installer, checks it, and returns where it is.
fn download(asset: &Asset, version: &str) -> Result<PathBuf> {
    if !asset.url.starts_with("https://") {
        bail!("the installer address is not https");
    }
    let dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Talkee")
        .join("update");
    fs::create_dir_all(&dir).context("creating the update folder")?;
    let path = dir.join(format!("Talkee-Setup-{version}.exe"));
    let partial = path.with_extension("part");

    let response = ureq::get(&asset.url)
        .timeout(Duration::from_secs(600))
        .call()
        .context("downloading the installer")?;
    let mut reader = response.into_reader().take(MAX_INSTALLER_BYTES + 1);
    let mut file = fs::File::create(&partial).context("creating the download")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer).context("reading the download")?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_INSTALLER_BYTES {
            let _ = fs::remove_file(&partial);
            bail!("the installer is larger than {MAX_INSTALLER_BYTES} bytes");
        }
        hasher.update(&buffer[..read]);
        std::io::Write::write_all(&mut file, &buffer[..read]).context("writing the download")?;
    }
    drop(file);

    let got = hex(&hasher.finalize());
    if got != asset.sha256 {
        let _ = fs::remove_file(&partial);
        bail!("the installer's SHA-256 is {got}, not the {} the release promises", asset.sha256);
    }
    fs::rename(&partial, &path).context("keeping the verified installer")?;
    Ok(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers_not_text() {
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.9.0", "0.10.0"), "0.10 is newer than 0.9, whatever the text says");
        assert!(is_newer("1.2.3", "2.0.0"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("1.2.3", "1.2.2"));
    }

    #[test]
    fn an_unreadable_version_is_never_an_update() {
        assert!(!is_newer("0.1.0", "banana"));
        assert!(!is_newer("0.1.0", "1.2"));
        assert!(!is_newer("0.1.0", "1.2.3.4"));
        assert!(!is_newer("nonsense", "9.9.9"));
    }

    #[test]
    fn a_prerelease_suffix_does_not_break_the_comparison() {
        assert_eq!(parse_version("1.2.3-beta.1"), Some((1, 2, 3)));
        assert!(is_newer("0.1.0", "0.2.0-beta"));
    }

    #[test]
    fn the_manifest_is_read_and_the_leading_v_is_dropped() {
        let m = Manifest::parse(
            r#"{"version":"v0.2.0","windows":{"url":"https://example.invalid/a.exe","sha256":"ABCD"},
                "android":{"url":"https://example.invalid/a.apk","sha256":"ef","versionCode":200}}"#,
        )
        .unwrap();
        assert_eq!(m.version, "0.2.0");
        let w = m.windows.unwrap();
        assert_eq!(w.sha256, "abcd", "hashes compare lowercase");
        assert_eq!(w.url, "https://example.invalid/a.exe");
    }

    #[test]
    fn a_manifest_with_no_version_or_a_bad_one_is_refused() {
        assert!(Manifest::parse("{}").is_err());
        assert!(Manifest::parse(r#"{"version":"soon"}"#).is_err());
        assert!(Manifest::parse("not json").is_err());
    }

    #[test]
    fn a_manifest_without_a_windows_file_is_still_valid() {
        let m = Manifest::parse(r#"{"version":"0.2.0"}"#).unwrap();
        assert!(m.windows.is_none());
    }

    #[test]
    fn installing_with_nothing_downloaded_is_an_error_not_a_crash() {
        let updater = Updater::start(false);
        assert!(updater.install().is_err());
        assert!(updater.ready_version().is_none());
        assert!(!updater.quit_requested());
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xab]), "000fab");
    }
}
