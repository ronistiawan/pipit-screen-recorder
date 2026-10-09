//! Real-time preview streaming during playback.
//!
//! The preview used to advance by chasing the playhead with one-shot ffmpeg
//! still decodes (~1 frame/second, each a fresh process spawn), which played
//! back like a slideshow. This module keeps a single ffmpeg process decoding
//! the recording continuously, paces frames against the playback clock, and
//! ships RGBA frames to the UI over a small channel so the preview updates at
//! the video's real frame rate.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::encoder::get_ffmpeg_path;

/// Preview decode width bounds. The decoder targets the width the preview is
/// actually drawn at (so playback is pixel-sharp without decoding more than
/// the panel shows); these clamp degenerate values.
const MIN_PREVIEW_W: u32 = 320;
const MAX_PREVIEW_W: u32 = 2560;

/// Timeline segment + the source range it maps to. Snapshotted from the
/// timeline because the decoder thread must not touch UI-owned state.
#[derive(Debug, Clone)]
pub struct PlaySegment {
    pub source_path: PathBuf,
    pub source_start: f64,
    pub source_end: f64,
    pub timeline_start: f64,
    pub timeline_end: f64,
}

/// Immutable description of one playback session.
pub struct PlaySpec {
    pub segments: Vec<PlaySegment>,
    /// Recording fps; the encoder writes CFR, so frame N sits at N / fps.
    pub fps: f64,
    /// Encoded size (may be odd — the file itself is padded to even).
    pub width: u32,
    pub height: u32,
    /// Timeline position at `t0`: the same clock `update_playback` advances.
    pub from: f64,
    pub t0: Instant,
    /// Decoding stops once the playhead reaches this.
    pub total: f64,
    /// Decode width the preview is drawn at (0 = full source width).
    /// Frames come out sharp at their on-screen size instead of being
    /// upscaled from a fixed small size.
    pub target_width: u32,
}

/// One decoded preview frame.
pub struct StreamFrame {
    /// Timeline seconds the frame belongs to (UI freshness checks).
    pub tl_t: f64,
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    /// height / width, ready for the preview's aspect-fit draw.
    pub aspect: f32,
}

/// Result of draining the decoder's frame channel.
pub enum Poll {
    /// Newest frame (anything older was drained too).
    Frame(StreamFrame),
    /// Decoder is alive but has nothing new yet.
    Idle,
    /// Decoder exited (error/EOF) — the caller should fall back to stills.
    Ended,
}

struct Shared {
    stop: AtomicBool,
    /// Live ffmpeg child so `Drop` can kill it even while the decoder thread
    /// is blocked reading its stdout.
    child: Mutex<Option<Child>>,
}

/// Handle to a running background decode. Dropping it kills ffmpeg and joins
/// the thread.
pub struct PreviewPlayer {
    rx: crossbeam_channel::Receiver<StreamFrame>,
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl PreviewPlayer {
    pub fn start(spec: PlaySpec) -> Option<PreviewPlayer> {
        if spec.segments.is_empty() || spec.fps <= 0.0 || spec.width == 0 || spec.height == 0 {
            return None;
        }
        let (tx, rx) = crossbeam_channel::bounded::<StreamFrame>(3);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            child: Mutex::new(None),
        });
        let thread_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("preview-player".into())
            .spawn(move || decode_loop(thread_shared, tx, spec))
            .ok()?;
        Some(PreviewPlayer {
            rx,
            shared,
            handle: Some(handle),
        })
    }

    /// Drain pending frames and return the newest one.
    pub fn poll(&self) -> Poll {
        let mut latest = None;
        loop {
            match self.rx.try_recv() {
                Ok(f) => latest = Some(f),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    return match latest {
                        Some(f) => Poll::Frame(f),
                        None => Poll::Ended,
                    };
                }
            }
        }
        match latest {
            Some(f) => Poll::Frame(f),
            None => Poll::Idle,
        }
    }
}

impl Drop for PreviewPlayer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Kill first so a thread blocked on stdout read sees EOF and exits.
        let child = self.shared.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Decode size for a source size + target width: source padded to even
/// (yuv420p), target clamped to `[MIN_PREVIEW_W, MAX_PREVIEW_W]` and the
/// source width, aspect preserved.
fn preview_dims(src_w: u32, src_h: u32, target_w: u32) -> (u32, u32) {
    let ew = src_w.div_ceil(2) * 2;
    let eh = src_h.div_ceil(2) * 2;
    let w = if target_w == 0 {
        ew
    } else {
        target_w.clamp(MIN_PREVIEW_W, MAX_PREVIEW_W).min(ew)
    };
    let h = ((eh as f64 * w as f64 / ew as f64).round() as u32).max(1);
    (w, h)
}

fn segment_index(segments: &[PlaySegment], tl: f64) -> Option<usize> {
    segments
        .iter()
        .position(|s| tl >= s.timeline_start && tl < s.timeline_end)
}

/// Kill + reap the previous decoder's child, if any.
fn retire_child(shared: &Shared) {
    let old = shared.child.lock().unwrap().take();
    if let Some(mut old) = old {
        let _ = old.kill();
        let _ = old.wait();
    }
}

/// Sleep in small slices so the stop flag is noticed quickly.
fn sleep_while(shared: &Shared, d: Duration) {
    let deadline = Instant::now() + d;
    while Instant::now() < deadline {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(left.min(Duration::from_millis(10)));
    }
}

/// Block until the playback clock reaches `tl_t`. Returns false when asked
/// to stop.
fn pace_until(shared: &Shared, spec: &PlaySpec, tl_t: f64) -> bool {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return false;
        }
        let wall_tl = spec.from + spec.t0.elapsed().as_secs_f64();
        let remain = tl_t - wall_tl;
        if remain <= 0.001 {
            return true;
        }
        sleep_while(shared, Duration::from_secs_f64(remain.min(0.02)));
    }
}

/// Send a frame without hanging forever if the UI stopped draining (so Drop
/// can always join the thread). Returns false when asked to stop.
fn send_frame(
    shared: &Shared,
    tx: &crossbeam_channel::Sender<StreamFrame>,
    mut frame: StreamFrame,
) -> bool {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return false;
        }
        match tx.send_timeout(frame, Duration::from_millis(50)) {
            Ok(()) => return true,
            Err(crossbeam_channel::SendTimeoutError::Timeout(f)) => frame = f,
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

fn spawn_decoder(
    ffmpeg: &Path,
    seg: &PlaySegment,
    src_t: f64,
    out_w: u32,
    out_h: u32,
) -> Option<(Child, ChildStdout)> {
    let mut child = Command::new(ffmpeg)
        .args([
            "-nostdin",
            "-v",
            "error",
            // Fast seek: start from the nearest keyframe at/before src_t;
            // ffmpeg decodes and discards up to the exact timestamp.
            "-ss",
            &format!("{:.3}", src_t.max(0.0)),
            "-i",
            &seg.source_path.to_string_lossy(),
            "-map",
            "0:v:0",
            "-an",
            "-vf",
            &format!("scale={out_w}:{out_h}"),
            "-pix_fmt",
            "rgba",
            "-f",
            "rawvideo",
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

struct Decoder {
    seg_idx: usize,
    stdout: ChildStdout,
    /// Playhead position when this decode session started; frame N's timeline
    /// time is `spawn_tl + N / fps`.
    spawn_tl: f64,
    frames_read: u64,
}

fn decode_loop(
    shared: Arc<Shared>,
    tx: crossbeam_channel::Sender<StreamFrame>,
    spec: PlaySpec,
) {
    let ffmpeg = get_ffmpeg_path();
    let (out_w, out_h) = preview_dims(spec.width, spec.height, spec.target_width);
    if out_w == 0 || out_h == 0 {
        return;
    }
    let frame_bytes = out_w as usize * out_h as usize * 4;
    let frame_dt = 1.0 / spec.fps;
    let aspect = out_h as f32 / out_w as f32;

    let mut dec: Option<Decoder> = None;
    // Consecutive spawn/EOF failures before giving up (the UI then falls
    // back to chase stills instead of a frozen preview).
    let mut strikes = 0u32;

    while !shared.stop.load(Ordering::Relaxed) {
        let wall_tl = spec.from + spec.t0.elapsed().as_secs_f64();
        if wall_tl >= spec.total {
            break;
        }
        let Some(seg_idx) = segment_index(&spec.segments, wall_tl) else {
            sleep_while(&shared, Duration::from_millis(20));
            continue;
        };

        // Restart when the playhead entered another segment (cuts splice
        // non-contiguous source ranges) or the next frame would run past the
        // segment's source range (content that was cut out).
        let next_tl = dec
            .as_ref()
            .map(|d| d.spawn_tl + d.frames_read as f64 * frame_dt);
        let needs_restart = match (&dec, next_tl) {
            (Some(d), Some(ntl)) => {
                d.seg_idx != seg_idx
                    || ntl >= spec.segments[seg_idx].timeline_end - 1e-9
            }
            _ => true,
        };
        if needs_restart {
            retire_child(&shared);
            dec = None;
            let seg = &spec.segments[seg_idx];
            let span = (seg.timeline_end - seg.timeline_start).max(1e-6);
            let frac = ((wall_tl - seg.timeline_start) / span).clamp(0.0, 1.0);
            let src_t = seg.source_start + frac * (seg.source_end - seg.source_start);
            match spawn_decoder(&ffmpeg, seg, src_t, out_w, out_h) {
                Some((mut child, stdout)) => {
                    if shared.stop.load(Ordering::Relaxed) {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    *shared.child.lock().unwrap() = Some(child);
                    dec = Some(Decoder {
                        seg_idx,
                        stdout,
                        spawn_tl: wall_tl,
                        frames_read: 0,
                    });
                    strikes = 0;
                }
                None => {
                    strikes += 1;
                    if strikes >= 3 {
                        break;
                    }
                    sleep_while(&shared, Duration::from_millis(150));
                    continue;
                }
            }
        }

        // Read the next frame (blocks until ffmpeg produces it — pipe
        // backpressure keeps the decode honest, pacing happens below).
        let read = {
            let Some(decoder) = dec.as_mut() else { continue };
            let mut rgba = vec![0u8; frame_bytes];
            match decoder.stdout.read_exact(&mut rgba) {
                Ok(()) => {
                    let tl_t = decoder.spawn_tl + decoder.frames_read as f64 * frame_dt;
                    decoder.frames_read += 1;
                    Some((tl_t, rgba))
                }
                Err(_) => None,
            }
        };
        let Some((tl_t, rgba)) = read else {
            // EOF or pipe error: the file may be shorter than the segment
            // claims. Retry a few times, then hand back to the stills path.
            retire_child(&shared);
            dec = None;
            strikes += 1;
            if strikes >= 3 {
                break;
            }
            sleep_while(&shared, Duration::from_millis(120));
            continue;
        };
        strikes = 0;

        // Deliver at the frame's playback time so the UI sees real-time video.
        if !pace_until(&shared, &spec, tl_t) {
            break;
        }
        let frame = StreamFrame {
            tl_t,
            width: out_w as usize,
            height: out_h as usize,
            rgba,
            aspect,
        };
        if !send_frame(&shared, &tx, frame) {
            break;
        }
    }

    retire_child(&shared);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_dims_are_even_and_capped() {
        // Target the on-screen width: no upscale, no over-decoding.
        assert_eq!(preview_dims(1920, 1080, 800), (800, 450));
        assert_eq!(preview_dims(1920, 1080, 0), (1920, 1080));
        // Odd capture sizes pad up to even like the encoder's pad filter.
        assert_eq!(preview_dims(321, 241, 0), (322, 242));
        // Never wider than the source, never absurd.
        assert_eq!(preview_dims(100, 100, 500), (100, 100));
        assert_eq!(preview_dims(1920, 1080, 64), (320, 180));
        assert_eq!(preview_dims(1920, 1080, 9999), (1920, 1080));
        assert_eq!(preview_dims(1080, 1920, 640), (640, 1138));
    }

    #[test]
    fn segment_index_maps_timeline_ranges() {
        let seg = |s: f64, e: f64| PlaySegment {
            source_path: PathBuf::from("x.mp4"),
            source_start: s,
            source_end: e,
            timeline_start: s,
            timeline_end: e,
        };
        let segs = vec![seg(0.0, 2.0), seg(2.0, 4.0)];
        assert_eq!(segment_index(&segs, 0.0), Some(0));
        assert_eq!(segment_index(&segs, 1.999), Some(0));
        assert_eq!(segment_index(&segs, 2.0), Some(1));
        assert_eq!(segment_index(&segs, 4.0), None);
        assert_eq!(segment_index(&segs, -0.1), None);
    }

    #[test]
    fn streams_playback_frames_in_realtime() {
        let out = std::env::temp_dir().join("pipit_player_test.mp4");
        let _ = std::fs::remove_file(&out);
        let mut enc = crate::encoder::VideoEncoder::new();
        enc.start(&out, 320, 240, 30).expect("start encoder");
        for f in 0..90u32 {
            let mut buf = vec![0u8; 320 * 240 * 4];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((i + f as usize) % 256) as u8;
            }
            enc.send_frame(buf, 320, 240, f as i64).expect("send frame");
        }
        enc.stop().expect("stop encoder");

        let spec = PlaySpec {
            segments: vec![PlaySegment {
                source_path: out.clone(),
                source_start: 0.0,
                source_end: 3.0,
                timeline_start: 0.0,
                timeline_end: 3.0,
            }],
            fps: 30.0,
            width: 320,
            height: 240,
            from: 0.0,
            t0: Instant::now(),
            total: 3.0,
            target_width: 0,
        };
        let player = PreviewPlayer::start(spec).expect("player starts");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut frames = Vec::new();
        while Instant::now() < deadline && frames.len() < 10 {
            match player.poll() {
                Poll::Frame(f) => frames.push(f),
                Poll::Idle => std::thread::sleep(Duration::from_millis(10)),
                Poll::Ended => break,
            }
        }
        assert!(
            frames.len() >= 5,
            "expected a stream of frames, got {}",
            frames.len()
        );
        let f0 = &frames[0];
        assert_eq!(f0.width, 320);
        assert_eq!(f0.height, 240);
        assert_eq!(f0.rgba.len(), 320 * 240 * 4);
        for pair in frames.windows(2) {
            assert!(pair[1].tl_t >= pair[0].tl_t, "timeline times must advance");
        }
        let _ = std::fs::remove_file(&out);
    }
}
