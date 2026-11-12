//! Recording from this computer's default microphone, for dictation.
//!
//! Opened when the hotkey goes down and closed when it comes up, so the microphone is in use only
//! while a dictation records: Windows shows its microphone indicator for exactly that long.

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{InputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use std::sync::{Arc, Mutex};

/// An open recording.
pub struct Capture {
    // Held for its Drop: dropping the stream stops the device.
    _stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<f32>>>,
    rate: u32,
}

impl Capture {
    /// Opens the default input device and starts recording mono samples.
    pub fn open() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("this computer has no microphone set as default")?;
        let supported = device
            .default_input_config()
            .context("asking the microphone for its format")?;
        let format = supported.sample_format();
        let channels = supported.channels();
        let rate = supported.sample_rate();
        let config: StreamConfig = supported.into();
        let buffer = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 30)));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, channels, Arc::clone(&buffer)),
            SampleFormat::I16 => build::<i16>(&device, config, channels, Arc::clone(&buffer)),
            SampleFormat::U16 => build::<u16>(&device, config, channels, Arc::clone(&buffer)),
            SampleFormat::I32 => build::<i32>(&device, config, channels, Arc::clone(&buffer)),
            other => bail!("the microphone uses sample format {other:?}, which Talkee cannot read"),
        }?;
        stream.play().context("starting the microphone")?;
        Ok(Capture {
            _stream: stream,
            buffer,
            rate,
        })
    }

    /// Stops recording and returns the mono samples and their rate.
    pub fn finish(self) -> (Vec<f32>, u32) {
        let rate = self.rate;
        let buffer = Arc::clone(&self.buffer);
        drop(self);
        let samples = std::mem::take(&mut *buffer.lock().unwrap_or_else(|p| p.into_inner()));
        (samples, rate)
    }
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    channels: u16,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = channels.max(1) as usize;
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _info: &InputCallbackInfo| {
            if let Ok(mut out) = buffer.lock() {
                for frame in data.chunks_exact(channels) {
                    let sum: f32 = frame.iter().map(|s| s.to_sample::<f32>()).sum();
                    out.push(sum / channels as f32);
                }
            }
        },
        move |error| eprintln!("dictation: microphone error: {error}"),
        None,
    )?)
}
