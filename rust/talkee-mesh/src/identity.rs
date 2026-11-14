//! Who this device is.
//!
//! The id is random, made once and kept in `device.json` (not a secret: it names the device, it does
//! not let anyone in). The name defaults to the host name and can be changed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use talkee_store::Store;

const FILE: &str = "device.json";

/// What sort of device this is. Decides how it is shown and what it is expected to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Pc,
    Phone,
    Server,
}

/// Something a device can do for the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    /// It has a microphone it can stream.
    Mic,
    /// It can turn speech into text.
    Dictate,
    /// It can type text into whatever has focus.
    TypeAtCursor,
    /// It can run coding agents (Phase 3).
    Agents,
    /// It can list and read files for a peer (Phase 4).
    Files,
}

/// This device, as it describes itself to peers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// 16 random bytes in hex. Stable for the life of the install.
    pub id: String,
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub caps: Vec<Capability>,
    /// The Talkee version, filled in at run time rather than stored.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// The operating system (`linux`, `windows`, `android`), filled in at run time.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
}

impl Device {
    /// This device's identity, made and saved on first use.
    ///
    /// `kind` and `caps` come from the program asking (the daemon is a server, the desktop is a PC),
    /// so they are refreshed every time; only the id and the name are kept.
    pub fn load_or_create(store: &Store, kind: Kind, caps: &[Capability]) -> Result<Self> {
        let path = store.root().join(FILE);
        let saved: Option<Device> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).ok(),
            Err(_) => None,
        };
        let mut device = match saved {
            Some(d) if valid_id(&d.id) => d,
            _ => {
                let mut bytes = [0u8; 16];
                getrandom::getrandom(&mut bytes)
                    .map_err(|e| anyhow::anyhow!("no randomness for a device id: {e}"))?;
                Device {
                    id: hex::encode(bytes),
                    name: default_name(),
                    kind,
                    caps: Vec::new(),
                    version: String::new(),
                    os: String::new(),
                }
            }
        };
        device.kind = kind;
        device.caps = caps.to_vec();
        device.save(store)?;
        device.version = env!("CARGO_PKG_VERSION").to_owned();
        device.os = std::env::consts::OS.to_owned();
        Ok(device)
    }

    /// Writes the id and name back.
    pub fn save(&self, store: &Store) -> Result<()> {
        std::fs::create_dir_all(store.root())
            .with_context(|| format!("creating {}", store.root().display()))?;
        let stored = Device {
            version: String::new(),
            os: String::new(),
            ..self.clone()
        };
        let path = store.root().join(FILE);
        std::fs::write(&path, serde_json::to_vec_pretty(&stored)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// True for a 32-character lowercase hex id.
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A name for the device a person would recognise: the host name, cleaned for display.
pub fn default_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .unwrap_or_default();
    let name = clean_name(&raw);
    if name.is_empty() {
        "Talkee device".to_owned()
    } else {
        name
    }
}

/// Printable ASCII only, at most 64 characters: names go into pairing links and terminals.
pub fn clean_name(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(64)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_survives_and_kind_is_refreshed() {
        let root = std::env::temp_dir().join("talkee-mesh-test-identity");
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::at(&root);
        let first = Device::load_or_create(&store, Kind::Pc, &[Capability::Mic]).unwrap();
        assert!(valid_id(&first.id));
        let second = Device::load_or_create(&store, Kind::Server, &[Capability::Agents]).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.kind, Kind::Server);
        assert_eq!(second.caps, vec![Capability::Agents]);
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  compaq\n"), "compaq");
        assert_eq!(clean_name("a\x1b[31mb"), "a[31mb");
    }
}
