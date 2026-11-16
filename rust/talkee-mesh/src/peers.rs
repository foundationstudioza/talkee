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
    /// The key this pair used before an introduction replaced it, still accepted and tried, so a
    /// pair where only one side has heard of the new key keeps working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    old_key: Option<String>,
    /// Where it was last reached (`host:port`), if this side dials it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// Other addresses it said it has (LAN, Tailscale), tried after `addr`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alt_addrs: Vec<String>,
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
            old_key: None,
            addr,
            alt_addrs: Vec::new(),
            paired_at: now(),
            last_seen: now(),
        }
    }

    pub fn psk(&self) -> Result<Psk> {
        Psk::from_base64url(&self.key)
    }

    /// Every key this pair may be using, newest first.
    pub fn keys(&self) -> Vec<Psk> {
        std::iter::once(&self.key)
            .chain(self.old_key.as_ref())
            .filter_map(|k| Psk::from_base64url(k).ok())
            .collect()
    }

    /// The key's public fingerprint, which is how a connecting device says which key it holds.
    pub fn fingerprint(&self) -> String {
        self.psk().map(|k| k.fingerprint()).unwrap_or_default()
    }
}

/// An introduction: "here is a device in the mesh, and the key you share with it".
///
/// When a device joins one member, that member makes a fresh key for the newcomer and each of its
/// other peers and tells both sides, so nobody pairs every device with every other device by hand.
/// One key per pair still holds: the introducer knows it, and nobody else.
#[derive(Clone, Serialize, Deserialize)]
pub struct Intro {
    /// Who this introduction is for (set while it waits in the queue).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub for_peer: String,
    pub id: String,
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub caps: Vec<Capability>,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alt_addrs: Vec<String>,
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
    /// Introductions waiting to be delivered to a peer that was not reachable.
    #[serde(default)]
    pub pending: Vec<Intro>,
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

    /// Reads `mesh.dat`, applies `f` and saves, holding a process-wide lock throughout, so two
    /// threads changing it at once cannot lose each other's change.
    pub fn update<R>(store: &Store, f: impl FnOnce(&mut Peers) -> R) -> Result<R> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut peers = Peers::load(store)?;
        let r = f(&mut peers);
        peers.save(store)?;
        Ok(r)
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
            for k in p.keys() {
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

    /// Takes in an introduction. A device already paired keeps its old key as a fallback, so a
    /// side that has not heard of the new key yet still gets in. Returns true if anything changed.
    pub fn apply(&mut self, intro: &Intro, my_id: &str) -> bool {
        if intro.id == my_id || !crate::identity::valid_id(&intro.id) {
            return false;
        }
        if Psk::from_base64url(&intro.key).is_err() {
            return false;
        }
        if let Some(p) = self.get_mut(&intro.id) {
            if p.key == intro.key {
                return false;
            }
            p.old_key = Some(std::mem::replace(&mut p.key, intro.key.clone()));
            if p.addr.is_none() {
                p.addr = intro.addr.clone();
            }
            for a in &intro.alt_addrs {
                if !p.alt_addrs.contains(a) {
                    p.alt_addrs.push(a.clone());
                }
            }
            return true;
        }
        self.peers.push(Peer {
            id: intro.id.clone(),
            name: crate::identity::clean_name(&intro.name),
            kind: intro.kind,
            caps: intro.caps.clone(),
            key: intro.key.clone(),
            old_key: None,
            addr: intro.addr.clone(),
            alt_addrs: intro.alt_addrs.clone(),
            paired_at: now(),
            last_seen: 0,
        });
        true
    }

    /// Makes introductions between `newcomer` and every other peer: returns the ones for the
    /// newcomer, and queues the matching ones for the others in [`Peers::pending`].
    pub fn introduce(&mut self, newcomer: &str, newcomer_addr: Option<String>) -> Result<Vec<Intro>> {
        let Some(new) = self.get(newcomer).cloned() else { return Ok(Vec::new()) };
        let mut for_newcomer = Vec::new();
        for p in self.peers.clone() {
            if p.id == new.id {
                continue;
            }
            let key = Psk::generate()?.to_base64url();
            for_newcomer.push(Intro {
                for_peer: String::new(),
                id: p.id.clone(),
                name: p.name.clone(),
                kind: p.kind,
                caps: p.caps.clone(),
                key: key.clone(),
                addr: p.addr.clone(),
                alt_addrs: p.alt_addrs.clone(),
            });
            self.pending.push(Intro {
                for_peer: p.id.clone(),
                id: new.id.clone(),
                name: new.name.clone(),
                kind: new.kind,
                caps: new.caps.clone(),
                key,
                addr: newcomer_addr.clone(),
                alt_addrs: new.alt_addrs.clone(),
            });
        }
        Ok(for_newcomer)
    }

    /// Removes and returns the introductions waiting for `peer`.
    /// Every address worth trying for a peer: the saved one, then the others it reported.
    pub fn dial_addrs(p: &Peer) -> Vec<String> {
        let mut out: Vec<String> = p.addr.iter().cloned().collect();
        for a in &p.alt_addrs {
            if !out.contains(a) {
                out.push(a.clone());
            }
        }
        out
    }

    pub fn take_pending(&mut self, peer: &str) -> Vec<Intro> {
        let (mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.pending).into_iter().partition(|i| i.for_peer == peer);
        self.pending = rest;
        mine
    }

    /// Forgets a peer. Its key stops working at once.
    pub fn forget(&mut self, id: &str) -> bool {
        self.pending.retain(|i| i.for_peer != id && i.id != id);
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
