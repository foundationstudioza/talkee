//! What Talkee keeps between runs.
//!
//! # Why anything is kept at all
//!
//! The pairing key used to be generated fresh for every run, and the host said as much:
//! *"the link carries this run's key: anyone who sees it can speak through this microphone until
//! the process exits"*. That is the right property for a *process* and the wrong one for a
//! *product*: it makes scanning the pairing code a daily chore, and a daily chore is one people
//! stop doing carefully. The key now belongs to the machine, so a phone pairs once.
//!
//! # What persisting a key costs
//!
//! A key that exists only in memory cannot be stolen from disk, so this trades one risk for
//! another. It is stored as a DPAPI blob (see [`dpapi`]) rather than a config file, because whoever
//! reads it can speak through this microphone, and a file in a user's profile is readable by every
//! process that user runs. `docs/PROTOCOL.md` already states the rule — *"stored in the platform
//! keystore — never in a plain config file"* — and this is it.
//!
//! # The file
//!
//! `%APPDATA%\Talkee\pairing.dat`: one DPAPI blob and nothing else. The whole payload is encrypted,
//! including the format marker, so nothing about the pairing is readable without the user's
//! credentials.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use talkee_crypto::Psk;

mod dpapi;

/// The file the pairing key lives in.
const PAIRING_FILE: &str = "pairing.dat";

/// A format marker, so a later version can change the payload without guessing.
///
/// Inside the encrypted blob rather than beside it. The marker is not a secret, but sitting outside
/// it would let anyone who sees the file tell whether this machine has ever paired.
const MAGIC: &[u8; 8] = b"TALKEEPA";

/// The pairing this machine is using, and how it came to be that way.
#[derive(Debug, Clone)]
pub struct Pairing {
    /// The key itself.
    pub psk: Psk,
    /// True when this machine had no key, so this one is new.
    pub created: bool,
    /// Set when a key existed but could not be used, with the reason.
    ///
    /// A caller has to say this out loud. A replaced key means every phone that was paired has to
    /// scan again, and that is not something a user should have to deduce from a microphone that
    /// stopped working.
    pub replaced: Option<String>,
}

/// Where this machine keeps Talkee's state.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens the store in the user's application data directory.
    ///
    /// `%APPDATA%\Talkee` rather than beside the executable: the executable may sit somewhere the
    /// user cannot write, and this is per-user state.
    pub fn open() -> Result<Self> {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).ok_or_else(|| {
            anyhow::anyhow!("no APPDATA directory, so there is nowhere to keep the pairing key")
        })?;
        Ok(Self {
            root: base.join("Talkee"),
        })
    }

    /// A store rooted somewhere else. For tests, which must not touch the real one.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory the store lives in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file the key is kept in.
    pub fn pairing_path(&self) -> PathBuf {
        self.root.join(PAIRING_FILE)
    }

    /// The pairing key, if this machine has one.
    ///
    /// `Ok(None)` means there is no key yet. An `Err` means there *is* a file and it cannot be
    /// read — a different situation, and one the caller must not paper over.
    pub fn load_pairing(&self) -> Result<Option<Psk>> {
        let path = self.pairing_path();
        let blob = match std::fs::read(&path) {
            Ok(blob) => blob,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };

        let plain =
            dpapi::unprotect(&blob).with_context(|| format!("decrypting {}", path.display()))?;
        if plain.len() != MAGIC.len() + talkee_crypto::PSK_LEN || &plain[..MAGIC.len()] != MAGIC {
            return Err(anyhow::anyhow!(
                "{} is not a Talkee pairing file",
                path.display()
            ));
        }
        Ok(Some(Psk::from_slice(&plain[MAGIC.len()..])?))
    }

    /// Writes the pairing key, replacing any previous one.
    pub fn save_pairing(&self, psk: &Psk) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;

        let mut plain = Vec::with_capacity(MAGIC.len() + talkee_crypto::PSK_LEN);
        plain.extend_from_slice(MAGIC);
        plain.extend_from_slice(psk.as_bytes());

        let blob = dpapi::protect(&plain)?;
        let path = self.pairing_path();
        std::fs::write(&path, &blob).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// The key to use, making one if this machine has none.
    pub fn pairing(&self) -> Result<Pairing> {
        match self.load_pairing() {
            Ok(Some(psk)) => Ok(Pairing {
                psk,
                created: false,
                replaced: None,
            }),
            Ok(None) => {
                let psk = Psk::generate()?;
                self.save_pairing(&psk)?;
                Ok(Pairing {
                    psk,
                    created: true,
                    replaced: None,
                })
            }
            // A file that cannot be read is replaced rather than fatal — the alternative is an app
            // that will not start because of a bad byte — but the caller is told, because the
            // pairing being replaced is one somebody is relying on.
            Err(e) => {
                let psk = Psk::generate()?;
                self.save_pairing(&psk)?;
                Ok(Pairing {
                    psk,
                    created: true,
                    replaced: Some(format!("{e:#}")),
                })
            }
        }
    }

    /// Removes the key, so the next run makes a new one and every paired phone is locked out.
    ///
    /// The reason a persisted credential needs an off switch: without one there is no way to revoke
    /// a phone that should no longer be able to speak through this microphone.
    pub fn forget_pairing(&self) -> Result<()> {
        let path = self.pairing_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A store in a directory of its own, so no test can touch the real one.
    fn temp_store(name: &str) -> Store {
        let root = std::env::temp_dir().join(format!("talkee-store-test-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        Store::at(root)
    }

    #[test]
    fn a_key_survives_the_process_that_made_it() {
        // The entire point. A second `Store` is what a restart looks like from here.
        let store = temp_store("survives");
        let first = store.pairing().expect("first run makes a key");
        assert!(first.created, "the first run should report a new key");
        assert!(first.replaced.is_none());

        let second = Store::at(store.root()).pairing().expect("second run reads it");
        assert!(!second.created, "the second run must not make another key");
        assert_eq!(
            first.psk.as_bytes(),
            second.psk.as_bytes(),
            "the key changed across a restart, which would unpair every phone"
        );
    }

    #[test]
    fn the_key_is_not_readable_in_the_file() {
        // The point of DPAPI rather than a file permission. A store that wrote a plaintext copy
        // beside the encrypted one would pass the round-trip test above and fail this one, which is
        // why both exist.
        let store = temp_store("at-rest");
        let pairing = store.pairing().expect("makes a key");
        let on_disk = std::fs::read(store.pairing_path()).expect("the file was written");

        let key = pairing.psk.as_bytes();
        assert!(
            !on_disk.windows(key.len()).any(|w| w == key.as_slice()),
            "the pairing key is sitting in the file in the clear"
        );
        assert!(
            !on_disk.windows(MAGIC.len()).any(|w| w == MAGIC.as_slice()),
            "even the format marker is meant to be inside the encrypted payload"
        );
        assert!(
            on_disk.len() > key.len(),
            "a file shorter than the key cannot be an encrypted key"
        );
    }

    #[test]
    fn a_damaged_file_is_replaced_and_reported_rather_than_crashing() {
        // A half-written file from a power cut must not leave an app that refuses to start. It is
        // replaced — but the caller is told, because the pairing being lost is one a phone relies on.
        let store = temp_store("damaged");
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(store.pairing_path(), b"not a dpapi blob at all").unwrap();

        let pairing = store.pairing().expect("a damaged file must not be fatal");
        assert!(pairing.created, "a replacement key should report as new");
        assert!(
            pairing.replaced.is_some(),
            "the caller has to be able to say the old pairing was lost"
        );

        // And the replacement is usable, so the next run is normal again.
        assert!(!Store::at(store.root()).pairing().unwrap().created);
    }

    #[test]
    fn an_empty_file_counts_as_damaged() {
        // The truncation case specifically: a zero-byte file is what an interrupted write leaves.
        let store = temp_store("empty");
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(store.pairing_path(), b"").unwrap();
        let pairing = store.pairing().expect("an empty file must not be fatal");
        assert!(pairing.created && pairing.replaced.is_some());
    }

    #[test]
    fn forgetting_removes_the_key_so_a_new_one_is_made() {
        // Revocation: the only way to stop a phone that should no longer be able to speak.
        let store = temp_store("forget");
        let before = store.pairing().unwrap();
        store.forget_pairing().expect("removing an existing key");
        assert!(store.load_pairing().unwrap().is_none(), "the key is still there");

        let after = store.pairing().unwrap();
        assert!(after.created, "a forgotten key must be replaced by a new one");
        assert_ne!(
            before.psk.as_bytes(),
            after.psk.as_bytes(),
            "forgetting must produce a different key, or it revoked nothing"
        );
    }

    #[test]
    fn forgetting_when_there_is_nothing_to_forget_is_not_an_error() {
        let store = temp_store("forget-nothing");
        store.forget_pairing().expect("no key is not a failure");
    }

    #[test]
    fn the_directory_is_created_on_first_use() {
        // `%APPDATA%\Talkee` does not exist the first time, and a save that assumed it did would
        // fail on exactly the run that matters — the first one.
        let store = temp_store("fresh-dir");
        assert!(!store.root().exists());
        store.pairing().expect("creating the key");
        assert!(store.root().is_dir());
        assert!(store.pairing_path().is_file());
    }
}
