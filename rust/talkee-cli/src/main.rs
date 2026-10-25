//! Talkee M0 command line.
//!
//! Five subcommands, which together are enough to measure real latency without
//! writing a single line of UI:
//!
//! * `probe`    — what audio endpoints exist, and is a virtual cable installed?
//! * `cable`    — the same status, plus a way to install VB-CABLE with consent
//! * `host`     — receive audio and play it into a virtual cable (the desktop side)
//! * `send`     — synthesise audio and send it (stands in for the phone)
//! * `discover` — look for hosts advertising themselves on this network

mod cable;
mod discover;
mod host;
mod probe;
mod qr;
mod send;

use clap::{Parser, Subcommand};

/// Share a phone microphone with a desktop.
#[derive(Parser)]
#[command(name = "talkee", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List audio endpoints and report whether a virtual cable is installed.
    Probe,
    /// Check for a virtual cable, and install one if there is none.
    Cable(cable::CableArgs),
    /// Receive audio and play it into a virtual cable's input.
    Host(host::HostArgs),
    /// Act as a phone: synthesise audio and send it to a host.
    Send(send::SendArgs),
    /// Look for Talkee hosts advertising themselves on this network.
    Discover(discover::DiscoverArgs),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Probe => probe::run(),
        Command::Cable(args) => cable::run(args),
        Command::Host(args) => host::run(args),
        Command::Send(args) => send::run(args),
        Command::Discover(args) => discover::run(args),
    }
}
