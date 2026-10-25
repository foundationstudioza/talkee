//! The boundary between the portable audio pipeline and the OS.
//!
//! `talkee-core` knows nothing about WASAPI, CoreAudio or PipeWire. It only
//! knows how to hand mono `f32` frames to something that implements
//! [`VirtualMicWriter`]. Everything platform-specific lives in `talkee-vmic`.
//!
//! That split is what lets the whole pipeline run in CI on a Linux runner
//! against a PulseAudio null sink, instead of needing a real virtual cable.

/// An output endpoint that the rest of the operating system can read as a
/// microphone.
///
/// Implementations are expected to hold an open stream. Dropping the value must
/// close the stream and release the device.
///
/// Deliberately **not** `Send`: cpal's `Stream` is not `Send` on every backend,
/// so requiring it here would make the trait unimplementable on some platforms.
/// Callers keep the writer on a single thread.
pub trait VirtualMicWriter {
    /// Human-readable name of the endpoint that was opened, for the UI and logs.
    fn endpoint_name(&self) -> String;

    /// Sample rate the endpoint actually runs at, in Hz.
    ///
    /// This is the rate the [`crate::queue::SharedQueue`] must be filled at, and
    /// it is frequently *not* what the user configured in their OS sound
    /// settings — hence reporting it rather than assuming.
    fn sample_rate(&self) -> u32;

    /// Channel count the endpoint actually runs at.
    fn channels(&self) -> u16;

    /// Latency the endpoint adds internally, if the platform can tell us.
    ///
    /// Used for the latency budget in `docs/LATENCY.md`. `None` means "unknown",
    /// which must not be confused with zero.
    fn reported_latency_ms(&self) -> Option<f32> {
        None
    }
}
