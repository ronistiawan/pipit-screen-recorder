//! Preview audio during playback.
//!
//! The preview used to be video-only: pressing ▶ showed moving frames but no
//! sound. This module decodes the recorded MP4's audio in the background —
//! segment by segment, honouring the timeline's cuts — into a small ring
//! buffer that the cpal output callback drains in real time. The speaker
//! device is the clock, so no manual pacing is needed; pause/seek simply drop
//! and respawn the session alongside the video player.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::encoder::get_ffmpeg_path;
use crate::player::PlaySegment;

/// Seconds of decoded audio buffered ahead of the speaker. Covers the
/// ffmpeg spawn gap at segment cuts (~100ms) without noticeable delay.
const RING_SECONDS: f64 = 0.4;

struct Shared {
    stop: AtomicBool,
    /// Live ffmpeg child so `Drop` can kill it while the decoder thread is
    /// blocked reading its stdout.
    child: Mutex<Option<Child>>,
    /// Interleaved f32 samples waiting for the output callback.
    ring: Mutex<VecDeque<f32>>,
    /// Max samples (f32s) the ring holds before the decoder waits.
    cap: usize,
}

/// Handle to a playing preview. Dropping it kills ffmpeg, joins the decoder,
/// and stops the output stream.
pub struct PreviewAudio {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    _stream: Option<cpal::Stream>,
}

impl PreviewAudio {
    /// Start playing `segments` from timeline second `from`.
    /// Returns `None` when there is no usable output device — playback then
    /// continues silently instead of failing.
    pub fn start(segments: Vec<PlaySegment>, from: f64) -> Option<PreviewAudio> {
        if segments.is_empty() {
            return None;
        }
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let supported = device.default_output_config().ok()?;
        let sample_format = supported.sample_format();
        let config = supported.config();
        let channels = config.channels.max(1) as usize;
        let rate = config.sample_rate.max(8000);

        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            child: Mutex::new(None),
            ring: Mutex::new(VecDeque::with_capacity(
                (rate as usize * channels * 2).max(1024),
            )),
            cap: (rate as f64 * channels as f64 * RING_SECONDS) as usize,
        });

        // Build (but don't keep) the stream first: without a device format we
        // understand, skip audio entirely rather than half-start.
        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_stream::<f32>(&device, config, shared.clone()),
            cpal::SampleFormat::I16 => build_stream::<i16>(&device, config, shared.clone()),
            _ => None,
        }?;
        stream.play().ok()?;

        let thread_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("preview-audio".into())
            .spawn(move || decode_loop(thread_shared, segments, from, rate, channels))
            .ok()?;

        Some(PreviewAudio {
            shared,
            handle: Some(handle),
            _stream: Some(stream),
        })
    }
}

impl Drop for PreviewAudio {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Kill first so a thread blocked on stdout read sees EOF and exits.
        let child = self
            .shared
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: Arc<Shared>,
) -> Option<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _info| fill_output(&shared, data),
            |e| eprintln!("[preview-audio] output error: {e}"),
            None,
        )
        .ok()
}

/// Drain the ring into the device buffer; silence on underflow/stop.
fn fill_output<T>(shared: &Shared, data: &mut [T])
where
    T: FromSample<f32>,
{
    if shared.stop.load(Ordering::Relaxed) {
        for s in data.iter_mut() {
            *s = T::from_sample_(0.0f32);
        }
        return;
    }
    let mut ring = shared.ring.lock().unwrap_or_else(|e| e.into_inner());
    let n = data.len().min(ring.len());
    for s in data.iter_mut().take(n) {
        let v = ring.pop_front().unwrap_or(0.0);
        *s = T::from_sample_(v.clamp(-1.0, 1.0));
    }
    for s in data.iter_mut().skip(n) {
        *s = T::from_sample_(0.0f32);
    }
}

fn spawn_decoder(
    ffmpeg: &Path,
    seg: &PlaySegment,
    src_from: f64,
    dur: f64,
    rate: u32,
    channels: usize,
) -> Option<(Child, ChildStdout)> {
    let mut child = Command::new(ffmpeg)
        .args([
            "-nostdin",
            "-v",
            "error",
            "-ss",
            &format!("{:.3}", src_from.max(0.0)),
            "-i",
            &seg.source_path.to_string_lossy(),
            "-t",
            &format!("{:.3}", dur),
            "-map",
            "0:a:0",
            "-vn",
            "-ar",
            &rate.to_string(),
            "-ac",
            &channels.to_string(),
            "-f",
            "f32le",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    match child.stdout.take() {
        Some(stdout) => Some((child, stdout)),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// Kill + reap the current segment's child, if any.
fn retire_child(shared: &Shared) {
    let old = shared
        .child
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(mut old) = old {
        let _ = old.kill();
        let _ = old.wait();
    }
}

/// Push one interleaved frame into the ring, waiting while it is full.
/// Returns false when asked to stop.
fn push_frame(shared: &Shared, buf: &[u8], channels: usize) -> bool {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return false;
        }
        {
            let mut ring = shared.ring.lock().unwrap_or_else(|e| e.into_inner());
            if ring.len() + channels <= shared.cap {
                for c in 0..channels {
                    let b = [buf[c * 4], buf[c * 4 + 1], buf[c * 4 + 2], buf[c * 4 + 3]];
                    ring.push_back(f32::from_le_bytes(b));
                }
                return true;
            }
        }
        // Ring full: the speaker is keeping up; wait for it to drain.
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Decode every segment after `from` into the shared ring until EOF or stop.
fn decode_loop(
    shared: Arc<Shared>,
    segments: Vec<PlaySegment>,
    from: f64,
    rate: u32,
    channels: usize,
) {
    let ffmpeg = get_ffmpeg_path();
    let frame_bytes = channels * 4;
    for seg in &segments {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        if seg.timeline_end <= from {
            continue;
        }
        // Timeline -> source mapping (segments preserve duration 1:1).
        let offset = (from - seg.timeline_start).max(0.0);
        let src_from = seg.source_start + offset;
        let dur = seg.source_end - src_from;
        if dur <= 0.01 {
            continue;
        }
        let Some((mut child, mut stdout)) =
            spawn_decoder(&ffmpeg, seg, src_from, dur, rate, channels)
        else {
            continue;
        };
        if shared.stop.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        *shared.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

        let mut buf = vec![0u8; frame_bytes];
        loop {
            match stdout.read_exact(&mut buf) {
                Ok(()) => {
                    if !push_frame(&shared, &buf, channels) {
                        retire_child(&shared);
                        return;
                    }
                }
                // EOF / pipe error: this segment is done (or the file has no
                // audio track) — move on to the next.
                Err(_) => break,
            }
        }
        retire_child(&shared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Device-free: decode a generated tone mp4 and verify PCM lands in the
    /// ring with real (non-silent) samples.
    #[test]
    fn decodes_segment_audio_into_ring() {
        let dir = std::env::temp_dir().join("pipit_audioplay_test");
        let _ = std::fs::create_dir_all(&dir);

        // 1s stereo tone wav.
        let wav = dir.join("tone.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for i in 0..48000 {
            let t = i as f32 / 48000.0;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();

        // 1s video, then mux the tone in.
        let video_only = dir.join("video_only.mp4");
        let mut enc = crate::encoder::VideoEncoder::new();
        enc.start(&video_only, 160, 120, 30).unwrap();
        for f in 0..30 {
            let buf = vec![128u8; 160 * 120 * 4];
            enc.send_frame(buf, 160, 120, f as i64).unwrap();
        }
        enc.stop().unwrap();
        let with_audio = dir.join("video.mp4");
        let _ = std::fs::remove_file(&with_audio);
        let ffmpeg = crate::encoder::get_ffmpeg_path();
        crate::audio::mux_audio_into_video(
            &ffmpeg,
            &video_only,
            &with_audio,
            Some(&wav),
            None,
            1.0,
            1.0,
        )
        .unwrap();

        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            child: Mutex::new(None),
            ring: Mutex::new(VecDeque::new()),
            cap: 48000 * 2, // >= 1s of stereo f32
        });
        let seg = PlaySegment {
            source_path: with_audio,
            source_start: 0.0,
            source_end: 1.0,
            timeline_start: 0.0,
            timeline_end: 1.0,
        };

        let h = {
            let shared = shared.clone();
            std::thread::spawn(move || decode_loop(shared, vec![seg], 0.0, 48000, 2))
        };

        // Wait for the first ~0.2s of audio to arrive.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let len = shared.ring.lock().unwrap_or_else(|e| e.into_inner()).len();
            if len >= 48000 * 2 / 5 || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let (len, peak) = {
            let ring = shared.ring.lock().unwrap_or_else(|e| e.into_inner());
            let peak = ring.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            (ring.len(), peak)
        };
        assert!(len > 10_000, "expected decoded pcm in the ring, got {len}");
        assert!(peak > 0.1, "expected the tone to survive decode, peak {peak}");

        // Tear down like Drop does.
        shared.stop.store(true, Ordering::SeqCst);
        let child = shared.child.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut c) = child {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = h.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
