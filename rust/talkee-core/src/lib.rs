//! Talkee audio pipeline.
//!
//! Everything here is **platform-free and UI-free**. All device-specific I/O
//! lives behind [`VirtualMicWriter`], which `talkee-vmic` implements per
//! platform. That boundary is what lets us run this whole pipeline in CI
//! against a PulseAudio null sink instead of a real virtual cable.
//!
//! # M0 data path
//!
//! ```text
//! UDP socket (own thread, not real-time critical)
//!   -> parse header            (talkee-proto)
//!   -> decode to mono f32      (talkee-proto)
//!   -> reorder / drop late     (stream::Receiver)
//!   -> resample to device rate (resample::LinearResampler)
//!   -> push into SharedQueue   (queue::SharedQueue)
//!
//! audio callback (real-time critical)
//!   -> try_lock SharedQueue, copy, pad with silence on underrun
//! ```
//!
//! The real-time callback never allocates, never locks with a blocking lock,
//! and never does sample-rate conversion. Conversion happens on the receive
//! thread, which is allowed to be slow.

#![forbid(unsafe_code)]

pub mod codec;
pub mod control;
pub mod echo;
pub mod controller;
pub mod jitter;
pub mod level;
pub mod live;
pub mod local;
pub mod meter;
pub mod queue;
pub mod resample;
pub mod stats;
pub mod stream;
pub mod traits;

pub use controller::{Correction, DepthController};
pub use level::AutoLevel;
pub use local::{is_lan_reachable, local_ipv4_addresses, primary_ipv4};
pub use meter::{ArrivalWindow, InputRate};
pub use queue::{FillOutcome, SharedQueue};
pub use stats::{Stats, StatsSnapshot};
pub use stream::{Receiver, ReceiverConfig};
pub use traits::VirtualMicWriter;
