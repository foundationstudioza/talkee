//! The PC's part in the device mesh.
//!
//! Two halves, in two processes:
//!
//! * the app (tray) process listens on TCP 47810 ([`start`]) so paired servers can call this PC;
//! * the window process only edits the pairing files ([`Devices`]): it pairs with a link, forgets a
//!   device, and makes an invite. The listener re-reads `mesh.dat` on every connection, so a change
//!   made in the window is seen straight away.
//!
//! The firewall rule `firewall` installs is per program with no port, so it covers 47810 as it
//! covers the audio ports. No key or link is ever logged or put in an error message here.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use talkee_mesh::node::{self, Handler, Node};
use talkee_mesh::{Capability, Device, Kind, Peer, Peers, MESH_PORT};
use talkee_store::Store;

/// What this PC can do for other devices.
const CAPS: &[Capability] = &[Capability::Mic, Capability::Dictate, Capability::TypeAtCursor];

/// This PC's identity, made on first use.
pub fn this_device(store: &Store) -> Result<Device> {
    Device::load_or_create(store, Kind::Pc, CAPS)
}

/// Answers the operations this PC adds to the built-in ones.
pub struct PcHandler {
    dir: PathBuf,
}

impl PcHandler {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `dictation.status`: is dictation on, is cleanup on, and the minutes used today.
    fn dictation_status(&self) -> Value {
        let config = crate::dictation::current_config(&self.dir);
        let mut body = json!({"enabled": config.enabled, "cleanup": config.cleanup});
        if let Some((used, limit)) = config.usage {
            body["minutes_used"] = json!(used.div_ceil(60).min(limit / 60));
            body["minutes_limit"] = json!(limit / 60);
        }
        body
    }
}

impl Handler for PcHandler {
    fn handle(&self, _peer: &Peer, op: &str, _body: &Value) -> Result<Option<Value>> {
        Ok(match op {
            "dictation.status" => Some(self.dictation_status()),
            _ => None,
        })
    }
}

/// Starts listening for paired devices on a background thread. Returns a line for the log. A busy
/// port or a missing data directory is reported, never fatal.
pub fn start() -> String {
    let store = match Store::open() {
        Ok(s) => s,
        Err(e) => return format!("mesh: off, no place to keep pairings ({e:#})"),
    };
    let device = match this_device(&store) {
        Ok(d) => d,
        Err(e) => return format!("mesh: off, no device identity ({e:#})"),
    };
    let listener = match TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, MESH_PORT))) {
        Ok(l) => l,
        Err(e) => {
            return format!("mesh: port {MESH_PORT} is in use or blocked ({e}); devices cannot call this PC")
        }
    };
    let dir = store.root().to_path_buf();
    let node = match Node::new(store, device) {
        Ok(n) => Arc::new(n),
        Err(e) => return format!("mesh: off, pairings unreadable ({e:#})"),
    };
    let spawned = std::thread::Builder::new().name("talkee-mesh".into()).spawn(move || {
        let _ = node::serve(node, listener, Arc::new(PcHandler::new(dir)));
    });
    match spawned {
        Ok(_) => format!("mesh: listening on port {MESH_PORT} for paired devices"),
        Err(e) => format!("mesh: could not start ({e})"),
    }
}

/// One paired device, as the window lists it. No key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRow {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub last_seen: u64,
    pub addr: String,
}

impl PeerRow {
    fn from(p: &Peer) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            kind: match p.kind {
                Kind::Pc => "PC",
                Kind::Phone => "Phone",
                Kind::Server => "Server",
            },
            last_seen: p.last_seen,
            addr: p.addr.clone().unwrap_or_default(),
        }
    }
}

/// The window's view of the pairing files.
#[derive(Clone)]
pub struct Devices {
    store: Store,
}

impl Devices {
    pub fn open() -> Result<Self> {
        Store::open()
            .map(|store| Self { store })
            .map_err(|_| anyhow!("Talkee has nowhere to keep its settings on this PC"))
    }

    pub fn list(&self) -> Vec<PeerRow> {
        Peers::load(&self.store)
            .map(|p| p.peers.iter().map(PeerRow::from).collect())
            .unwrap_or_default()
    }

    pub fn forget(&self, id: &str) -> Result<()> {
        let mut peers =
            Peers::load(&self.store).map_err(|_| anyhow!("The list of devices could not be read."))?;
        peers.forget(id);
        peers.save(&self.store).map_err(|_| anyhow!("The change could not be saved."))
    }

    /// An invite link for another device to join this PC, naming this PC's LAN address.
    pub fn invite(&self) -> Result<String> {
        let host = talkee_core::local_ipv4_addresses()
            .into_iter()
            .find_map(|ip| match ip {
                IpAddr::V4(v4) => Some(v4.to_string()),
                IpAddr::V6(_) => None,
            })
            .ok_or_else(|| anyhow!("This PC has no network address another device could reach."))?;
        let me = this_device(&self.store).map_err(|_| anyhow!("This PC's device identity could not be read."))?;
        let node = Node::new(self.store.clone(), me)
            .map_err(|_| anyhow!("The list of devices could not be read."))?;
        node.invite_link(&host, MESH_PORT)
            .map_err(|_| anyhow!("Could not make an invite."))
    }

    /// Pairs with the device that printed `link`. Blocks on the network: call from a thread.
    pub fn join(&self, link: &str) -> Result<PeerRow> {
        let me = this_device(&self.store).map_err(|_| anyhow!("This PC's device identity could not be read."))?;
        node::join(&self.store, &me, link)
            .map(|p| PeerRow::from(&p))
            .map_err(|e| anyhow!(plain_join_error(&format!("{e:#}"))))
    }
}

/// Turns a join failure into a sentence for a person. Nothing from the link is repeated.
pub fn plain_join_error(raw: &str) -> String {
    let r = raw.to_ascii_lowercase();
    if r.contains("connecting to") || r.contains("timed out") || r.contains("refused") || r.contains("resolving") {
        "Could not reach that device. Is it on, on the same network, and is its Talkee running?".into()
    } else if r.contains("already used")
        || r.contains("expired")
        || r.contains("hung up")
        || r.contains("authenticat")
        || r.contains("unknown key")
    {
        "That link has expired or was already used. Make a new one on the other device.".into()
    } else if r.contains("link") || r.contains("parse") || r.contains("talkee://") || r.contains("scheme") {
        "That is not a Talkee pairing link. It starts with talkee://pair".into()
    } else {
        "Pairing did not work. Make a new link on the other device and try again.".into()
    }
}

/// "just now", "5 min ago", "2 h ago", "3 days ago".
pub fn ago(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_plainly() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(1000, 700), "5 min ago");
        assert_eq!(ago(10_000, 0), "2 h ago");
        assert_eq!(ago(300_000, 0), "3 days ago");
        assert_eq!(ago(5, 10), "just now");
    }

    #[test]
    fn errors_do_not_echo_the_link() {
        let msg = plain_join_error("connecting to 10.0.0.2:47810 key=SECRET");
        assert!(!msg.contains("SECRET") && !msg.contains("10.0.0.2"));
        assert!(plain_join_error("bad link").contains("pairing link"));
    }

    #[test]
    fn the_handler_answers_dictation_status_and_nothing_else() {
        let dir = std::env::temp_dir().join("talkee-desktop-mesh-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let handler = PcHandler::new(dir);
        let body = handler.dictation_status();
        assert!(body["enabled"].is_boolean() && body["cleanup"].is_boolean());
        let peer = Peer::new(
            "a".repeat(32),
            "x".into(),
            Kind::Server,
            vec![],
            &talkee_crypto::Psk::generate().unwrap(),
            None,
        );
        assert!(handler.handle(&peer, "dictation.status", &Value::Null).unwrap().is_some());
        assert!(handler.handle(&peer, "nope", &Value::Null).unwrap().is_none());
    }

    #[test]
    fn a_bad_link_is_refused_in_plain_words() {
        let root = std::env::temp_dir().join("talkee-desktop-mesh-test-pair");
        let _ = std::fs::remove_dir_all(&root);
        let pc = Devices { store: Store::at(&root) };
        assert!(pc.list().is_empty());
        let err = pc.join("garbage").unwrap_err().to_string();
        assert!(err.contains("pairing link"), "{err}");
    }

    #[test]
    fn pairing_with_a_listening_server_and_forgetting_it() {
        let s_root = std::env::temp_dir().join("talkee-desktop-mesh-test-server");
        let c_root = std::env::temp_dir().join("talkee-desktop-mesh-test-pc");
        let _ = std::fs::remove_dir_all(&s_root);
        let _ = std::fs::remove_dir_all(&c_root);
        let s_store = Store::at(&s_root);
        let server_dev = Device::load_or_create(&s_store, Kind::Server, &[Capability::Agents]).unwrap();
        let server = Arc::new(Node::new(s_store, server_dev).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let link = server.invite_link("127.0.0.1", port).unwrap();
        {
            let server = server.clone();
            let handler: Arc<dyn Handler> = Arc::new(PcHandler::new(s_root.clone()));
            std::thread::spawn(move || node::serve(server, listener, handler));
        }
        let pc = Devices { store: Store::at(&c_root) };
        let row = pc.join(&link).unwrap();
        assert_eq!(row.kind, "Server");
        assert_eq!(pc.list().len(), 1);
        pc.forget(&row.id).unwrap();
        assert!(pc.list().is_empty());
    }
}
