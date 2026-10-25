//! `talkee discover` — find Talkee hosts on this network.
//!
//! This is the query side of zero-configuration discovery. In the shipping app
//! the *phone* does this (via Android's `NsdManager`), so this command exists to
//! verify the advertisement works, and to answer "why can't my phone see my PC"
//! without a phone in hand.

use anyhow::Result;
use clap::Args;
use std::time::Duration;
use talkee_discovery as discovery;

/// Arguments for `talkee discover`.
#[derive(Args, Debug)]
pub struct DiscoverArgs {
    /// How long to listen, in seconds.
    #[arg(long, default_value_t = 5)]
    pub seconds: u64,
}

/// Browses for hosts and prints what it finds.
pub fn run(args: DiscoverArgs) -> Result<()> {
    let seconds = args.seconds.max(1);
    println!(
        "Talkee {} — looking for hosts for {seconds} s (mDNS service {})",
        env!("CARGO_PKG_VERSION"),
        discovery::SERVICE_TYPE
    );

    let hosts = discovery::discover(Duration::from_secs(seconds))?;
    if hosts.is_empty() {
        println!("\nNo hosts found.");
        println!("\nIf `talkee host` is running on this network, check in this order:");
        println!("  1. Both devices are on the same subnet (not guest Wi-Fi / VPN).");
        println!("  2. The firewall allows inbound UDP 5353 (mDNS) and UDP 47801 (audio).");
        println!("  3. The host reported no error on startup about advertising.");
        return Ok(());
    }

    println!("\nFound {} host(s):", hosts.len());
    for host in &hosts {
        println!("\n  {}", host.instance);
        println!("    hostname : {}", host.host);
        match host.best_address() {
            Some(ip) => println!("    audio    : {ip}:{}", host.audio_port),
            None => println!("    audio    : (no reachable address — link-local only)"),
        }
        println!("    control  : :{}", host.control_port);
        match host.protocol_version {
            Some(version) => println!("    protocol : v{version}"),
            None => println!("    protocol : (not advertised)"),
        }
    }
    Ok(())
}
