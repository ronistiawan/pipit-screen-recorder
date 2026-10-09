use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

pub(crate) fn get_ffmpeg_path() -> PathBuf {
    let candidates = [
        PathBuf::from(r"D:\Tools\ffmpeg\bin\ffmpeg.exe"),
        PathBuf::from(r"C:\ffmpeg\bin\ffmpeg.exe"),
        PathBuf::from(r"C:\Program Files\ffmpeg\bin\ffmpeg.exe"),
        PathBuf::from("ffmpeg"), // fallback to PATH
    ];

    for candidate in &candidates {
        // `PathBuf::from("ffmpeg").exists()` checks CWD, so only trust
        // candidates with an actual file on disk, except the bare name.
        if candidate.components().count() > 1 && candidate.exists() {
            return candidate.clone();
        }
    }

    PathBuf::from("ffmpeg")
}

fn get_ffprobe_path() -> PathBuf {
    let ffmpeg = get_ffmpeg_path();
    if ffmpeg.file_name().map(|n| n == "ffmpeg.exe").unwrap_or(false) {
        let probe = ffmpeg.with_file_name("ffprobe.exe");
        if probe.exists() {
            return probe;
        }
    }
    PathBuf::from("ffprobe")
}

/// Last ~4KB of stderr so error messages stay readable.
fn tail(s: &str) -> String {
    const MAX: usize = 4000;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("...[truncated]\n{}", &s[s.len() - MAX..])
    }
}

pub struct VideoEncoder {
    frame_sender: Option<mpsc::Sender<FrameToEncode>>,
    encoding_thread: Option<thread::JoinHandle<Result<()>>>,
    is_encoding: bool,
}

struct FrameToEncode {
    data: Vec<u8>,
}

impl VideoEncoder {
    pub fn new() -> Self {
        let ffmpeg_path = get_ffmpeg_path();
        if ffmpeg_path.components().count() > 1 {
            eprintln!("Using ffmpeg: {}", ffmpeg_path.display());
        } else {
            eprintln!("Using ffmpeg from PATH");
        }

        Self {
            frame_sender: None,
            encoding_thread: None,
            is_encoding: false,
        }
    }

    pub fn start(
        &mut self,
        output_path: impl AsRef<Path>,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<()> {
        if width == 0 || height == 0 {
            anyhow::bail!("invalid capture size {width}x{height}");
        }
        let output_path = output_path.as_ref().to_path_buf();
        if let Some(parent) = output_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let ffmpeg_path = get_ffmpeg_path();
        if ffmpeg_path.components().count() > 1 && !ffmpeg_path.exists() {
            anyhow::bail!(
                "ffmpeg not found at {}. Install ffmpeg or put it on PATH.",
                ffmpeg_path.display()
            );
        }

        let (tx, rx) = mpsc::channel::<FrameToEncode>();

        let encoding_thread = thread::spawn(move || {
            Self::encoding_loop(&ffmpeg_path, &output_path, width, height, fps, rx)
        });

        self.frame_sender = Some(tx);
        self.encoding_thread = Some(encoding_thread);
        self.is_encoding = true;

        Ok(())
    }

    fn encoding_loop(
        ffmpeg_path: &Path,
        output_path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        rx: mpsc::Receiver<FrameToEncode>,
    ) -> Result<()> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| anyhow::anyhow!("capture size overflow {width}x{height}"))?;

        let mut child = Command::new(ffmpeg_path)
            .args([
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                &format!("{}x{}", width, height),
                "-r",
                &fps.to_string(),
                "-i",
                "-",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "23",
                // Short GOP so scrub-preview seeks land quickly (decode ≤60
                // frames instead of up to a ~250-frame keyframe interval).
                "-g",
                "60",
                "-pix_fmt",
                "yuv420p",
                // Region selections are often odd-sized; yuv420p/x264 needs even dims.
                "-vf",
                "pad=ceil(iw/2)*2:ceil(ih/2)*2",
                "-movflags",
                "+faststart",
                &output_path.to_string_lossy(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to launch {} — is ffmpeg installed and on PATH?",
                    ffmpeg_path.display()
                )
            })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("ffmpeg stdin unavailable"))?;

        let mut frames = 0u64;
        let mut write_err: Option<anyhow::Error> = None;

        while let Ok(frame) = rx.recv() {
            if frame.data.len() != expected {
                write_err = Some(anyhow::anyhow!(
                    "frame {} has {} bytes but {width}x{height} RGBA needs {expected} — dropping recording",
                    frames,
                    frame.data.len(),
                ));
                break;
            }
            if let Err(e) = stdin.write_all(&frame.data) {
                write_err = Some(anyhow::anyhow!("failed writing frame {frames} to ffmpeg: {e}"));
                break;
            }
            frames += 1;
        }
        // Close stdin so ffmpeg finishes the file.
        drop(stdin);

        // stdin was taken above for writing; stderr is still piped so
        // wait_with_output captures the ffmpeg log for error reports.
        let output = child
            .wait_with_output()
            .context("failed waiting for ffmpeg")?;
        let log = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "ffmpeg encoded {frames} frames -> {} (status: {})",
            output_path.display(),
            output.status
        );

        if let Some(e) = write_err {
            return Err(anyhow::anyhow!("{e}\nffmpeg log:\n{}", tail(&log)));
        }
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "ffmpeg exited with {} after {frames} frames.\nffmpeg log:\n{}",
                output.status,
                tail(&log)
            ));
        }
        if frames == 0 {
            return Err(anyhow::anyhow!("no frames received — nothing was recorded"));
        }
        Ok(())
    }

    pub fn send_frame(&self, data: Vec<u8>, _w: u32, _h: u32, _pts: i64) -> Result<()> {
        if let Some(tx) = &self.frame_sender {
            tx.send(FrameToEncode { data })
                .map_err(|_| anyhow::anyhow!("encoder thread is gone"))?;
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.frame_sender.take(); // closes channel -> thread finishes encode
        if let Some(h) = self.encoding_thread.take() {
            match h.join() {
                Ok(r) => r?,
                Err(_) => anyhow::bail!("encoder thread panicked"),
            }
        }
        self.is_encoding = false;
        // Remove stale frame dumps from the old encoder, if any.
        let stale = std::env::temp_dir().join("pipit_frames");
        if stale.exists() {
            let _ = std::fs::remove_dir_all(&stale);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn is_encoding(&self) -> bool {
        self.is_encoding
    }
}

impl Default for VideoEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn encodes_odd_size_clip_via_stdin() {
        // Odd dimensions (like a dragged region) must not fail: the pad
        // filter rounds up to even for yuv420p/x264.
        let out = std::env::temp_dir().join("pipit_encoder_test.mp4");
        let _ = std::fs::remove_file(&out);
        let w = 321u32;
        let h = 241u32;
        let mut enc = VideoEncoder::new();
        enc.start(&out, w, h, 30).expect("start encoder");
        for f in 0..30u32 {
            let mut buf = vec![0u8; (w * h * 4) as usize];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((i + f as usize) % 256) as u8;
            }
            enc.send_frame(buf, w, h, f as i64).expect("send frame");
        }
        enc.stop().expect("stop encoder");
        let meta = std::fs::metadata(&out).expect("output exists");
        assert!(meta.len() > 1024, "output too small: {}", meta.len());
        assert!(
            meta.len() < 5_000_000,
            "output suspiciously large: {}",
            meta.len()
        );
        let _ = std::fs::remove_file(&out);
    }

    fn make_clip(path: &std::path::Path, secs: u32) {
        let _ = std::fs::remove_file(path);
        let mut enc = VideoEncoder::new();
        enc.start(path, 160, 120, 30).expect("start encoder");
        for f in 0..(secs * 30) {
            let mut buf = vec![0u8; 160 * 120 * 4];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((i + f as usize) % 256) as u8;
            }
            enc.send_frame(buf, 160, 120, f as i64).expect("send frame");
        }
        enc.stop().expect("stop encoder");
    }

    #[test]
    fn export_timeline_trims_video_only() {
        let dir = std::env::temp_dir().join("pipit_export_test");
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("src.mp4");
        make_clip(&src, 2); // 2s clip
        let out = dir.join("trimmed.mp4");
        let _ = std::fs::remove_file(&out);
        export_timeline(&src, &out, &[(0.5, 1.5)], 1.0, 0.0, 0.0).expect("export");
        let meta = std::fs::metadata(&out).expect("output exists");
        assert!(meta.len() > 1024, "output too small: {}", meta.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Decode one full-resolution frame at `src_secs` from a recorded file.
/// Returns raw RGBA bytes at the ENCODED dimensions (odd capture sizes are
/// padded up to even for yuv420p — see the `pad` filter in `start`).
pub fn extract_frame(
    ffmpeg: &Path,
    video: &Path,
    src_secs: f64,
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, u32, u32)> {
    let ew = width.div_ceil(2) * 2;
    let eh = height.div_ceil(2) * 2;
    if ew == 0 || eh == 0 {
        anyhow::bail!("invalid frame size {width}x{height}");
    }
    let expected = (ew as usize)
        .checked_mul(eh as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("frame size overflow {ew}x{eh}"))?;
    let out = Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-ss",
            &format!("{:.3}", src_secs.max(0.0)),
            "-i",
            &video.to_string_lossy(),
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-pix_fmt",
            "rgba",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .with_context(|| format!("failed to launch {}", ffmpeg.display()))?;
    if !out.status.success() {
        anyhow::bail!("ffmpeg seek failed with {}", out.status);
    }
    if out.stdout.len() != expected {
        anyhow::bail!(
            "decoded {} bytes but {ew}x{eh} RGBA needs {expected} (short seek past EOF?)",
            out.stdout.len()
        );
    }
    Ok((out.stdout, ew, eh))
}

/// Export timeline segments (source-time trims of a single recording) with
/// volume + fades applied. Falls back to video-only if the input has no audio.
pub fn export_timeline(
    input_path: &Path,
    output_path: &Path,
    segments: &[(f64, f64)],
    volume: f32,
    fade_in: f64,
    fade_out: f64,
) -> Result<()> {
    if segments.is_empty() {
        anyhow::bail!("nothing to export — timeline is empty");
    }
    let ffmpeg_path = get_ffmpeg_path();
    let total: f64 = segments.iter().map(|(s, e)| (e - s).max(0.0)).sum();

    let has_audio = Command::new(get_ffprobe_path())
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
            &input_path.to_string_lossy(),
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("audio"))
        .unwrap_or(false);

    let mut parts: Vec<String> = Vec::new();
    for (i, (ss, se)) in segments.iter().enumerate() {
        parts.push(format!(
            "[0:v]trim=start={:.3}:end={:.3},setpts=PTS-STARTPTS[v{}]",
            ss.max(0.0),
            se.max(ss + 0.01),
            i
        ));
        if has_audio {
            parts.push(format!(
                "[0:a]atrim=start={:.3}:end={:.3},asetpts=PTS-STARTPTS[a{}]",
                ss.max(0.0),
                se.max(ss + 0.01),
                i
            ));
        }
    }
    let vcat = (0..segments.len())
        .map(|i| format!("[v{i}]"))
        .collect::<String>();
    parts.push(format!(
        "{vcat}concat=n={}:v=1:a=0[vcat]",
        segments.len()
    ));
    if has_audio {
        let acat = (0..segments.len())
            .map(|i| format!("[a{i}]"))
            .collect::<String>();
        parts.push(format!(
            "{acat}concat=n={}:v=0:a=1[acat]",
            segments.len()
        ));
        let mut afilter = format!("volume={:.2}", volume.clamp(0.0, 2.0));
        if fade_in > 0.05 {
            afilter.push_str(&format!(",afade=t=in:st=0:d={:.2}", fade_in));
        }
        if fade_out > 0.05 {
            afilter.push_str(&format!(
                ",afade=t=out:st={:.2}:d={:.2}",
                (total - fade_in.max(0.0) - fade_out).max(0.0),
                fade_out
            ));
        }
        parts.push(format!("[acat]{afilter}[aout]"));
    }
    let filter = parts.join(";");

    let mut cmd = Command::new(&ffmpeg_path);
    cmd.args(["-y", "-i", &input_path.to_string_lossy()]);
    cmd.args(["-filter_complex", &filter]);
    cmd.args(["-map", "[vcat]"]);
    if has_audio {
        cmd.args(["-map", "[aout]"]);
    }
    cmd.args([
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-pix_fmt",
        "yuv420p",
    ]);
    if has_audio {
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-ar", "48000", "-ac", "2"]);
    }
    cmd.args([
        "-movflags",
        "+faststart",
        &output_path.to_string_lossy(),
    ]);
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "ffmpeg export failed with {}\nffmpeg log:\n{}",
            out.status,
            tail(&String::from_utf8_lossy(&out.stderr))
        ));
    }
    Ok(())
}

#[allow(dead_code)]
pub fn cut_video(
    input_path: &Path,
    output_path: &Path,
    cut_ranges: &[(f64, f64)],
) -> Result<()> {
    if cut_ranges.is_empty() {
        std::fs::copy(input_path, output_path)?;
        return Ok(());
    }

    let ffmpeg_path = get_ffmpeg_path();
    let ffprobe_path = get_ffprobe_path();

    let mut filter_parts = Vec::new();
    let mut current_time = 0.0;

    for (i, (start, end)) in cut_ranges.iter().enumerate() {
        if current_time < *start {
            filter_parts.push(format!(
                "[0:v]trim=start={}:end={},setpts=PTS-STARTPTS[v{}]",
                current_time, start, i
            ));
        }
        current_time = *end;
    }

    let duration_cmd = Command::new(&ffprobe_path)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            &input_path.to_string_lossy(),
        ])
        .output()?;

    let total_duration: f64 = String::from_utf8_lossy(&duration_cmd.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0);

    if current_time < total_duration {
        filter_parts.push(format!(
            "[0:v]trim=start={},setpts=PTS-STARTPTS[v{}]",
            current_time,
            filter_parts.len()
        ));
    }

    if filter_parts.is_empty() {
        std::fs::copy(input_path, output_path)?;
        return Ok(());
    }

    let filter_complex = filter_parts.join(";");
    let concat_inputs = (0..filter_parts.len())
        .map(|i| format!("[v{}]", i))
        .collect::<Vec<_>>()
        .join("");
    let filter_complex = format!(
        "{};{}concat=n={}:v=1:a=0[out]",
        filter_complex,
        concat_inputs,
        filter_parts.len()
    );

    let out = Command::new(&ffmpeg_path)
        .args([
            "-y",
            "-i",
            &input_path.to_string_lossy(),
            "-filter_complex",
            &filter_complex,
            "-map",
            "[out]",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-movflags",
            "+faststart",
            &output_path.to_string_lossy(),
        ])
        .output()?;

    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "ffmpeg cut failed with {}\nffmpeg log:\n{}",
            out.status,
            tail(&String::from_utf8_lossy(&out.stderr))
        ));
    }

    Ok(())
}
