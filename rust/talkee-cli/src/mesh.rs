//! `talkee mesh`: pair this PC with other devices (a server running `talkeed`) and talk to them.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::Value;
use talkee_mesh::{node, Capability, Device, Kind, Peers};
use talkee_store::Store;

#[derive(Args)]
pub struct MeshArgs {
    #[command(subcommand)]
    command: MeshCommand,
}

#[derive(Subcommand)]
enum MeshCommand {
    /// Use a pairing link printed by `talkeed pair` on the other device.
    Join { link: String },
    /// List paired devices.
    Peers,
    /// Send a request to a paired device, e.g. `talkee mesh call compaq system.status`.
    Call {
        who: String,
        op: String,
        /// JSON body.
        #[arg(default_value = "null")]
        body: String,
    },
    /// Set the address a paired device is dialled on, e.g. a Tailscale name or fixed IP.
    SetAddr {
        who: String,
        /// `host` or `host:port` (port defaults to 47810).
        addr: String,
    },
    /// Look for mesh devices on the local network (mDNS) for a few seconds.
    Discover {
        #[arg(long, default_value_t = 3)]
        secs: u64,
    },
    /// Unpair a device on this side.
    Forget { who: String },
}

pub fn run(args: MeshArgs) -> Result<()> {
    let store = Store::open()?;
    let me = Device::load_or_create(
        &store,
        Kind::Pc,
        &[Capability::Mic, Capability::Dictate, Capability::TypeAtCursor],
    )?;
    match args.command {
        MeshCommand::Join { link } => {
            let peer = node::join(&store, &me, &link)?;
            println!(
                "Paired with {} ({:?}) at {}.",
                peer.name,
                peer.kind,
                peer.addr.as_deref().unwrap_or("?")
            );
        }
        MeshCommand::Peers => {
            let peers = Peers::load(&store)?;
            if peers.peers.is_empty() {
                println!("No paired devices.");
            }
            for p in &peers.peers {
                println!(
                    "{:<24} {:<7} {}  {}",
                    p.name,
                    format!("{:?}", p.kind).to_lowercase(),
                    &p.id[..8],
                    p.addr.as_deref().unwrap_or("-")
                );
            }
        }
        MeshCommand::Call { who, op, body } => {
            let body: Value = serde_json::from_str(&body)?;
            let mut resolve = |id: &str, t: std::time::Duration| {
                talkee_discovery::mesh::resolve_mesh_peer(id, t)
            };
            let (_, mut client) = node::connect_peer_with(&store, &who, &mut resolve)?;
            let started = std::time::Instant::now();
            let answer = client.call(&op, body)?;
            println!("{}", serde_json::to_string_pretty(&answer)?);
            eprintln!("({} ms)", started.elapsed().as_millis());
        }
        MeshCommand::SetAddr { who, addr } => {
            let (peer, addr) = node::set_peer_addr(&store, &who, &addr)?;
            println!("{} will be dialled at {addr}.", peer.name);
        }
        MeshCommand::Discover { secs } => {
            let paired = Peers::load(&store)?;
            let found = talkee_discovery::mesh::discover_mesh(std::time::Duration::from_secs(secs))?;
            if found.is_empty() {
                println!("No mesh devices answered.");
            }
            for h in found {
                let tag = if paired.get(&h.id).is_some() { "paired" } else { "not paired" };
                println!("{:<24} {:<7} {}  {}  {}", h.name, h.kind, &h.id[..8], h.dial_addrs().join(","), tag);
            }
        }
        MeshCommand::Forget { who } => {
            let mut peers = Peers::load(&store)?;
            let Some(peer) = peers.get(&who).cloned() else {
                anyhow::bail!("no paired device called {who}")
            };
            peers.forget(&peer.id);
            peers.save(&store)?;
            println!("Forgot {}.", peer.name);
        }
    }
    Ok(())
}
