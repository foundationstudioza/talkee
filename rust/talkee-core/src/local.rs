//! Finding this machine's own LAN addresses.
//!
//! Used to print "point the phone at ..." and to advertise over mDNS. Kept in
//! `talkee-core` rather than the CLI so that the desktop app and any future tool
//! share one implementation.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// Best-effort list of this machine's non-loopback IPv4 addresses.
///
/// Uses the classic trick of "connecting" a UDP socket to a remote address. No
/// packets are sent for UDP — the kernel just selects the interface it would use
/// and reports the local address. That avoids enumerating interfaces, and so
/// avoids a platform-specific dependency, for what is only a convenience.
///
/// **Returns an empty list when there is no default route**, which is the normal
/// state on a machine that is not currently on any network. Callers must handle
/// that rather than assuming at least one address exists.
///
/// Deliberately not deduplicated or sorted: there is usually exactly one entry,
/// and callers that care can sort.
pub fn local_ipv4_addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        // A public address is used only to pick a route; nothing is transmitted.
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(local) = socket.local_addr() {
                let ip = local.ip();
                if ip.is_ipv4() && !ip.is_loopback() && !ip.is_unspecified() {
                    out.push(ip);
                }
            }
        }
    }
    out
}

/// True if a peer on the *same local network* could plausibly reach this address.
///
/// # Why this is stricter than "is it routable"
///
/// The name says LAN-reachable, not routable, because on a modern developer
/// machine those are very different questions. Measured on the machine this was
/// written for, there were **eight** IPv4 addresses:
///
/// ```text
/// 172.22.160.1    vEthernet (WSL)
/// 169.254.42.130  Local Area Connection* 10   <- link-local
/// 169.254.62.125  Local Area Connection* 9    <- link-local
/// 192.168.137.1   Ethernet                    <- ICS / mobile hotspot
/// 169.254.195.79  Ethernet                    <- link-local
/// 192.168.0.24    WiFi                        <- the real one
/// 100.118.201.84  Tailscale                   <- CGNAT, useless to a phone
/// 127.0.0.1       Loopback
/// ```
///
/// Handing all of those to mDNS produced a host that phones could *discover* but
/// not *connect to*, because the resolver surfaced a link-local address. So:
///
/// * **link-local** (`169.254.0.0/16`) means "I failed to get a DHCP lease" and
///   is never the right answer;
/// * **CGNAT** (`100.64.0.0/10`) covers Tailscale and carrier-grade NAT, which a
///   phone on the Wi-Fi has no route to;
/// * loopback and the unspecified address are obviously wrong.
///
/// Private ranges (`192.168.x`, `10.x`, `172.16-31.x`) are deliberately allowed:
/// those are what a home LAN actually uses.
pub fn is_lan_reachable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_unspecified()
                && !v4.is_link_local()
                && !is_cgnat(v4)
        }
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified(),
    }
}

/// True for the shared address space of RFC 6598, `100.64.0.0/10`.
///
/// This is where Tailscale and most carrier-grade NAT live.
fn is_cgnat(v4: &Ipv4Addr) -> bool {
    let octets = v4.octets();
    octets[0] == 100 && (64..128).contains(&octets[1])
}

/// Convenience for the common case of wanting one address or none.
pub fn primary_ipv4() -> Option<Ipv4Addr> {
    local_ipv4_addresses().into_iter().find_map(|ip| match ip {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("test address must parse")
    }

    #[test]
    fn never_returns_an_address_a_phone_could_not_reach() {
        // Environment-dependent by nature, so assert the invariant rather than a
        // specific value: whatever it finds must be genuinely reachable, and an
        // empty result is valid on a machine with no network.
        for addr in local_ipv4_addresses() {
            assert!(is_lan_reachable(&addr), "returned an unreachable address: {addr}");
        }
    }

    #[test]
    fn obvious_non_addresses_are_rejected() {
        assert!(!is_lan_reachable(&ip("127.0.0.1")));
        assert!(!is_lan_reachable(&ip("0.0.0.0")));
        assert!(!is_lan_reachable(&ip("::1")));
    }

    #[test]
    fn link_local_is_rejected() {
        // Each of these was actually present on the development machine.
        assert!(!is_lan_reachable(&ip("169.254.42.130")));
        assert!(!is_lan_reachable(&ip("169.254.62.125")));
        assert!(!is_lan_reachable(&ip("169.254.195.79")));
    }

    #[test]
    fn tailscale_cgnat_is_rejected() {
        // A phone on the Wi-Fi has no route to a Tailscale address, so
        // advertising one produces "found it but cannot connect".
        assert!(!is_lan_reachable(&ip("100.118.201.84")));
        assert!(!is_lan_reachable(&ip("100.64.0.1")));
        assert!(!is_lan_reachable(&ip("100.127.255.254")));
    }

    #[test]
    fn addresses_just_outside_cgnat_are_kept() {
        assert!(is_lan_reachable(&ip("100.63.255.255")));
        assert!(is_lan_reachable(&ip("100.128.0.1")));
    }

    #[test]
    fn real_lan_addresses_are_kept() {
        for text in ["192.168.0.24", "10.0.0.5", "172.16.4.9", "192.168.137.1"] {
            assert!(is_lan_reachable(&ip(text)), "{text} should be usable");
        }
    }

    #[test]
    fn primary_ipv4_agrees_with_the_full_list() {
        let direct = local_ipv4_addresses().into_iter().find_map(|a| match a {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        });
        assert_eq!(primary_ipv4(), direct);
    }
}

