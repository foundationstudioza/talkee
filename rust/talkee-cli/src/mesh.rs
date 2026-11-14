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
            let (_, mut client) = node::connect_peer(&store, &who)?;
            let started = std::time::Instant::now();
            let answer = client.call(&op, body)?;
            println!("{}", serde_json::to_string_pretty(&answer)?);
            eprintln!("({} ms)", started.elapsed().as_millis());
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
