//! Device selection, buffer sizing, and the capture-side resampler.

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, Device, Host, SupportedStreamConfig};

use crate::SR;

/// Pick an input/output device by (case-insensitive substring) name, else the default.
pub(crate) fn pick_device(host: &Host, want: &str, input: bool) -> Result<Device> {
    if !want.is_empty() {
        let devs: Vec<Device> = if input {
            host.input_devices()?.collect()
        } else {
            host.output_devices()?.collect()
        };
        let hit = devs.into_iter().find(|d| {
            d.name().map(|n| n.to_lowercase().contains(&want.to_lowercase())).unwrap_or(false)
        });
        if let Some(d) = hit {
            return Ok(d);
        }
        // Falling back silently would send a call's audio to the wrong device without
        // the user ever knowing why, so this is an error.
        bail!("no {} device matching {want:?}", if input { "input" } else { "output" });
    }
    let d = if input { host.default_input_device() } else { host.default_output_device() };
    d.context("no default audio device")
}

/// Prefer a 48 kHz F32 config so neither side has to resample; otherwise take the
/// device default and let the caller resample.
pub(crate) fn pick_config(dev: &Device, input: bool) -> Result<SupportedStreamConfig> {
    let ranges: Vec<_> = if input {
        dev.supported_input_configs()?.collect()
    } else {
        dev.supported_output_configs()?.collect()
    };
    let native_48k = ranges.iter().find(|r| {
        r.sample_format() == cpal::SampleFormat::F32
            && r.min_sample_rate().0 <= SR
            && r.max_sample_rate().0 >= SR
    });
    if let Some(r) = native_48k {
        return Ok(r.clone().with_sample_rate(cpal::SampleRate(SR)));
    }
    let cfg = if input { dev.default_input_config()? } else { dev.default_output_config()? };
    Ok(cfg)
}

/// Ask the device for a buffer of `ms`, clamped to what it supports. `ms == 0` keeps
/// the device default.
pub(crate) fn buffer_size_for(supported: &cpal::SupportedBufferSize, ms: u32) -> BufferSize {
    if ms == 0 {
        return BufferSize::Default;
    }
    let want = (SR / 1000) * ms;
    match supported {
        cpal::SupportedBufferSize::Range { min, max } => BufferSize::Fixed(want.clamp(*min, *max)),
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
    }
}

/// Linear-interpolating resampler for the capture path (device rate -> 48 kHz).
///
/// Linear is chosen deliberately: it is a few instructions per sample inside the audio
/// callback, and the artifacts it introduces sit far above the voice band once the
/// signal is Opus-encoded. A polyphase resampler would cost latency we care about more.
pub(crate) struct Resampler {
    ratio: f64, // src samples consumed per dst sample
    pos: f64,
    /// Last frame of the previous block, so interpolation is continuous across callbacks.
    last: Vec<f32>,
    ch: usize,
}

impl Resampler {
    pub(crate) fn new(from: u32, to: u32, ch: usize) -> Self {
        Self { ratio: from as f64 / to as f64, pos: 0.0, last: vec![0.0; ch], ch }
    }

    /// Resample interleaved `src` into `out` (appended). `src` must be a whole number
    /// of `ch`-sample frames.
    pub(crate) fn process(&mut self, src: &[f32], out: &mut Vec<f32>) {
        let frames = src.len() / self.ch;
        if frames == 0 {
            return;
        }
        // `pos` is a fractional index into a virtual buffer whose frame -1 is `last`.
        while self.pos < frames as f64 {
            let i = self.pos.floor() as isize;
            let frac = (self.pos - self.pos.floor()) as f32;
            for c in 0..self.ch {
                let a = if i < 0 { self.last[c] } else { src[i as usize * self.ch + c] };
                let b = if (i + 1) < frames as isize {
                    src[(i + 1) as usize * self.ch + c]
                } else {
                    // Clamp at the block edge; the next block continues from `last`.
                    a
                };
                out.push(a + (b - a) * frac);
            }
            self.pos += self.ratio;
        }
        self.last.copy_from_slice(&src[(frames - 1) * self.ch..frames * self.ch]);
        self.pos -= frames as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_ratio_preserves_length() {
        let mut r = Resampler::new(48_000, 48_000, 1);
        let src: Vec<f32> = (0..480).map(|i| i as f32).collect();
        let mut out = Vec::new();
        r.process(&src, &mut out);
        assert_eq!(out.len(), 480);
    }

    /// 44.1k -> 48k must produce roughly rate-ratio more samples, and stay stable
    /// across many blocks (no drift or runaway from the fractional position).
    #[test]
    fn upsample_rate_is_stable_across_blocks() {
        let mut r = Resampler::new(44_100, 48_000, 1);
        let src = vec![0.5f32; 441];
        let mut total = 0;
        for _ in 0..100 {
            let mut out = Vec::new();
            r.process(&src, &mut out);
            total += out.len();
        }
        // 100 blocks of 441 @44.1k = 1.0s -> ~48000 samples out, allow a small margin.
        assert!((total as i64 - 48_000).abs() < 50, "got {total}");
    }

    #[test]
    fn stereo_keeps_channels_interleaved() {
        let mut r = Resampler::new(48_000, 48_000, 2);
        let src = vec![1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
        let mut out = Vec::new();
        r.process(&src, &mut out);
        assert_eq!(out.len() % 2, 0);
        // L stays positive, R stays negative - no cross-channel bleed.
        for pair in out.chunks(2) {
            assert!(pair[0] > 0.0 && pair[1] < 0.0);
        }
    }
}
