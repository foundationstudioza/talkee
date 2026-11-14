//! The devices this one is paired with, and the invites it has handed out.
//!
//! # One key per pair
//!
//! Every pair of devices has its own 32-byte key. Forgetting one device revokes exactly that device,
//! and a key copied off one phone opens nothing else. A key starts life as an [`Invite`]: made by
//! `talkeed pair` (or the desktop), shown once as a link, single use, gone after
//! [`INVITE_TTL_SECS`]. The first device to complete the handshake with it becomes a [`Peer`].
//!
//! # Where it lives
//!
//! `mesh.dat` in the store, as a protected secret (DPAPI on Windows, a 0600 file on Linux): it
//! holds keys.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use talkee_crypto::Psk;
use talkee_store::Store;

use crate::identity::{Capability, Kind};

const FILE: &str = "mesh.dat";
const MAGIC: &[u8; 8] = b"TALKEEMS";

/// How long an invite link works. Long enough to walk to the other machine, short enough that a
/// link left in a terminal's scrollback is dead by the time anyone finds it.
pub const INVITE_TTL_SECS: u64 = 15 * 60;

/// A paired device. `Debug` is written by hand so the key never reaches a log.
#[derive(Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub caps: Vec<Capability>,
    /// The pair's key, base64url. Never printed.
    key: String,
    /// Where it was last reached (`host:port`), if this side dials it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// Unix seconds.
    pub paired_at: u64,
    #[serde(default)]
    pub last_seen: u64,
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("addr", &self.addr)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl Peer {
    pub fn new(
        id: String,
        name: String,
        kind: Kind,
        caps: Vec<Capability>,
        psk: &Psk,
        addr: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            kind,
            caps,
            key: psk.to_base64url(),
            addr,
            paired_at: now(),
            last_seen: now(),
        }
    }

    pub fn psk(&self) -> Result<Psk> {
        Psk::from_base64url(&self.key)
    }

    /// The key's public fingerprint, which is how a connecting device says which key it holds.
    pub fn fingerprint(&self) -> String {
        self.psk().map(|k| k.fingerprint()).unwrap_or_default()
    }
}

/// A key handed out in a link, waiting for a device to use it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Invite {
    key: String,
    pub expires: u64,
}

impl Invite {
    pub fn psk(&self) -> Result<Psk> {
        Psk::from_base64url(&self.key)
    }
}

/// Everything in `mesh.dat`.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Peers {
    #[serde(default)]
    pub peers: Vec<Peer>,
    #[serde(default)]
    invites: Vec<Invite>,
}

/// What a key presented in a handshake turned out to be.
#[derive(Debug, Clone)]
pub enum KeyMatch {
    Peer(Psk, String),
    Invite(Psk),
}

impl Peers {
    pub fn load(store: &Store) -> Result<Self> {
        let Some(plain) = store.load_secret(FILE)? else {
            return Ok(Self::default());
        };
        let json = plain
            .strip_prefix(MAGIC.as_slice())
            .context("mesh.dat is not a Talkee mesh file")?;
        serde_json::from_slice(json).context("reading mesh.dat")
    }

    pub fn save(&self, store: &Store) -> Result<()> {
        let mut plain = MAGIC.to_vec();
        plain.extend_from_slice(&serde_json::to_vec(self)?);
        store.save_secret(FILE, &plain)
    }

    /// Makes a new single-use invite and returns its key.
    pub fn invite(&mut self) -> Result<Psk> {
        self.prune();
        let psk = Psk::generate()?;
        self.invites.push(Invite {
            key: psk.to_base64url(),
            expires: now() + INVITE_TTL_SECS,
        });
        Ok(psk)
    }

    /// Drops invites that have expired.
    pub fn prune(&mut self) {
        let t = now();
        self.invites.retain(|i| i.expires > t);
    }

    pub fn pending_invites(&self) -> usize {
        let t = now();
        self.invites.iter().filter(|i| i.expires > t).count()
    }

    /// The key a connecting device holds, by its fingerprint: a paired device's, or a live invite's.
    pub fn find_key(&self, fingerprint: &str) -> Option<KeyMatch> {
        for p in &self.peers {
            if let Ok(k) = p.psk() {
                if k.fingerprint() == fingerprint {
                    return Some(KeyMatch::Peer(k, p.id.clone()));
                }
            }
        }
        let t = now();
        self.invites
            .iter()
            .filter(|i| i.expires > t)
            .filter_map(|i| i.psk().ok())
            .find(|k| k.fingerprint() == fingerprint)
            .map(KeyMatch::Invite)
    }

    /// Turns the invite with this key into a peer (or refreshes the peer with this id).
    ///
    /// The invite is consumed: a second device holding the same link gets nowhere.
    pub fn accept(&mut self, psk: &Psk, peer: Peer) {
        let fp = psk.fingerprint();
        self.invites
            .retain(|i| i.psk().map(|k| k.fingerprint() != fp).unwrap_or(false));
        self.upsert(peer);
    }

    /// Adds a peer, replacing one with the same id.
    pub fn upsert(&mut self, peer: Peer) {
        self.peers.retain(|p| p.id != peer.id);
        self.peers.push(peer);
    }

    pub fn get(&self, id_or_name: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.id == id_or_name).or_else(|| {
            self.peers
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(id_or_name) || p.id.starts_with(id_or_name))
        })
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Peer> {
        self.peers.iter_mut().find(|p| p.id == id)
    }

    /// Forgets a peer. Its key stops working at once.
    pub fn forget(&mut self, id: &str) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| p.id != id);
        self.peers.len() != before
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_is_single_use_and_survives_a_restart() {
        let root = std::env::temp_dir().join("talkee-mesh-test-peers");
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::at(&root);
        let mut peers = Peers::load(&store).unwrap();
        let key = peers.invite().unwrap();
        peers.save(&store).unwrap();

        let mut peers = Peers::load(&store).unwrap();
        assert!(matches!(peers.find_key(&key.fingerprint()), Some(KeyMatch::Invite(_))));
        let peer = Peer::new("a".repeat(32), "pc".into(), Kind::Pc, vec![], &key, None);
        peers.accept(&key, peer);
        assert_eq!(peers.pending_invites(), 0);
        assert!(matches!(peers.find_key(&key.fingerprint()), Some(KeyMatch::Peer(..))));
        assert!(peers.forget(&"a".repeat(32)));
        assert!(peers.find_key(&key.fingerprint()).is_none());
    }

    #[test]
    fn the_key_is_not_in_debug_output() {
        let key = Psk::generate().unwrap();
        let peer = Peer::new("b".repeat(32), "x".into(), Kind::Pc, vec![], &key, None);
        assert!(!format!("{peer:?}").contains(&key.to_base64url()));
    }
}
