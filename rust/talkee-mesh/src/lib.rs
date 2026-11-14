//! The Talkee device mesh: many devices, each paired with the others it talks to.
//!
//! * [`identity`]: who this device is (a stable id, a name, a kind, what it can do).
//! * [`peers`]: the devices this one is paired with, one key per pair, and pending invites.
//! * [`wire`]: the link between two devices: a mutual challenge/response over the pair's key, then
//!   AES-256-GCM frames carrying JSON requests and responses with ids.
//! * [`node`]: serving requests ([`node::serve`]) and making them ([`node::Client`]).
//!
//! No platform code: the store decides how secrets are protected on each OS.
#![forbid(unsafe_code)]

pub mod identity;
pub mod node;
pub mod peers;
pub mod wire;

pub use identity::{Capability, Device, Kind};
pub use node::{Client, Handler, Request, Response};
pub use peers::{Invite, Peer, Peers};

/// The mesh's TCP port. Beside the microphone's 47800/47801, so one firewall rule range covers both.
pub const MESH_PORT: u16 = 47810;

/// Version of the mesh link. A peer speaking another version is refused with a clear message.
pub const MESH_VERSION: u32 = 1;
