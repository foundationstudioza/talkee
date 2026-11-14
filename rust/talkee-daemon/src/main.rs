//! `talkeed`: headless Talkee for a Linux server (or any machine without a screen).
//!
//! * `talkeed pair`     print a one-time pairing link (and a QR code) for another device to use
//! * `talkeed run`      answer paired devices on TCP 47810 (what the systemd service runs)
//! * `talkeed peers`    list paired devices
//! * `talkeed forget`   unpair a device; its key stops working at once
//! * `talkeed rename`   change the name other devices see
//! * `talkeed service`  install and start the systemd user service
//!
//! State lives in `~/.config/talkee` (`device.json`, and `mesh.dat` with mode 0600: it holds keys).
#![forbid(unsafe_code)]

use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use talkee_mesh::node::{self, Handler, Node};
use talkee_mesh::{Device, Kind, Peer, Peers, MESH_PORT};
use talkee_store::Store;

#[derive(Parser)]
#[command(name = "talkeed", version, about = "Headless Talkee for servers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Answer paired devices.
    Run {
        #[arg(long, default_value_t = MESH_PORT)]
        port: u16,
    },
    /// Print a one-time pairing link, valid for 15 minutes.
    Pair {
        /// The address other devices reach this one on (default: this machine's LAN address).
        #[arg(long)]
        host: Option<String>,
        #[arg(long, default_value_t = MESH_PORT)]
        port: u16,
        /// Skip the QR code.
        #[arg(long)]
        no_qr: bool,
    },
    /// List paired devices.
    Peers,
    /// Unpair a device (by name or id).
    Forget { who: String },
    /// Change the name other devices see.
    Rename { name: String },
    /// Install and start the systemd user service.
    Service,
}

fn main() -> Result<()> {
    let store = Store::open()?;
    let device = Device::load_or_create(&store, Kind::Server, &[])?;
    match Cli::parse().command {
        Command::Run { port } => run(store, device, port),
        Command::Pair { host, port, no_qr } => pair(store, device, host, port, no_qr),
        Command::Peers => peers(&store),
        Command::Forget { who } => forget(&store, &who),
        Command::Rename { name } => rename(&store, device, &name),
        Command::Service => service(),
    }
}

fn run(store: Store, device: Device, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .with_context(|| format!("listening on port {port}"))?;
    println!(
        "talkeed {}: {} ({}) listening on {port}",
        env!("CARGO_PKG_VERSION"),
        device.name,
        &device.id[..8]
    );
    // Let paired devices find this one again after its address changes. Best effort: a machine
    // with no LAN address, or no multicast, still serves on the saved address.
    let _advert = match talkee_discovery::mesh::advertise_mesh(
        &device.id,
        &device.name,
        "server",
        talkee_mesh::MESH_VERSION,
        port,
    ) {
        Ok(a) => {
            println!("advertising {}", a.fullname());
            Some(a)
        }
        Err(e) => {
            eprintln!("mDNS advertising is off: {e:#}");
            None
        }
    };
    let node = Arc::new(Node::new(store, device)?);
    node::serve(node, listener, Arc::new(Server))
}

/// Operations a server adds.
struct Server;

impl Handler for Server {
    fn handle(&self, _peer: &Peer, op: &str, _body: &Value) -> Result<Option<Value>> {
        Ok(match op {
            "system.status" => Some(status()),
            _ => None,
        })
    }
}

/// Load, memory and uptime, read from /proc. Fields are absent where the platform has no /proc.
fn status() -> Value {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let load: Vec<f64> = read("/proc/loadavg")
        .split_whitespace()
        .take(3)
        .filter_map(|v| v.parse().ok())
        .collect();
    let uptime = read("/proc/uptime")
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v as u64);
    let mem = read("/proc/meminfo");
    let kib = |key: &str| {
        mem.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    json!({
        "load": load,
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "uptime_secs": uptime,
        "mem_total_mib": kib("MemTotal:").map(|k| k / 1024),
        "mem_available_mib": kib("MemAvailable:").map(|k| k / 1024),
        "os": std::env::consts::OS,
    })
}

fn pair(store: Store, device: Device, host: Option<String>, port: u16, no_qr: bool) -> Result<()> {
    let host = match host {
        Some(h) => h,
        None => lan_address().context("could not work out this machine's address; pass --host")?,
    };
    let node = Node::new(store, device)?;
    let link = node.invite_link(&host, port)?;
    if !no_qr {
        let code = qrcode::QrCode::new(link.as_bytes())?;
        let art = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{art}");
    }
    println!("On your PC, run:\n\n  talkee mesh join \"{link}\"\n");
    println!("The link works once, for 15 minutes. Anyone who sees it in that time can pair.");
    println!("`talkeed run` (or the service) must be running for the other device to connect.");
    Ok(())
}

/// The address this machine uses to reach the network: the source address of a route outwards.
/// Nothing is sent; connecting a UDP socket only picks the route.
fn lan_address() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

fn peers(store: &Store) -> Result<()> {
    let peers = Peers::load(store)?;
    if peers.peers.is_empty() {
        println!("No paired devices. Run `talkeed pair`.");
    }
    for p in &peers.peers {
        println!("{:<24} {:<7} {}  last seen {}", p.name, format!("{:?}", p.kind).to_lowercase(), &p.id[..8], ago(p.last_seen));
    }
    let pending = peers.pending_invites();
    if pending > 0 {
        println!("{pending} unused invite(s) pending.");
    }
    Ok(())
}

fn ago(t: u64) -> String {
    let d = talkee_mesh::peers::now().saturating_sub(t);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86399 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86400),
    }
}

fn forget(store: &Store, who: &str) -> Result<()> {
    let mut peers = Peers::load(store)?;
    let Some(peer) = peers.get(who).cloned() else { bail!("no paired device called {who}") };
    peers.forget(&peer.id);
    peers.save(store)?;
    println!("Forgot {}. Its key no longer works here.", peer.name);
    Ok(())
}

fn rename(store: &Store, mut device: Device, name: &str) -> Result<()> {
    let name = talkee_mesh::identity::clean_name(name);
    if name.is_empty() {
        bail!("that name is empty once cleaned");
    }
    device.name = name;
    device.save(store)?;
    println!("Renamed to {}. Restart the service for peers to see it.", device.name);
    Ok(())
}

fn service() -> Result<()> {
    let exe = std::env::current_exe()?;
    let home = std::env::var("HOME").context("no HOME")?;
    let dir = std::path::Path::new(&home).join(".config/systemd/user");
    std::fs::create_dir_all(&dir)?;
    let unit = format!(
        "[Unit]\nDescription=Talkee headless node\nAfter=network-online.target\n\n\
         [Service]\nExecStart={} run\nRestart=on-failure\nRestartSec=3\n\n\
         [Install]\nWantedBy=default.target\n",
        exe.display()
    );
    std::fs::write(dir.join("talkeed.service"), unit)?;
    for args in [&["daemon-reload"][..], &["enable", "--now", "talkeed.service"], &["restart", "talkeed.service"]] {
        let ok = std::process::Command::new("systemctl").arg("--user").args(args).status()?.success();
        if !ok {
            bail!("systemctl --user {} failed", args.join(" "));
        }
    }
    println!("talkeed is running as a user service. Logs: journalctl --user -u talkeed -f");
    Ok(())
}
