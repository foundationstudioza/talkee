//! Zero-configuration discovery.
//!
//! # Why this exists so early
//!
//! The product requirement is that a user does nothing: open the app and audio
//! flows. No IP address, no port number, no pairing code, no settings screen.
//! That is a *hard* constraint, not polish, and it means discovery has to be
//! designed in rather than bolted on — the phone has to find the desktop on its
//! own before any of the rest matters.
//!
//! # The split of responsibilities
//!
//! The desktop **advertises**; the phone **browses**. This crate implements both
//! halves so it can test itself, but in the shipping app the phone side will use
//! Android's built-in `NsdManager` rather than this code. That is not
//! duplication for its own sake: raw multicast on Android needs a
//! `WifiManager.MulticastLock`, which is easy to get wrong and which
//! `NsdManager` handles for us.
//!
//! `talkee discover` on the desktop is therefore both a diagnostic and the
//! reference implementation of the query side.

use anyhow::{anyhow, Result};
use mdns_sd::{Receiver, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// The mDNS service type. Length must be <= 15 bytes for the service label, and
/// `_talkee` is 7.
pub const SERVICE_TYPE: &str = "_talkee._tcp.local.";

/// TXT record keys.
///
/// Kept to two characters because a TXT record has very little room, and every
/// byte here is a byte not spent on audio.
pub mod txt {
    /// The UDP port that carries audio. The SRV port carries control.
    pub const AUDIO_PORT: &str = "ap";
    /// Protocol version, so a mismatched pair fails fast and visibly.
    pub const VERSION: &str = "v";
    /// Human-readable machine name, for when there is more than one host.
    pub const NAME: &str = "n";
}

/// A Talkee host found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    /// mDNS instance name, e.g. `Talkee on DESKTOP-ABC`.
    pub instance: String,
    /// The host's DNS name, e.g. `desktop-abc.local.`.
    pub host: String,
    /// IPv4 addresses. IPv6 is ignored: this is a LAN feature and IPv4 is what
    /// works everywhere on a home network.
    pub addresses: Vec<Ipv4Addr>,
    /// The control port, which is the SRV record's port.
    pub control_port: u16,
    /// The audio port from the TXT record, falling back to the control port.
    pub audio_port: u16,
    /// Protocol version from the TXT record, if the host published one.
    pub protocol_version: Option<u8>,
}

impl HostInfo {
    /// Builds a [`HostInfo`] from an mDNS resolution result.
    pub fn from_resolved(resolved: ResolvedService) -> Self {
        let instance = instance_from_fullname(&resolved.fullname);
        let mut addresses: Vec<Ipv4Addr> = resolved.get_addresses_v4().into_iter().collect();
        // Deterministic order, so "first host found" does not depend on hash
        // iteration and produce different answers on different runs.
        addresses.sort();
        // Default the audio port to the control port rather than to 0, so a host
        // that forgot its TXT record is still worth trying.
        let audio_port = resolved
            .get_property_val_str(txt::AUDIO_PORT)
            .and_then(|s| s.parse().ok())
            .unwrap_or(resolved.port);
        let protocol_version = resolved
            .get_property_val_str(txt::VERSION)
            .and_then(|s| s.parse().ok());
        Self {
            instance,
            host: resolved.host,
            addresses,
            control_port: resolved.port,
            audio_port,
            protocol_version,
        }
    }

    /// The address to send audio to, or `None` if the host resolved to nothing
    /// reachable.
    pub fn best_address(&self) -> Option<Ipv4Addr> {
        self.addresses
            .iter()
            .copied()
            .find(|a| talkee_core::is_lan_reachable(&std::net::IpAddr::V4(*a)))
    }
}

/// Strips the service type off an mDNS full name to recover the instance name.
///
/// `"Talkee on DESKTOP._talkee._tcp.local."` -> `"Talkee on DESKTOP"`.
fn instance_from_fullname(fullname: &str) -> String {
    let with_dot = format!(".{SERVICE_TYPE}");
    fullname
        .strip_suffix(with_dot.as_str())
        .or_else(|| fullname.strip_suffix(SERVICE_TYPE))
        .unwrap_or(fullname)
        .to_string()
}

/// A sensible advertisement name, derived from the machine name.
pub fn default_instance_name() -> String {
    let raw = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "computer".to_owned());
    format!("Talkee on {raw}")
}

/// Turns an instance name into a legal DNS host label.
///
/// DNS labels are letters, digits and hyphens. A space in a host name makes the
/// A record unresolvable, which presents as "discovery works but connection
/// fails" — a genuinely confusing bug, so it is normalised here and tested.
fn sanitize_host_label(instance: &str) -> String {
    let mut out = String::with_capacity(instance.len());
    let mut last_was_dash = false;
    for ch in instance.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "talkee".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// A live advertisement. Dropping it withdraws the service.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    /// The full mDNS name other devices see, e.g.
    /// `Talkee on DESKTOP-ABC._talkee._tcp.local.`
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // Best effort: this normally runs as the process is exiting anyway.
        let _ = self.daemon.shutdown();
    }
}

/// Advertises this desktop so that phones can find it with no configuration.
///
/// `control_port` becomes the SRV record's port; the audio port travels in a TXT
/// record, because a service has only one SRV port and we need two.
pub fn advertise(
    instance: &str,
    control_port: u16,
    audio_port: u16,
    protocol_version: u8,
) -> Result<Advertisement> {
    let daemon =
        ServiceDaemon::new().map_err(|e| anyhow!("could not start the mDNS daemon: {e}"))?;

    // Advertise only addresses a phone on the same LAN can actually reach.
    //
    // We deliberately do NOT call `ServiceInfo::enable_addr_auto()` here, even
    // though it would handle Wi-Fi roaming for free. On a multi-homed machine it
    // publishes *every* interface address, and measured here that meant
    // link-local (169.254.x), WSL (172.22.x) and Tailscale (100.118.x) alongside
    // the real 192.168.0.24. The result was a host that phones could **discover
    // but not connect to**, because the resolver surfaced a link-local address.
    //
    // Roaming support therefore needs to be "re-advertise when the chosen
    // address changes" rather than "publish everything", which is M4 work.
    let addresses: Vec<String> = talkee_core::local_ipv4_addresses()
        .iter()
        .filter(|ip| talkee_core::is_lan_reachable(ip))
        .map(|ip| ip.to_string())
        .collect();
    if addresses.is_empty() {
        return Err(anyhow!(
            "no LAN address to advertise. Talkee streams over the local network, so \
             connect to Wi-Fi or Ethernet first."
        ));
    }

    // These must outlive the `properties` slice below; binding them to locals
    // rather than inlining `&port.to_string()` is required to avoid borrowing a
    // temporary.
    let audio_port_text = audio_port.to_string();
    let version_text = protocol_version.to_string();
    let properties = [
        (txt::NAME, instance),
        (txt::AUDIO_PORT, audio_port_text.as_str()),
        (txt::VERSION, version_text.as_str()),
    ];

    let host_name = format!("{}.local.", sanitize_host_label(instance));
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        instance,
        &host_name,
        addresses.as_slice(),
        control_port,
        &properties[..],
    )
    .map_err(|e| anyhow!("could not build the mDNS service record: {e}"))?;

    daemon
        .register(info)
        .map_err(|e| anyhow!("could not register with mDNS: {e}"))?;

    Ok(Advertisement {
        daemon,
        fullname: format!("{instance}.{SERVICE_TYPE}"),
    })
}

/// Browses for hosts. Stops when dropped.
pub struct Browser {
    daemon: ServiceDaemon,
    events: Receiver<ServiceEvent>,
}

impl Browser {
    /// Starts browsing for Talkee hosts.
    pub fn start() -> Result<Self> {
        let daemon =
            ServiceDaemon::new().map_err(|e| anyhow!("could not start the mDNS daemon: {e}"))?;
        let events = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| anyhow!("could not browse for hosts: {e}"))?;
        Ok(Self { daemon, events })
    }

    /// Waits up to `timeout` for a fully resolved host.
    ///
    /// Uses a single deadline for the whole call: `ServiceFound` and
    /// `SearchStarted` events arrive constantly and would otherwise reset the
    /// timeout on every iteration and spin.
    pub fn next(&self, timeout: Duration) -> Option<HostInfo> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.events.recv_timeout(remaining) {
                // Only resolved services have usable addresses; anything else is
                // an intermediate event.
                Ok(ServiceEvent::ServiceResolved(resolved)) if resolved.is_valid() => {
                    return Some(HostInfo::from_resolved(*resolved));
                }
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// Returns every distinct host found within `timeout`.
pub fn discover(timeout: Duration) -> Result<Vec<HostInfo>> {
    let browser = Browser::start()?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<HostInfo> = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match browser.next(remaining) {
            Some(info) => {
                if !found.iter().any(|f| f.instance == info.instance) {
                    found.push(info);
                }
            }
            None => break,
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a resolved service without touching the network, so these tests
    /// are hermetic.
    fn resolved(ip: &str, control_port: u16, props: &[(&str, &str)]) -> HostInfo {
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "Talkee on DESKTOP",
            "desktop.local.",
            ip,
            control_port,
            props,
        )
        .expect("valid service info");
        HostInfo::from_resolved(info.as_resolved_service())
    }

    #[test]
    fn instance_name_is_recovered_from_the_full_name() {
        assert_eq!(
            instance_from_fullname("Talkee on DESKTOP._talkee._tcp.local."),
            "Talkee on DESKTOP"
        );
    }

    #[test]
    fn instance_name_survives_an_unexpected_full_name() {
        // Must not panic or truncate to nothing on input we did not expect.
        assert_eq!(instance_from_fullname("something else"), "something else");
    }

    #[test]
    fn resolved_host_carries_ports_and_version() {
        let host = resolved("192.168.0.24", 47_800, &[("ap", "47801"), ("v", "1")]);
        assert_eq!(host.instance, "Talkee on DESKTOP");
        assert_eq!(host.host, "desktop.local.");
        assert_eq!(host.control_port, 47_800);
        assert_eq!(host.audio_port, 47_801, "audio port must come from the TXT record");
        assert_eq!(host.protocol_version, Some(1));
        assert_eq!(host.best_address(), Some(Ipv4Addr::new(192, 168, 0, 24)));
    }

    #[test]
    fn audio_port_falls_back_to_the_control_port() {
        // A host that forgot its TXT record is still worth trying rather than
        // being discarded as port 0.
        let host = resolved("192.168.0.24", 47_800, &[]);
        assert_eq!(host.audio_port, 47_800);
        assert_eq!(host.protocol_version, None);
    }

    #[test]
    fn a_garbage_txt_value_does_not_break_resolution() {
        let host = resolved("192.168.0.24", 47_800, &[("ap", "not-a-port"), ("v", "x")]);
        assert_eq!(host.audio_port, 47_800);
        assert_eq!(host.protocol_version, None);
    }

    #[test]
    fn unroutable_addresses_are_not_offered() {
        // A link-local address means the host is not actually reachable.
        let host = resolved("169.254.10.20", 47_800, &[]);
        assert_eq!(host.best_address(), None);
    }

    #[test]
    fn a_routable_address_is_preferred_over_a_link_local_one() {
        let host = resolved("169.254.10.20,192.168.0.5", 47_800, &[]);
        assert_eq!(host.best_address(), Some(Ipv4Addr::new(192, 168, 0, 5)));
    }

    #[test]
    fn addresses_come_back_in_a_stable_order() {
        // "First host found" must not depend on HashSet iteration order.
        let a = resolved("192.168.0.5,192.168.0.4", 47_800, &[]);
        let b = resolved("192.168.0.4,192.168.0.5", 47_800, &[]);
        assert_eq!(a.addresses, b.addresses);
    }

    #[test]
    fn host_labels_are_made_dns_safe() {
        // A space in a DNS host label makes the A record unresolvable, which
        // presents as "discovery works, connection fails".
        assert_eq!(sanitize_host_label("Talkee on DESKTOP-ABC"), "talkee-on-desktop-abc");
        assert_eq!(sanitize_host_label("  weird   name!! "), "weird-name");
        assert_eq!(sanitize_host_label(""), "talkee");
        assert_eq!(sanitize_host_label("---"), "talkee");
        assert_eq!(sanitize_host_label("Ünïcode hôst"), "n-code-h-st");
    }

    #[test]
    fn default_instance_name_is_never_empty() {
        let name = default_instance_name();
        assert!(!name.trim().is_empty());
        assert!(name.starts_with("Talkee on "));
    }

    /// Not run by default: needs a live network, and needs no other Talkee
    /// instance already bound to UDP 5353 in a conflicting way.
    ///
    /// Run with `cargo test -p talkee-discovery -- --ignored`.
    #[test]
    #[ignore = "requires a live network"]
    fn live_advertise_then_discover_round_trip() {
        let instance = "Talkee live test";
        let advertisement =
            advertise(instance, 47_800, 47_801, 1).expect("advertising needs a LAN address");
        assert!(advertisement.fullname().contains(SERVICE_TYPE));

        let hosts = discover(Duration::from_secs(5)).expect("browsing should start");
        let mine = hosts
            .iter()
            .find(|h| h.instance == instance)
            .unwrap_or_else(|| panic!("our own advertisement was not discovered: {hosts:?}"));
        assert_eq!(mine.audio_port, 47_801);
        assert_eq!(mine.protocol_version, Some(1));
        assert!(mine.best_address().is_some());
    }
}


