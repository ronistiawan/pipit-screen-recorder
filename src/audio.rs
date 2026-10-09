use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::thread::JoinHandle;

fn level_from_floats(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sum = 0.0f64;
    let mut peak = 0.0f32;
    // Sample every value: the old step_by(2) skipped half of mono chunks
    // (understating RMS ~3 dB) and could miss the peak entirely.
    for &s in samples.iter() {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sum += (s as f64) * (s as f64);
    }
    let rms = (sum / (samples.len() as f64).max(1.0)).sqrt() as f32;
    // Blend RMS + peak so meters feel alive even on quiet audio, then map
    // through the perceptual (dB) curve so quiet-but-present signals like a
    // far-field mic actually move the meters (linear 0.005 would be invisible).
    display_level((rms * 1.4 + peak * 0.25).clamp(0.0, 1.0))
}

/// Perceptual loudness mapping: linear 0..1 -> display 0..1 via a -50 dB
/// floor. Linear scaling hides quiet signals (0.005 peak = a 0.5% bar, i.e.
/// "no wave"); dB scaling shows them (0.005 -> ~0.08, speech at 0.05 -> ~0.48)
/// while true silence/noise floor (< -50 dB) still reads 0.
pub fn display_level(v: f32) -> f32 {
    if v <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * v.log10();
    if db < -50.0 {
        0.0
    } else {
        ((db + 50.0) / 50.0).clamp(0.0, 1.0)
    }
}

fn store_level(slot: &Arc<AtomicU32>, v: f32) {
    slot.store(v.to_bits(), Ordering::Relaxed);
}

pub fn load_level(slot: &Arc<AtomicU32>) -> f32 {
    f32::from_bits(slot.load(Ordering::Relaxed))
}

#[derive(Debug, Clone)]
pub struct AudioDevices {
    pub microphones: Vec<String>,
    #[allow(dead_code)]
    pub speakers: Vec<String>,
    pub default_mic: Option<String>,
    #[allow(dead_code)]
    pub default_speaker: Option<String>,
}

pub fn list_audio_devices() -> AudioDevices {
    use cpal::traits::HostTrait;
    let mut mics = Vec::new();
    let mut speakers = Vec::new();
    let mut default_mic = None;
    let mut default_speaker = None;

    // Prefer WASAPI friendly names (Windows). initialize_mta returns HRESULT.
    let _ = wasapi::initialize_mta();
    if let Ok(enumerator) = wasapi::DeviceEnumerator::new() {
        if let Ok(col) = enumerator.get_device_collection(&wasapi::Direction::Capture) {
            for d in &col {
                if let Ok(dev) = d {
                    if let Ok(name) = dev.get_friendlyname() {
                        if !mics.contains(&name) {
                            mics.push(name);
                        }
                    }
                }
            }
        }
        if let Ok(col) = enumerator.get_device_collection(&wasapi::Direction::Render) {
            for d in &col {
                if let Ok(dev) = d {
                    if let Ok(name) = dev.get_friendlyname() {
                        if !speakers.contains(&name) {
                            speakers.push(name);
                        }
                    }
                }
            }
        }
        default_mic = enumerator
            .get_default_device(&wasapi::Direction::Capture)
            .ok()
            .and_then(|d| d.get_friendlyname().ok());
        default_speaker = enumerator
            .get_default_device(&wasapi::Direction::Render)
            .ok()
            .and_then(|d| d.get_friendlyname().ok());
    }

    // Fallback / supplement via cpal (helps when WASAPI COM is busy).
    if mics.is_empty() {
        let host = cpal::default_host();
        if let Ok(devs) = host.input_devices() {
            for d in devs {
                let n = d.to_string();
                if !n.is_empty() && !mics.contains(&n) {
                    mics.push(n);
                }
            }
        }
        if default_mic.is_none() {
            default_mic = host.default_input_device().map(|d| d.to_string());
        }
    }
    if speakers.is_empty() {
        use cpal::traits::HostTrait as _HostTrait2;
        let host = cpal::default_host();
        if let Ok(devs) = host.output_devices() {
            for d in devs {
                let n = d.to_string();
                if !n.is_empty() && !speakers.contains(&n) {
                    speakers.push(n);
                }
            }
        }
        if default_speaker.is_none() {
            default_speaker = host.default_output_device().map(|d| d.to_string());
        }
    }

    AudioDevices {
        microphones: mics,
        speakers,
        default_mic,
        default_speaker,
    }
}

fn find_device_by_name(
    enumerator: &wasapi::DeviceEnumerator,
    dir: &wasapi::Direction,
    needle: &str,
) -> Option<wasapi::Device> {
    let needle = needle.to_lowercase();
    let col = enumerator.get_device_collection(dir).ok()?;
    for d in &col {
        if let Ok(dev) = d {
            if let Ok(name) = dev.get_friendlyname() {
                let n = name.to_lowercase();
                if n == needle || n.contains(&needle) || needle.contains(&n) {
                    return Some(dev);
                }
            }
        }
    }
    None
}

/// WASAPI capture thread shared by mic + system-loopback.
/// `render_device=true` means system sound (loopback from the speaker device).
/// `channels`: 2 for system loopback (speakers are stereo), 1 for mic.
/// Most mics are mono — requesting stereo fails or yields silence on some
/// devices, so the mic is captured mono and upmixed to stereo at mux time
/// (`aformat=channel_layouts=stereo`).
fn capture_thread(
    label: &'static str,
    render_device: bool,
    device_name: Option<String>,
    out_wav: PathBuf,
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    channels: u16,
) -> Result<(), String> {
    // initialize_mta returns HRESULT; failure just means COM was already init'd differently.
    let _ = wasapi::initialize_mta();

    let enumerator =
        wasapi::DeviceEnumerator::new().map_err(|e| format!("device enumerator: {e:?}"))?;

    let dir = if render_device {
        wasapi::Direction::Render
    } else {
        wasapi::Direction::Capture
    };

    let device = match device_name {
        Some(ref n) if !n.is_empty() => find_device_by_name(&enumerator, &dir, n).ok_or_else(|| {
            format!("audio device '{n}' not found — using default instead will be tried")
        }).or_else(|_| {
            enumerator
                .get_default_device(&dir)
                .map_err(|e| format!("no audio device ({label}): {e:?}"))
        })?,
        _ => enumerator
            .get_default_device(&dir)
            .map_err(|e| format!("no audio device ({label}): {e:?}"))?,
    };

    let mut client = device
        .get_iaudioclient()
        .map_err(|e| format!("{label}: IAudioClient: {e:?}"))?;

    // 48 kHz float; WASAPI converts the mix format automatically.
    // Mic = mono (works on mono and stereo mics via autoconvert downmix),
    // system loopback = stereo.
    let channels = channels.clamp(1, 2);
    let format = wasapi::WaveFormat::new(
        32,
        32,
        &wasapi::SampleType::Float,
        48000,
        channels as usize,
        None,
    );
    let blockalign = format.get_blockalign() as usize;

    // Loopback (render device) ignores the period; mic uses the device minimum.
    let buffer_hns: i64 = if render_device {
        0
    } else {
        client.get_device_period().map(|(_, min)| min).unwrap_or(200_000)
    };
    let mode = wasapi::StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: buffer_hns,
    };
    client
        .initialize_client(&format, &wasapi::Direction::Capture, &mode)
        .map_err(|e| format!("{label}: init: {e:?}"))?;

    let event = client
        .set_get_eventhandle()
        .map_err(|e| format!("{label}: event: {e:?}"))?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|e| format!("{label}: capture client: {e:?}"))?;

    let spec = hound::WavSpec {
        channels,
        sample_rate: 48000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    if let Some(p) = out_wav.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let mut writer = hound::WavWriter::create(&out_wav, spec)
        .map_err(|e| format!("{label}: wav create: {e}"))?;

    let mut queue: VecDeque<u8> =
        VecDeque::with_capacity(4 * 48000 * blockalign.max(8));
    let mut frames_written: u64 = 0;

    client
        .start_stream()
        .map_err(|e| format!("{label}: start: {e:?}"))?;

    let mut idle_ticks = 0;
    while !stop.load(Ordering::Relaxed) {
        // Drain whatever WASAPI has buffered.
        match capture.read_from_device_to_deque(&mut queue) {
            Ok(_) => idle_ticks = 0,
            Err(e) => {
                // Buffer overruns happen under load; keep going.
                let msg = format!("{e:?}");
                if msg.contains("AUDCLNT_S_BUFFER_EMPTY") || msg.contains("empty") {
                    idle_ticks += 1;
                } else {
                    // Unknown errors: wait a bit, don't spin.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        // Write whole frames to disk + update the meter.
        // blockalign is 8 for stereo f32, 4 for mono f32.
        let frame_bytes = blockalign;
        let mut chunk_floats: Vec<f32> = Vec::with_capacity(4096);
        while queue.len() >= frame_bytes {
            if frame_bytes == 8 {
                let mut bytes = [0u8; 8];
                for i in 0..8 {
                    bytes[i] = queue.pop_front().unwrap_or(0);
                }
                let l = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                let r = f32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
                writer.write_sample(l).ok();
                writer.write_sample(r).ok();
                chunk_floats.push(l);
                chunk_floats.push(r);
                frames_written += 1;
            } else if frame_bytes == 4 {
                let mut bytes = [0u8; 4];
                for i in 0..4 {
                    bytes[i] = queue.pop_front().unwrap_or(0);
                }
                let m = f32::from_le_bytes(bytes);
                writer.write_sample(m).ok();
                chunk_floats.push(m);
                frames_written += 1;
            } else {
                // Unexpected format — drop one frame worth of bytes to avoid
                // spinning, rather than growing the queue unboundedly.
                let drop = frame_bytes.min(queue.len());
                for _ in 0..drop {
                    queue.pop_front();
                }
                break;
            }
        }
        if !chunk_floats.is_empty() {
            store_level(&level, level_from_floats(&chunk_floats));
        } else {
            // Decay the meter when silent so it doesn't freeze.
            let cur = load_level(&level);
            if cur > 0.0 {
                store_level(&level, (cur * 0.9).max(0.0));
            }
        }

        if event.wait_for_event(50).is_err() {
            // Timeout is normal (esp. loopback silence); just loop and check stop flag.
            idle_ticks += 1;
            if idle_ticks > 4000 {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }

    let _ = client.stop_stream();
    store_level(&level, 0.0);
    writer
        .finalize()
        .map_err(|e| format!("{label}: wav finalize: {e}"))?;
    eprintln!("[audio:{label}] captured {frames_written} frames -> {}", out_wav.display());
    Ok(())
}

pub struct AudioCapture {
    stop_flag: Arc<AtomicBool>,
    sys_level: Arc<AtomicU32>,
    mic_level: Arc<AtomicU32>,
    sys_handle: Option<JoinHandle<Result<(), String>>>,
    mic_handle: Option<JoinHandle<Result<(), String>>>,
    sys_path: Option<PathBuf>,
    mic_path: Option<PathBuf>,
    sys_error: Option<String>,
    mic_error: Option<String>,
}

impl AudioCapture {
    pub fn is_active(&self) -> bool {
        self.sys_handle.is_some() || self.mic_handle.is_some()
    }

    pub fn system_level(&self) -> f32 {
        load_level(&self.sys_level)
    }

    pub fn mic_level(&self) -> f32 {
        load_level(&self.mic_level)
    }

    pub fn start(
        record_system: bool,
        record_mic: bool,
        mic_device: Option<String>,
        session_dir: &Path,
    ) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let sys_level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let mic_level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let _ = std::fs::create_dir_all(session_dir);

        let mut cap = Self {
            stop_flag,
            sys_level,
            mic_level,
            sys_handle: None,
            mic_handle: None,
            sys_path: None,
            mic_path: None,
            sys_error: None,
            mic_error: None,
        };

        if record_system {
            let path = session_dir.join("system.wav");
            let _ = std::fs::remove_file(&path);
            let stop = cap.stop_flag.clone();
            let lvl = cap.sys_level.clone();
            let p = path.clone();
            let h = std::thread::Builder::new()
                .name("audio-system".into())
                .spawn(move || capture_thread("system", true, None, p, stop, lvl, 2));
            match h {
                Ok(h) => {
                    cap.sys_handle = Some(h);
                    cap.sys_path = Some(path);
                }
                Err(e) => cap.sys_error = Some(format!("system audio thread: {e}")),
            }
        }

        if record_mic {
            let path = session_dir.join("mic.wav");
            let _ = std::fs::remove_file(&path);
            let stop = cap.stop_flag.clone();
            let lvl = cap.mic_level.clone();
            let p = path.clone();
            let dev = mic_device.clone();
            let h = std::thread::Builder::new()
                .name("audio-mic".into())
                .spawn(move || capture_thread("mic", false, dev, p, stop, lvl, 1));
            match h {
                Ok(h) => {
                    cap.mic_handle = Some(h);
                    cap.mic_path = Some(path);
                }
                Err(e) => cap.mic_error = Some(format!("mic thread: {e}")),
            }
        }

        cap
    }

    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self {
            stop_flag: Arc::new(AtomicBool::new(true)),
            sys_level: Arc::new(AtomicU32::new(0f32.to_bits())),
            mic_level: Arc::new(AtomicU32::new(0f32.to_bits())),
            sys_handle: None,
            mic_handle: None,
            sys_path: None,
            mic_path: None,
            sys_error: None,
            mic_error: None,
        }
    }

    /// Signal threads to stop, join them, return usable wav files.
    pub fn stop(mut self) -> AudioResult {
        self.stop_flag.store(true, Ordering::Relaxed);
        // Give capture loops a moment to see the flag (they poll every ~50ms).
        std::thread::sleep(std::time::Duration::from_millis(150));

        let mut errors = Vec::new();
        if let Some(e) = self.sys_error.take() {
            errors.push(e);
        }
        if let Some(e) = self.mic_error.take() {
            errors.push(e);
        }
        if let Some(h) = self.sys_handle.take() {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push(e),
                Err(_) => errors.push("system audio thread panicked".into()),
            }
        }
        if let Some(h) = self.mic_handle.take() {
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push(e),
                Err(_) => errors.push("mic thread panicked".into()),
            }
        }

        let sys = self.sys_path.take().filter(|p| wav_has_audio(p));
        let mic = self.mic_path.take().filter(|p| wav_has_audio(p));
        AudioResult {
            system_wav: sys,
            mic_wav: mic,
            errors,
        }
    }
}

pub struct AudioResult {
    pub system_wav: Option<PathBuf>,
    pub mic_wav: Option<PathBuf>,
    pub errors: Vec<String>,
}

impl AudioResult {
    pub fn has_audio(&self) -> bool {
        self.system_wav.is_some() || self.mic_wav.is_some()
    }
}

fn wav_has_audio(p: &Path) -> bool {
    // 44-byte header + data. Loopback with nothing playing yields a
    // header-only (or near-empty) file, while a working mic always streams
    // samples — even digital silence. Threshold ~0.04s of stereo f32 so
    // short takes (~0.5s, incl. mono mic at half the byte rate) are kept.
    match std::fs::metadata(p) {
        Ok(m) => m.len() > 44 + 8_000,
        Err(_) => false,
    }
}

/// Mux captured wavs into the final mp4. Video stays as-is (`-c:v copy`).
pub fn mux_audio_into_video(
    ffmpeg: &Path,
    video_input: &Path,
    final_output: &Path,
    system_wav: Option<&Path>,
    mic_wav: Option<&Path>,
    system_volume: f32,
    mic_volume: f32,
) -> anyhow::Result<()> {
    use std::process::Command;
    if system_wav.is_none() && mic_wav.is_none() {
        if video_input != final_output {
            std::fs::copy(video_input, final_output)?;
        }
        return Ok(());
    }
    let mut cmd = Command::new(ffmpeg);
    cmd.arg("-y")
        .arg("-i")
        .arg(video_input);
    if let Some(p) = system_wav {
        cmd.arg("-i").arg(p);
    }
    if let Some(p) = mic_wav {
        cmd.arg("-i").arg(p);
    }
    let sys_vol = system_volume.clamp(0.0, 2.0);
    let mic_vol = mic_volume.clamp(0.0, 2.0);
    match (system_wav, mic_wav) {
        (Some(_), Some(_)) => {
            let filter = format!(
                "[1:a]volume={:.2},aresample=48000,aformat=channel_layouts=stereo[a1];[2:a]volume={:.2},aresample=48000,aformat=channel_layouts=stereo[a2];[a1][a2]amix=inputs=2:duration=longest:dropout_transition=0:normalize=0[aout]",
                sys_vol, mic_vol
            );
            cmd.args([
                "-filter_complex",
                &filter,
                "-map",
                "0:v",
                "-map",
                "[aout]",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-shortest",
                "-movflags",
                "+faststart",
            ]);
        }
        _ => {
            let vol = if system_wav.is_some() { sys_vol } else { mic_vol };
            let filter = format!("volume={:.2},aresample=48000,aformat=channel_layouts=stereo", vol);
            cmd.args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-af",
                &filter,
                "-shortest",
                "-movflags",
                "+faststart",
            ]);
        }
    }
    cmd.arg(final_output);
    let out = cmd.output()?;
    if !out.status.success() {
        let log = String::from_utf8_lossy(&out.stderr);
        let tail = if log.len() > 4000 {
            format!("...[truncated]\n{}", &log[log.len() - 4000..])
        } else {
            log.to_string()
        };
        anyhow::bail!("ffmpeg mux failed with {}\n{tail}", out.status);
    }
    Ok(())
}

/// Peak-per-bucket waveform (0..1) mixed across one or more wav files.
/// Buckets are uniform at `rate_per_sec` across `total_secs`.
pub fn waveform_from_wavs(paths: &[PathBuf], rate_per_sec: f64, total_secs: f64) -> Vec<f32> {
    if paths.is_empty() || rate_per_sec <= 0.0 || total_secs <= 0.0 {
        return Vec::new();
    }
    let n = (total_secs * rate_per_sec).ceil().max(1.0) as usize;
    let mut peaks = vec![0.0f32; n];
    for path in paths {
        let Ok(reader) = hound::WavReader::open(path) else {
            continue;
        };
        let spec = reader.spec();
        let sr = spec.sample_rate.max(1) as f64;
        let ch = spec.channels.max(1) as usize;
        let per_bucket = (sr / rate_per_sec).max(1.0) as usize;
        let mut bucket = 0usize;
        let mut count = 0usize;
        let mut peak = 0.0f32;
        let flush = |peaks: &mut Vec<f32>, bucket: usize, peak: f32| {
            if bucket < peaks.len() {
                peaks[bucket] = peaks[bucket].max(peak);
            }
        };
        match spec.sample_format {
            hound::SampleFormat::Float => {
                for (i, s) in reader.into_samples::<f32>().enumerate() {
                    let v = s.unwrap_or(0.0).abs();
                    if v > peak {
                        peak = v;
                    }
                    if (i + 1) % (per_bucket * ch) == 0 {
                        flush(&mut peaks, bucket, peak);
                        bucket += 1;
                        peak = 0.0;
                        count = 0;
                        if bucket >= n {
                            break;
                        }
                    } else {
                        count += 1;
                    }
                }
            }
            hound::SampleFormat::Int => {
                let scale = match spec.bits_per_sample {
                    8 => 128.0,
                    16 => 32768.0,
                    24 => 8_388_608.0,
                    _ => 2_147_483_648.0,
                };
                for (i, s) in reader.into_samples::<i32>().enumerate() {
                    let v = (s.unwrap_or(0) as f32 / scale).abs();
                    if v > peak {
                        peak = v;
                    }
                    if (i + 1) % (per_bucket * ch) == 0 {
                        flush(&mut peaks, bucket, peak);
                        bucket += 1;
                        peak = 0.0;
                        if bucket >= n {
                            break;
                        }
                    } else {
                        count += 1;
                    }
                }
            }
        }
        let _ = count;
        // Trailing partial bucket.
        if bucket < n && peak > 0.0 {
            flush(&mut peaks, bucket, peak);
        }
    }
    // Perceptual (dB) curve so quiet speech stays visible but the noise
    // floor / digital silence still reads ~0. Linear peaks like 0.005 would
    // render as a sub-pixel line ("no wave"); mapped they read ~0.08.
    for v in peaks.iter_mut() {
        *v = display_level(*v);
    }
    peaks
}

/// Peak absolute amplitude (0..1-ish) of a wav file, across all channels.
/// Used to tell "mic captured silence" apart from "mic missing".
pub fn wav_peak(p: &Path) -> Option<f32> {
    let reader = hound::WavReader::open(p).ok()?;
    let spec = reader.spec();
    let scale = match spec.bits_per_sample {
        8 => 128.0,
        16 => 32768.0,
        24 => 8_388_608.0,
        _ => 2_147_483_648.0,
    };
    let mut peak = 0.0f32;
    if spec.sample_format == hound::SampleFormat::Float {
        for s in reader.into_samples::<f32>() {
            let v = s.unwrap_or(0.0).abs();
            if v > peak {
                peak = v;
            }
        }
    } else {
        for s in reader.into_samples::<i32>() {
            let v = (s.unwrap_or(0) as f32 / scale).abs();
            if v > peak {
                peak = v;
            }
        }
    }
    Some(peak)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_audio_devices_without_panic() {
        let devs = list_audio_devices();
        eprintln!(
            "mics: {:?} | speakers: {:?} | default mic: {:?}",
            devs.microphones, devs.speakers, devs.default_mic
        );
    }

    /// Live diagnostic (ignored by default): captures 3s from the default mic
    /// and reports the meter + wav peak. Run manually:
    /// `cargo test diag_mic_capture -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn diag_mic_capture_live() {
        let dir = std::env::temp_dir().join("pipit_mic_diag");
        let _ = std::fs::remove_dir_all(&dir);
        let cap = AudioCapture::start(false, true, None, &dir);
        assert!(cap.is_active(), "mic thread did not start");
        let mut peak_level = 0.0f32;
        let mut last_level = 0.0f32;
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            last_level = cap.mic_level();
            peak_level = peak_level.max(last_level);
        }
        let res = cap.stop();
        eprintln!("meter peak over 3s: {peak_level:.3} (last {last_level:.3})");
        eprintln!("errors: {:?}", res.errors);
        eprintln!("mic_wav: {:?}", res.mic_wav);
        if let Some(p) = &res.mic_wav {
            let bytes = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            eprintln!(
                "wav bytes: {bytes}, wav peak: {:?}",
                wav_peak(p).map(|v| format!("{v:.4}"))
            );
        }
    }

    #[test]
    fn mux_single_silence_wav_into_video() {
        let dir = std::env::temp_dir().join("pipit_audio_mux_test");
        let _ = std::fs::create_dir_all(&dir);
        let wav = dir.join("silence.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for _ in 0..48000 {
            w.write_sample(0.0f32).unwrap();
            w.write_sample(0.0f32).unwrap();
        }
        w.finalize().unwrap();

        let video = dir.join("video.mp4");
        let mut enc = crate::encoder::VideoEncoder::new();
        enc.start(&video, 160, 120, 30).unwrap();
        for f in 0..30 {
            let mut buf = vec![0u8; 160 * 120 * 4];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((i + f) % 256) as u8;
            }
            enc.send_frame(buf, 160, 120, f as i64).unwrap();
        }
        enc.stop().unwrap();

        let out = dir.join("final.mp4");
        let _ = std::fs::remove_file(&out);
        let ffmpeg = crate::encoder::get_ffmpeg_path();
        mux_audio_into_video(&ffmpeg, &video, &out, Some(&wav), None, 1.0, 1.0).unwrap();
        let meta = std::fs::metadata(&out).unwrap();
        assert!(meta.len() > 1024, "muxed file too small: {}", meta.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_level_maps_quiet_signals_visibly() {
        assert_eq!(display_level(0.0), 0.0);
        assert_eq!(display_level(0.00001), 0.0); // below the -50 dB floor
        // A quiet far-field mic (~0.005 peak) must render as a visible bar,
        // not the sub-pixel line linear scaling would give.
        let quiet = display_level(0.005);
        assert!(quiet > 0.05, "quiet signal should be visible, got {quiet}");
        assert!(display_level(0.05) > quiet);
        assert_eq!(display_level(1.0), 1.0);
    }

    #[test]
    fn wav_peak_separates_tone_from_silence() {
        let dir = std::env::temp_dir().join("pipit_wav_peak_test");
        let _ = std::fs::create_dir_all(&dir);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let tone = dir.join("tone.wav");
        let mut w = hound::WavWriter::create(&tone, spec).unwrap();
        for i in 0..4800 {
            let t = i as f32 / 48000.0;
            w.write_sample((2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5)
                .unwrap();
        }
        w.finalize().unwrap();
        let peak = wav_peak(&tone).unwrap();
        assert!((peak - 0.5).abs() < 0.01, "tone peak ~0.5, got {peak}");

        let silence = dir.join("silence.wav");
        let mut w = hound::WavWriter::create(&silence, spec).unwrap();
        for _ in 0..4800 {
            w.write_sample(0.0f32).unwrap();
        }
        w.finalize().unwrap();
        assert_eq!(wav_peak(&silence).unwrap(), 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mux_dual_mono_mic_plus_stereo_system() {
        // Regression test for the real recording layout: stereo system
        // loopback + MONO mic mixed via amix. A mono+stereo mix used to be
        // untested; if the mic track is dropped here it is dropped in-app.
        let dir = std::env::temp_dir().join("pipit_audio_mux_dual_test");
        let _ = std::fs::create_dir_all(&dir);
        let sr = 48_000u32;
        let secs = 1u32;
        let n = (sr * secs) as usize;

        // Stereo system tone (440 Hz).
        let system_wav = dir.join("system.wav");
        let spec_stereo = hound::WavSpec {
            channels: 2,
            sample_rate: sr,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&system_wav, spec_stereo).unwrap();
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();

        // Mono mic tone (880 Hz) — matches the in-app mic capture format.
        let mic_wav = dir.join("mic.wav");
        let spec_mono = hound::WavSpec {
            channels: 1,
            sample_rate: sr,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&mic_wav, spec_mono).unwrap();
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let s = (2.0 * std::f32::consts::PI * 880.0 * t).sin() * 0.5;
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();

        let video = dir.join("video.mp4");
        let mut enc = crate::encoder::VideoEncoder::new();
        enc.start(&video, 160, 120, 30).unwrap();
        for f in 0..30 {
            let mut buf = vec![0u8; 160 * 120 * 4];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((i + f) % 256) as u8;
            }
            enc.send_frame(buf, 160, 120, f as i64).unwrap();
        }
        enc.stop().unwrap();
        let video_len = std::fs::metadata(&video).unwrap().len();

        let out = dir.join("final.mp4");
        let _ = std::fs::remove_file(&out);
        let ffmpeg = crate::encoder::get_ffmpeg_path();
        mux_audio_into_video(
            &ffmpeg,
            &video,
            &out,
            Some(&system_wav),
            Some(&mic_wav),
            1.0,
            1.0,
        )
        .unwrap();
        let meta = std::fs::metadata(&out).unwrap();
        assert!(meta.len() > 1024, "muxed file too small: {}", meta.len());
        // Mixed AAC audio must add payload on top of the video-only input.
        assert!(
            meta.len() > video_len,
            "muxed file ({}) not larger than video-only input ({video_len}) — audio track missing?",
            meta.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
