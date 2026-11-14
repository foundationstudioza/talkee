//! Mesh discovery: paired devices find each other again after an address changes.
//!
//! A separate mDNS service, `_talkee-mesh._tcp.local.`, on the mesh port. The microphone service
//! (`_talkee._tcp`) is untouched. TXT records carry only labels: device id, name, kind and mesh
//! version. No key and no key fingerprint is published; the id is not a secret, and a device found
//! this way still has to prove the pair's key in the handshake before anything is answered.
//!
//! Kept free of `talkee-mesh` types (kind is a string) so the dependency points one way.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::{sanitize_host_label, Advertisement};

pub const MESH_SERVICE_TYPE: &str = "_talkee-mesh._tcp.local.";

/// TXT keys.
pub mod txt {
    pub const ID: &str = "id";
    pub const NAME: &str = "n";
    pub const KIND: &str = "k";
    pub const VERSION: &str = "v";
}

/// A mesh node seen on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshHost {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub version: u32,
    pub addresses: Vec<Ipv4Addr>,
    pub port: u16,
}

impl MeshHost {
    /// `ip:port` strings, LAN-reachable addresses first, for dialling.
    pub fn dial_addrs(&self) -> Vec<String> {
        let mut addrs: Vec<Ipv4Addr> = self.addresses.clone();
        addrs.sort_by_key(|a| !talkee_core::is_lan_reachable(&std::net::IpAddr::V4(*a)));
        addrs.iter().map(|a| format!("{a}:{}", self.port)).collect()
    }
}

/// The TXT records for a node. Names are cut to fit a TXT string (255 bytes with the key).
pub fn encode_txt(id: &str, name: &str, kind: &str, version: u32) -> Vec<(String, String)> {
    let mut name = name.to_owned();
    while name.len() > 100 {
        name.pop();
    }
    vec![
        (txt::ID.into(), id.into()),
        (txt::NAME.into(), name),
        (txt::KIND.into(), kind.into()),
        (txt::VERSION.into(), version.to_string()),
    ]
}

/// Reads TXT records back. `None` when there is no usable id (anything else is optional).
pub fn parse_txt(get: impl Fn(&str) -> Option<String>) -> Option<(String, String, String, u32)> {
    let id = get(txt::ID)?;
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        id,
        get(txt::NAME).unwrap_or_default(),
        get(txt::KIND).unwrap_or_default(),
        get(txt::VERSION).and_then(|v| v.parse().ok()).unwrap_or(0),
    ))
}

/// Advertises a mesh node on `port`. Dropping the result withdraws it.
pub fn advertise_mesh(id: &str, name: &str, kind: &str, version: u32, port: u16) -> Result<Advertisement> {
    let daemon = ServiceDaemon::new().map_err(|e| anyhow!("could not start the mDNS daemon: {e}"))?;
    // Same rule as the microphone service: only addresses a LAN peer can use (not link-local,
    // WSL or Tailscale). Tailscale peers use `talkee mesh set-addr` instead.
    let addresses: Vec<String> = talkee_core::local_ipv4_addresses()
        .iter()
        .filter(|ip| talkee_core::is_lan_reachable(ip))
        .map(|ip| ip.to_string())
        .collect();
    if addresses.is_empty() {
        return Err(anyhow!("no LAN address to advertise"));
    }
    // The id prefix keeps the instance unique even if two devices share a name.
    let instance = format!("talkee-{}", &id[..id.len().min(12)]);
    let props: HashMap<String, String> = encode_txt(id, name, kind, version).into_iter().collect();
    let host_name = format!("{}.local.", sanitize_host_label(&instance));
    let info = ServiceInfo::new(MESH_SERVICE_TYPE, &instance, &host_name, addresses.as_slice(), port, props)
        .map_err(|e| anyhow!("could not build the mDNS record: {e}"))?;
    daemon.register(info).map_err(|e| anyhow!("could not register with mDNS: {e}"))?;
    Ok(Advertisement::new(daemon, format!("{instance}.{MESH_SERVICE_TYPE}")))
}

/// Browses for mesh nodes for up to `timeout`, calling `keep_going` with each one found; stops
/// early when it returns `false`.
pub fn browse_mesh(timeout: Duration, mut keep_going: impl FnMut(MeshHost) -> bool) -> Result<()> {
    let daemon = ServiceDaemon::new().map_err(|e| anyhow!("could not start the mDNS daemon: {e}"))?;
    let events = daemon
        .browse(MESH_SERVICE_TYPE)
        .map_err(|e| anyhow!("could not browse: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match events.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(r)) if r.is_valid() => {
                let Some((id, name, kind, version)) =
                    parse_txt(|k| r.get_property_val_str(k).map(str::to_owned))
                else {
                    continue;
                };
                let mut addresses: Vec<Ipv4Addr> = r.get_addresses_v4().into_iter().collect();
                addresses.sort();
                let host = MeshHost { id, name, kind, version, addresses, port: r.port };
                if !keep_going(host) {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    Ok(())
}

/// Every mesh node found within `timeout`.
pub fn discover_mesh(timeout: Duration) -> Result<Vec<MeshHost>> {
    let mut found: Vec<MeshHost> = Vec::new();
    browse_mesh(timeout, |h| {
        if !found.iter().any(|f| f.id == h.id) {
            found.push(h);
        }
        true
    })?;
    Ok(found)
}

/// Looks for one device by id; returns the addresses to dial, best first. This is the resolver
/// the PC hands to `talkee_mesh::node::connect_peer_with`.
pub fn resolve_mesh_peer(id: &str, timeout: Duration) -> Vec<String> {
    let mut out = Vec::new();
    let _ = browse_mesh(timeout, |h| {
        if h.id == id {
            out = h.dial_addrs();
            false
        } else {
            true
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn txt_round_trips_and_carries_no_key_material() {
        let t = encode_txt(ID, "compaq", "server", 1);
        let map: HashMap<_, _> = t.iter().cloned().collect();
        let (id, name, kind, v) = parse_txt(|k| map.get(k).cloned()).unwrap();
        assert_eq!((id.as_str(), name.as_str(), kind.as_str(), v), (ID, "compaq", "server", 1));
        let keys: Vec<&str> = t.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["id", "n", "k", "v"], "only labels are published");
    }

    #[test]
    fn bad_or_missing_id_is_rejected() {
        assert!(parse_txt(|_| None).is_none());
        assert!(parse_txt(|k| (k == "id").then(|| "short".to_owned())).is_none());
        assert!(parse_txt(|k| (k == "id").then(|| "z".repeat(32))).is_none());
    }

    #[test]
    fn missing_optional_fields_default() {
        let (_, name, kind, v) = parse_txt(|k| (k == "id").then(|| ID.to_owned())).unwrap();
        assert_eq!((name.as_str(), kind.as_str(), v), ("", "", 0));
    }

    #[test]
    fn long_names_are_cut() {
        let t = encode_txt(ID, &"x".repeat(500), "pc", 1);
        assert!(t[1].1.len() <= 100);
    }

    #[test]
    fn dial_addrs_put_lan_first() {
        let h = MeshHost {
            id: ID.into(),
            name: "n".into(),
            kind: "server".into(),
            version: 1,
            addresses: vec![Ipv4Addr::new(169, 254, 1, 1), Ipv4Addr::new(192, 168, 137, 5)],
            port: 47810,
        };
        assert_eq!(h.dial_addrs()[0], "192.168.137.5:47810");
    }
}
