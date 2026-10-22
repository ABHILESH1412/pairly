//! What this PC plays (the default speakers, through WASAPI's loopback recording), as 16-bit
//! stereo in 20 ms pieces, for the screen being shared.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Receives (sample rate, channels, 16-bit little-endian samples).
pub type Sink = Box<dyn Fn(u32, u8, Vec<u8>) + Send>;

/// Recording until stopped (or dropped).
pub struct Loopback {
    stop: Arc<AtomicBool>,
}

impl Loopback {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start recording what this PC plays; `None` if there's no way to (no speakers).
pub fn start(sink: Sink) -> Option<Loopback> {
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    {
        let flag = stop.clone();
        std::thread::Builder::new()
            .name("pairly-sound".into())
            .spawn(move || {
                if let Err(e) = imp::record(&sink, &flag) {
                    tracing::debug!(error = %e, "no sound with the shared screen");
                }
            })
            .ok()?;
    }
    #[cfg(not(windows))]
    let _ = sink;
    Some(Loopback { stop })
}

/// Samples as 16-bit stereo: the first two channels (a mono source on both).
fn to_stereo_i16(samples: impl Iterator<Item = f32>, channels: usize, out: &mut Vec<u8>) {
    let samples: Vec<f32> = samples.collect();
    for frame in samples.chunks(channels.max(1)) {
        let left = frame.first().copied().unwrap_or(0.0);
        let right = frame.get(1).copied().unwrap_or(left);
        for s in [left, right] {
            let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
        IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole,
        eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    };

    use super::{Sink, to_stereo_i16};

    /// 200 ms of buffer, in 100-nanosecond units.
    const BUFFER: i64 = 2_000_000;

    pub fn record(sink: &Sink, stop: &AtomicBool) -> windows::core::Result<()> {
        // SAFETY: COM calls in their documented order, on this thread; the mix format is read
        // before it's freed; every buffer is released after it's copied.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let speakers = devices.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = speakers.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let (rate, channels, bits) = (
                (*format).nSamplesPerSec,
                usize::from((*format).nChannels),
                (*format).wBitsPerSample,
            );
            let started = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER,
                0,
                format,
                None,
            );
            CoTaskMemFree(Some(format as *const _));
            started?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            tracing::info!(rate, channels, bits, "recording this PC's sound");
            // 20 ms of 16-bit stereo.
            let piece = (rate as usize / 50) * 4;
            let mut out: Vec<u8> = Vec::with_capacity(piece * 2);
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
                while capture.GetNextPacketSize()? > 0 {
                    let mut data = std::ptr::null_mut();
                    let (mut frames, mut flags) = (0u32, 0u32);
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let count = frames as usize * channels;
                    if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                        out.resize(out.len() + frames as usize * 4, 0);
                    } else if bits == 32 {
                        // The usual mix format: 32-bit float.
                        let samples = std::slice::from_raw_parts(data.cast::<f32>(), count);
                        to_stereo_i16(samples.iter().copied(), channels, &mut out);
                    } else if bits == 16 {
                        let samples = std::slice::from_raw_parts(data.cast::<i16>(), count);
                        let floats = samples.iter().map(|&s| f32::from(s) / f32::from(i16::MAX));
                        to_stereo_i16(floats, channels, &mut out);
                    }
                    capture.ReleaseBuffer(frames)?;
                }
                while out.len() >= piece {
                    let rest = out.split_off(piece);
                    sink(rate, 2, std::mem::replace(&mut out, rest));
                }
            }
            let _ = client.Stop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_become_16_bit_stereo() {
        let mut out = Vec::new();
        // Two frames of 6-channel sound: only the first two channels are kept.
        to_stereo_i16(
            [1.0, -1.0, 0.5, 0.5, 0.5, 0.5, 0.0, 2.0, 0.0, 0.0, 0.0, 0.0].into_iter(),
            6,
            &mut out,
        );
        let v: Vec<i16> = out
            .chunks(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(v, [i16::MAX, -i16::MAX, 0, i16::MAX]);
        // Mono goes to both sides.
        out.clear();
        to_stereo_i16([0.5].into_iter(), 1, &mut out);
        assert_eq!(out.len(), 4);
    }
}
