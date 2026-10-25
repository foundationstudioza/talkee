//! Talkee's desktop output: opens a render endpoint that the rest of the system
//! can read as a microphone.
//!
//! # What this actually does
//!
//! We do not create a device. On Windows there is no user-mode way to do that —
//! ACX is KMDF-only — so we play our audio into *someone else's* virtual cable,
//! and the user selects that cable's capture endpoint as their microphone.
//! `talkee probe` prints exactly which endpoint to select.
//!
//! # Why cpal for M0
//!
//! Raw WASAPI would let us match the device mix format exactly and opt into
//! `IAudioClient3` low-latency shared streams, worth several milliseconds. But
//! the M0 question is "does the pipeline work at all", not "is it minimal
//! latency" — and cpal also covers CoreAudio and PipeWire, so we get three
//! platforms from one implementation. M1 replaces the Windows path with a
//! hand-written WASAPI backend behind the same [`VirtualMicWriter`] trait.

#![forbid(unsafe_code)]

pub mod cable;

// Re-exported so `talkee_vmic::cable_state()` reads as the status API it is,
// while `talkee_vmic::cable::*` stays the module that owns the licensing data.
pub use cable::{cable_state_from, CableState, InstallPlan};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, OutputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use std::sync::Arc;
use talkee_core::{SharedQueue, VirtualMicWriter};

/// How many frames the audio callback can serve in one go.
///
/// Pre-allocated once so the real-time callback never allocates, and generous
/// enough that no realistic buffer size hits the ceiling. If a device ever asks
/// for more, the tail of its buffer stays silent rather than stuttering.
const CALLBACK_SCRATCH_FRAMES: usize = 16_384;

/// Which way audio flows through an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Something we can write into (speakers, headphones, a cable's input side).
    Render,
    /// Something that can be read (a mic, a cable's output side).
    Capture,
}

impl DeviceKind {
    /// Single-word label for printing.
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceKind::Render => "render",
            DeviceKind::Capture => "capture",
        }
    }
}

/// One audio endpoint as discovered on this machine.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Friendly name, e.g. `CABLE Input (VB-Audio Virtual Cable)`.
    pub name: String,
    /// Direction.
    pub kind: DeviceKind,
    /// True if this looks like a virtual device rather than hardware.
    pub is_virtual: bool,
    /// Curated family id, if we recognise the exact product.
    pub family: Option<&'static str>,
    /// Curated family label, if we recognise the exact product.
    pub family_label: Option<&'static str>,
    /// True if this is the OS default for its direction.
    pub is_default: bool,
}

/// Lists every audio endpoint cpal can see, render and capture.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let default_render = host.default_output_device();
    let default_capture = host.default_input_device();

    let mut out = Vec::new();
    for device in host.output_devices().context("enumerating output devices")? {
        if let Some(info) = describe(&device, DeviceKind::Render, default_render.as_ref()) {
            out.push(info);
        }
    }
    for device in host.input_devices().context("enumerating input devices")? {
        if let Some(info) = describe(&device, DeviceKind::Capture, default_capture.as_ref()) {
            out.push(info);
        }
    }
    Ok(out)
}

/// Builds a [`DeviceInfo`], or `None` if the backend will not describe it.
fn describe(
    device: &cpal::Device,
    kind: DeviceKind,
    default: Option<&cpal::Device>,
) -> Option<DeviceInfo> {
    let name = device.description().ok()?.name().to_string();
    let family = cable::detect_render(&name);
    Some(DeviceInfo {
        is_virtual: family.is_some() || cable::looks_virtual(&name),
        family: family.map(|f| f.id),
        family_label: family.map(|f| f.label),
        name,
        kind,
        is_default: default == Some(device),
    })
}

/// Finds a render endpoint we recognise as a usable virtual cable.
///
/// Returns the endpoint's name and its curated family. It returns a *name*
/// rather than a `cpal::Device` because [`open_output`] resolves devices by
/// name anyway, and this keeps callers off cpal entirely.
pub fn find_virtual_cable() -> Result<Option<(String, &'static cable::CableFamily)>> {
    let host = cpal::default_host();
    for device in host.output_devices().context("enumerating output devices")? {
        let Ok(description) = device.description() else {
            continue;
        };
        let name = description.name();
        if let Some(family) = cable::detect_render(name) {
            return Ok(Some((name.to_string(), family)));
        }
    }
    Ok(None)
}

/// Reports what this machine can give us to play into, and what to do if nothing can.
///
/// # The status API
///
/// `talkee-vmic` answers the four questions a UI has to ask, in two calls:
///
/// * which family is installed, and may we redistribute it — [`CableState`] and
///   [`CableState::may_redistribute`];
/// * what we would do about it — [`CableState::install_plan`], whose only
///   installable answer is VB-CABLE, because that is the only product we may
///   ship (asserted by a test in [`cable`]);
/// * what the user has to read — [`CableState::next_steps`], shared with
///   `talkee probe` and `talkee cable status` so the advice cannot differ.
///
/// [`find_virtual_cable`] is unchanged and remains the way to get the endpoint
/// *name* to open. Both go through [`cable::detect_render`] over the same
/// enumeration order, so they cannot disagree about which cable `host` picks.
pub fn cable_state() -> Result<CableState> {
    Ok(cable::cable_state_from(&list_devices()?))
}

/// A render endpoint's real format, read *before* we open a stream on it.
///
/// This exists to break a chicken-and-egg problem: the output queue has to be
/// sized against the device's actual sample rate, but the queue has to exist
/// before the stream that tells us the rate can be built.
#[derive(Debug, Clone)]
pub struct EndpointFormat {
    /// Friendly endpoint name.
    pub name: String,
    /// Sample rate the endpoint runs at, in Hz.
    pub sample_rate: u32,
    /// Channel count the endpoint runs at.
    pub channels: u16,
    /// Debug-printable sample format, e.g. `F32`.
    pub sample_format: String,
}

/// Reads the default output format of the endpoint `query` resolves to.
pub fn inspect_render(query: &str) -> Result<EndpointFormat> {
    let host = cpal::default_host();
    let device = find_render_device(&host, query)?;
    let name = device
        .description()
        .context("describing the chosen device")?
        .name()
        .to_string();
    let supported = device
        .default_output_config()
        .with_context(|| format!("querying the default output format of {name}"))?;
    Ok(EndpointFormat {
        sample_format: format!("{:?}", supported.sample_format()),
        sample_rate: supported.sample_rate(),
        channels: supported.channels(),
        name,
    })
}

/// Resolves a user-supplied device query to a render endpoint.
///
/// Tries an exact case-insensitive name match first, then a substring match.
/// Substring matters because Windows friendly names carry a vendor suffix:
/// nobody wants to type `CABLE Input (VB-Audio Virtual Cable)`.
pub fn find_render_device(host: &cpal::Host, query: &str) -> Result<cpal::Device> {
    let wanted = query.trim();
    if wanted.is_empty() {
        return Err(anyhow!("device name is empty"));
    }
    let needle = wanted.to_ascii_lowercase();
    let mut substring_match = None;
    let mut available = Vec::new();

    for device in host.output_devices().context("enumerating output devices")? {
        let Ok(description) = device.description() else {
            continue;
        };
        let name = description.name().to_string();
        if name.eq_ignore_ascii_case(wanted) {
            return Ok(device);
        }
        if substring_match.is_none() && name.to_ascii_lowercase().contains(&needle) {
            substring_match = Some(device);
        }
        available.push(name);
    }

    if let Some(device) = substring_match {
        return Ok(device);
    }
    Err(anyhow!(
        "no output device matches {wanted:?}. Available: {}",
        if available.is_empty() {
            "<none>".to_string()
        } else {
            available.join(" | ")
        }
    ))
}

/// A render endpoint we have opened, with a live stream pulling from a queue.
struct CpalWriter {
    /// Kept alive purely for its side effect: dropping the stream closes the
    /// device. Hence the underscore.
    _stream: cpal::Stream,
    endpoint: String,
    sample_rate: u32,
    channels: u16,
}

impl VirtualMicWriter for CpalWriter {
    fn endpoint_name(&self) -> String {
        self.endpoint.clone()
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

/// Opens a render endpoint and starts feeding it from `queue`.
///
/// `query` is matched with [`find_render_device`]. `buffer_frames` is a request,
/// not a guarantee: if the backend rejects a fixed buffer size we retry with the
/// device's own default rather than failing to start, because a stream that runs
/// at a slightly higher latency is worth more than one that refuses to open.
///
/// The returned writer borrows the queue for the lifetime of the stream and must
/// stay alive: dropping it silences the microphone.
pub fn open_output(
    query: &str,
    queue: Arc<SharedQueue>,
    buffer_frames: Option<u32>,
) -> Result<Box<dyn VirtualMicWriter>> {
    let host = cpal::default_host();
    let device = find_render_device(&host, query)?;
    let name = device
        .description()
        .context("describing the chosen device")?
        .name()
        .to_string();

    let supported = device
        .default_output_config()
        .with_context(|| format!("querying the default output format of {name}"))?;
    let sample_format = supported.sample_format();
    let channels = supported.channels();
    let sample_rate = supported.sample_rate();

    // `into()` gives us the device's real mix format, so the audio engine inside
    // WASAPI has nothing to convert. Feeding it a format it has to resample would
    // add an unknown, and unknowable, amount of latency.
    let mut config: StreamConfig = supported.into();
    if let Some(frames) = buffer_frames {
        config.buffer_size = BufferSize::Fixed(frames);
    }

    let stream = match build_stream(&device, &config, sample_format, channels, Arc::clone(&queue)) {
        Ok(stream) => stream,
        Err(first_error) if buffer_frames.is_some() => {
            eprintln!(
                "talkee: {name} rejected a fixed buffer of {buffer_frames:?} frames ({first_error}); \
                 retrying with the device default"
            );
            config.buffer_size = BufferSize::Default;
            build_stream(&device, &config, sample_format, channels, Arc::clone(&queue))
                .with_context(|| format!("opening {name}"))?
        }
        Err(e) => return Err(anyhow!("opening {name} failed: {e}")),
    };

    stream.play().context("starting the output stream")?;

    Ok(Box::new(CpalWriter {
        _stream: stream,
        endpoint: name,
        sample_rate,
        channels,
    }))
}

/// Dispatches to a monomorphised stream builder for the device's real sample
/// format.
///
/// Running one match is how we avoid a runtime conversion loop in the callback:
/// the sample-type conversion is resolved at compile time, once per format.
fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    channels: u16,
    queue: Arc<SharedQueue>,
) -> Result<cpal::Stream> {
    macro_rules! dispatch {
        ($($variant:ident => $ty:ty),* $(,)?) => {
            match sample_format {
                $(SampleFormat::$variant => build_typed::<$ty>(device, config, channels, queue),)*
                other => Err(anyhow!("unsupported output sample format {other:?}")),
            }
        };
    }
    dispatch!(
        I8 => i8,
        I16 => i16,
        I32 => i32,
        I64 => i64,
        U8 => u8,
        U16 => u16,
        U32 => u32,
        U64 => u64,
        F32 => f32,
        F64 => f64,
    )
}

/// Peak absolute sample of a block, `0.0` for an empty or silent one.
///
/// # Why the meter gets the peak and not an average
///
/// A level meter answers "is it too quiet, or clipping?" — both are questions
/// about extremes. An average of a loud burst and its silence reads as "fine",
/// which is the one answer that is certainly wrong.
///
/// Non-finite samples are skipped rather than propagated: a single `NaN` that
/// reached the meter would make it read `NaN` for the rest of the session, and
/// `NaN` compares false against everything, so it cannot even become the peak —
/// the `>` below is what makes skipping free.
fn peak_abs(samples: &[f32]) -> f32 {
    let mut peak = 0.0f32;
    for &sample in samples {
        let magnitude = sample.abs();
        if magnitude.is_finite() && magnitude > peak {
            peak = magnitude;
        }
    }
    peak
}

/// Builds the output stream for one concrete sample type.
///
/// # This is the real-time path
///
/// The closure below runs on the audio thread and holds a hard deadline. It
/// therefore:
///
/// * never allocates — `scratch` is allocated before the stream starts;
/// * never takes a blocking lock — `SharedQueue::fill` uses `try_lock`;
/// * never converts sample rates — that happened on the receive thread.
///
/// A mono frame is duplicated across every output channel, because we are
/// feeding a stereo-or-wider device from a mono microphone.
fn build_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: u16,
    queue: Arc<SharedQueue>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channel_count = channels.max(1) as usize;
    let mut scratch = vec![0.0f32; CALLBACK_SCRATCH_FRAMES];

    let stream = device.build_output_stream(
        *config,
        move |out: &mut [T], _info: &OutputCallbackInfo| {
            // cpal pre-fills `out` with silence, so anything we do not write is
            // already correct. That also covers the case of a device asking for
            // more frames than our scratch buffer holds.
            let frames = (out.len() / channel_count).min(scratch.len());
            queue.fill(&mut scratch[..frames]);

            // The desktop app's output meter, which is how a user answers "can
            // they hear me?" without reading a log. The queue already carries the
            // same `Arc<Stats>` the app holds — `SharedQueue::new` is the only way
            // to build one — so this needs no extra parameter and no extra state,
            // and `level_in` vs `level_out` stay distinguishable when audio is
            // arriving but not leaving.
            queue.stats().set_output_level(peak_abs(&scratch[..frames]));

            for (index, frame) in out.chunks_mut(channel_count).enumerate() {
                let value: T = T::from_sample(scratch.get(index).copied().unwrap_or(0.0));
                for slot in frame.iter_mut() {
                    *slot = value;
                }
            }
        },
        move |error| eprintln!("talkee: output stream error: {error}"),
        None,
    )?;

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use talkee_core::Stats;

    #[test]
    fn an_empty_block_has_no_peak() {
        assert_eq!(peak_abs(&[]), 0.0);
        assert_eq!(peak_abs(&[0.0; 64]), 0.0);
    }

    #[test]
    fn the_peak_is_absolute_so_both_polarities_count() {
        assert_eq!(peak_abs(&[0.25, -0.5, 0.125]), 0.5);
        assert_eq!(peak_abs(&[-1.0, 0.5]), 1.0);
        // Above full scale is reported as-is: clipping is information the user
        // needs, and clamping it would hide the one number that explains a
        // distorted microphone.
        assert_eq!(peak_abs(&[1.75]), 1.75);
    }

    #[test]
    fn samples_that_are_not_numbers_never_become_the_peak() {
        // A NaN that reached the meter would leave it reading NaN forever, and
        // the desktop app would have no way to tell that from a real reading.
        assert!(peak_abs(&[f32::NAN, 0.5, f32::NAN]).is_finite());
        assert_eq!(peak_abs(&[f32::NAN, 0.5]), 0.5);
        assert_eq!(peak_abs(&[f32::INFINITY, 0.25]), 0.25);
        assert_eq!(peak_abs(&[f32::NEG_INFINITY]), 0.0);
    }

    /// End-to-end proof that the audio callback publishes the level it wrote.
    ///
    /// Ignored by default, because it needs a real virtual cable and therefore
    /// cannot run in CI: `cargo test --workspace` must stay clean on a machine
    /// with no cable installed. Run it by hand on one that has:
    ///
    /// ```text
    /// cargo test -p talkee-vmic -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a virtual cable installed on this machine"]
    fn the_audio_callback_publishes_the_peak_it_wrote() {
        // Quiet enough not to be unpleasant if something is listening to the
        // cable's other end while this runs.
        const AMPLITUDE: f32 = 0.02;

        let found = find_virtual_cable().expect("enumerating output devices");
        let Some((endpoint, _family)) = found else {
            // No cable, nothing to prove, and not a reason to fail a hand-run.
            eprintln!("talkee: no virtual cable on this machine; skipping");
            return;
        };

        let stats = Arc::new(Stats::new());
        let queue = Arc::new(SharedQueue::new(96_000, Arc::clone(&stats)));
        let writer = open_output(&endpoint, Arc::clone(&queue), None).expect("opening the cable");

        // Half a second of tone, which outlasts the sleep below at any device
        // rate we are likely to meet, so the callback cannot run dry and report
        // the silence it pads with instead of the audio we sent.
        queue.push(&[AMPLITUDE; 48_000]);
        std::thread::sleep(Duration::from_millis(250));

        assert_eq!(
            stats.output_level(),
            AMPLITUDE,
            "the callback must publish the peak of what it wrote, not `level_in`"
        );
        // Keep the writer alive until after the assertion: dropping it closes the
        // stream, and a closed stream writes nothing.
        drop(writer);
    }
}




/// A live capture of what the PC is playing. Dropping it stops the capture.
pub struct Loopback {
    _stream: cpal::Stream,
    /// The endpoint being listened to, for the log.
    pub endpoint: String,
    /// Rate the samples are delivered at. Not resampled here: the caller owns the pipeline's rate.
    pub sample_rate: u32,
}

/// Captures the mix going to the default speakers, as mono, and hands it to `on_samples`.
///
/// This is the *reference* for echo cancellation: the one thing the PC knows about the sound the
/// phone's microphone is picking up. On Windows, opening a render endpoint for input is WASAPI
/// loopback.
///
/// Refuses a virtual cable as the default output. If the default output *is* the cable, the
/// reference would be the phone's own voice, and cancelling it would remove the user from their own
/// microphone — worse than the echo.
///
/// `on_samples` runs on the audio thread: it must not block or allocate.
pub fn open_loopback<F>(mut on_samples: F) -> Result<Loopback>
where
    F: FnMut(&[f32], u32) + Send + 'static,
{
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("this machine has no default speakers to listen to"))?;
    let name = device
        .description()
        .context("describing the default speakers")?
        .name()
        .to_string();
    if cable::detect_render(&name).is_some() || cable::looks_virtual(&name) {
        return Err(anyhow!(
            "the default output is a virtual device ({name}); echo cancellation needs your real \
             speakers as the default so it can hear what they play"
        ));
    }

    let supported = device
        .default_output_config()
        .with_context(|| format!("querying the format of {name}"))?;
    if supported.sample_format() != SampleFormat::F32 {
        return Err(anyhow!(
            "{name} mixes in {:?}, and the loopback capture reads f32",
            supported.sample_format()
        ));
    }
    let channels = supported.channels().max(1) as usize;
    let sample_rate = supported.sample_rate();
    let config: StreamConfig = supported.into();

    let mut mono: Vec<f32> = Vec::with_capacity(4096);
    let stream = device
        .build_input_stream::<f32, _, _>(
            config,
            move |data: &[f32], _| {
                mono.clear();
                for frame in data.chunks_exact(channels) {
                    mono.push(frame.iter().sum::<f32>() / channels as f32);
                }
                on_samples(&mono, sample_rate);
            },
            |e| eprintln!("talkee: loopback capture error: {e}"),
            None,
        )
        .with_context(|| format!("capturing what {name} plays"))?;
    stream.play().context("starting the loopback capture")?;
    Ok(Loopback {
        _stream: stream,
        endpoint: name,
        sample_rate,
    })
}
