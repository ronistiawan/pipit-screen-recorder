use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use chrono::Local;

use crate::audio::{AudioCapture, AudioDevices, AudioResult};
use crate::capture::{ScreenCapture, FrameData, MonitorInfo, WindowInfo};
use crate::encoder::VideoEncoder;
use crate::timeline::{Timeline, TimelineState};
use crate::theme;
use crate::ui::format_duration;
use egui::Color32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    Idle,
    SelectingArea,
    Recording,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Monitor,
    Window,
    Region,
}

#[derive(Debug, Clone)]
enum SelectorPhase {
    /// App window parked off-screen; waiting a beat so the frozen screenshot
    /// doesn't contain our own window.
    Hiding { since: Instant },
    /// Fullscreen overlay armed over the frozen screenshot.
    Armed,
}

#[derive(Debug, Clone)]
struct SelectorBg {
    width: u32,
    height: u32,
}

/// Result of a background sharp-still decode for the scrub preview.
struct StillResult {
    timeline_t: f64,
    gen: u64,
    frame: Option<(Vec<u8>, u32, u32)>,
}

pub struct ScreenRecorderApp {
    pub capture: ScreenCapture,
    pub encoder: VideoEncoder,
    pub timeline: Timeline,
    pub timeline_state: TimelineState,

    pub state: RecordingState,
    pub capture_mode: CaptureMode,

    pub selected_monitor: Option<usize>,
    pub selected_window: Option<String>,
    pub capture_region: Option<(i32, i32, u32, u32)>,

    pub area_selector: Option<crate::ui::AreaSelector>,
    selector_phase: Option<SelectorPhase>,
    selector_bg: Option<SelectorBg>,
    selector_tex: Option<egui::TextureHandle>,
    prev_outer_pos: Option<egui::Pos2>,

    pub recording_start_time: Option<Instant>,
    pub paused_duration: Duration,
    pub current_frame_count: u64,
    pub target_fps: u32,

    pub output_path: PathBuf,
    /// Folder new takes are written to (user-selectable, remembered).
    pub output_dir: PathBuf,

    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<WindowInfo>,

    pub show_settings: bool,
    pub show_area_selector: bool,

    pub error_message: Option<String>,
    /// `(message, first_shown)` — lets stale errors auto-dismiss instead of
    /// sitting in the status bar forever.
    error_seen: Option<(String, Instant)>,
    pub status_message: Option<String>,

    // ---- audio ----
    pub record_system_audio: bool,
    pub record_mic: bool,
    pub mic_device: Option<String>,
    pub system_volume: f32,
    pub mic_volume: f32,
    pub audio_devices: AudioDevices,
    audio_capture: Option<AudioCapture>,
    session_dir: Option<PathBuf>,
    video_only_path: Option<PathBuf>,
    last_audio_result: Option<AudioSummary>,
    /// Recent `[system, mic]` levels (~20/s) for the recording controller's
    /// live wave — mic input visibility while the main window is minimized.
    audio_hist: VecDeque<[f32; 2]>,
    audio_hist_last: Option<Instant>,

    // ---- preview ----
    last_frame: Option<FrameData>,
    preview_texture: Option<egui::TextureHandle>,
    preview_size: Option<(u32, u32)>,
    last_preview_update: Option<Instant>,
    // ---- scrub stills (sharp full-res frame decoded on demand) ----
    /// (timeline_t, media generation, aspect, texture)
    still_tex: Option<(f64, u64, f32, egui::TextureHandle)>,
    still_rx: Option<std::sync::mpsc::Receiver<StillResult>>,
    still_pending: Option<(f64, u64)>,
    still_probe: Option<(f64, Instant)>,
    /// Bumped whenever timeline media changes; stills tagged with a stale
    /// generation are ignored.
    media_gen: u64,

    // ---- editor tracks (filmstrip + waveform) ----
    pub filmstrip: Vec<(f64, egui::ColorImage)>,
    thumb_textures: Vec<egui::TextureHandle>,
    last_thumb_time: f64,
    pub waveform: Vec<f32>,
    pub waveform_rate: f64,
    // ---- playback ----
    playback_started: Option<Instant>,
    playback_from: f64,
    /// Timeline second playback stops at (a selected range plays only itself).
    playback_end: Option<f64>,
    /// Background frame stream feeding the preview while playing.
    player: Option<crate::player::PreviewPlayer>,
    /// Background audio stream feeding the preview while playing.
    audio_player: Option<crate::audioplay::PreviewAudio>,
    /// Newest streamed frame: (timeline_t, media_gen, aspect, texture).
    play_tex: Option<(f64, u64, f32, egui::TextureHandle)>,
    /// On-screen width the preview is drawn at (what to decode for).
    preview_wanted_w: u32,
    /// Decode width of the current player session.
    preview_decode_w: u32,
    /// When the current player session started (resize-retarget cooldown).
    player_started: Option<Instant>,
    /// Preview zoom: 1.0 = fit, >1 = magnified (scrollable).
    preview_zoom: f32,
    // ---- audio effects applied on Save ----
    pub fade_in: f64,
    pub fade_out: f64,
    pub master_volume: f32,
    show_volume_popup: bool,
    // Editor dock (timeline) is hidden until the user asks to edit.
    pub show_editor: bool,
    // Edge detection for the global ESC-to-stop poll.
    esc_prev_down: bool,
}

/// Global (focus-independent) ESC state via Win32 GetAsyncKeyState.
/// High bit set = key currently down.
#[cfg(target_os = "windows")]
fn esc_down_global() -> bool {
    // VK_ESCAPE = 0x1B
    unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x1B) as u16 & 0x8000 != 0 }
}

#[cfg(not(target_os = "windows"))]
fn esc_down_global() -> bool {
    false
}

/// Title of the floating recording controller (shared with the window
/// builder so the exclusion lookup can never drift from the real title).
const REC_CTRL_TITLE: &str = "Pipit — Recording";

/// Title of one of the 4 thin red marker edge windows.
fn edge_window_title(name: &str) -> String {
    format!("Pipit edge {name}")
}

/// Hide one of our top-level windows from screen capture via
/// `WDA_EXCLUDEFROMCAPTURE`: the window stays visible on the monitor, but
/// DWM/Windows.Graphics.Capture omits it from captured frames — which is
/// exactly what the red area marker and the recording controller need,
/// otherwise they land in the recorded video.
///
/// Returns `false` while the window does not exist yet (the viewport is
/// created a moment after the first frame); callers simply retry.
#[cfg(target_os = "windows")]
pub(crate) fn exclude_window_from_capture(title: &str) -> bool {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, SetWindowDisplayAffinity,
        WDA_EXCLUDEFROMCAPTURE,
    };

    /// Enum state: only our own process's windows are eligible, so a foreign
    /// window with a duplicate title can never swallow the lookup.
    struct Search {
        title: Vec<u16>,
        pid: u32,
        found: Option<HWND>,
    }

    unsafe extern "system" fn find_ours(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let s = &mut *(lparam.0 as *mut Search);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == s.pid {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
            if n == s.title.len() && buf[..n] == s.title[..] {
                s.found = Some(hwnd);
                return BOOL(0); // found it — stop enumerating
            }
        }
        BOOL(1)
    }

    let mut search = Search {
        title: title.encode_utf16().collect(),
        pid: std::process::id(),
        found: None,
    };
    unsafe {
        let _ = EnumWindows(
            Some(find_ours),
            LPARAM(&mut search as *mut Search as isize),
        );
    }
    match search.found {
        Some(hwnd) => unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok() },
        // Native window not created yet — caller retries on the next frame.
        None => false,
    }
}

#[cfg(not(target_os = "windows"))]
fn exclude_window_from_capture(_title: &str) -> bool {
    true
}

#[derive(Debug, Clone, Default)]
pub struct AudioSummary {
    pub has_system: bool,
    pub has_mic: bool,
    pub warnings: Vec<String>,
}

fn default_output_dir() -> PathBuf {
    dirs::video_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap())
        .join("Pipit Screen Recorder")
}

fn output_dir_config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("Pipit Screen Recorder").join("output_dir.txt"))
}

/// Config path used by older FreeCam3 builds (checked as a fallback so an
/// upgrade keeps the previously chosen save folder).
fn legacy_output_dir_config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("FreeCam3").join("output_dir.txt"))
}

fn load_saved_output_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(file) = output_dir_config_file() {
        candidates.push(file);
    }
    if let Some(file) = legacy_output_dir_config_file() {
        candidates.push(file);
    }
    for file in candidates {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let dir = PathBuf::from(text.trim());
        if dir.as_os_str().is_empty() {
            continue;
        }
        // Only reuse it if it still exists (or can be recreated).
        if dir.exists() || std::fs::create_dir_all(&dir).is_ok() {
            return Some(dir);
        }
    }
    None
}

fn save_output_dir(dir: &Path) {
    if let Some(file) = output_dir_config_file() {
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(file, dir.to_string_lossy().as_bytes());
    }
}

impl ScreenRecorderApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let output_dir = load_saved_output_dir().unwrap_or_else(default_output_dir);
        std::fs::create_dir_all(&output_dir).ok();

        let monitors = crate::capture::get_monitors().unwrap_or_default();
        let windows = crate::capture::get_windows().unwrap_or_default();
        let audio_devices = crate::audio::list_audio_devices();
        let default_mic = audio_devices.default_mic.clone();

        Self {
            capture: ScreenCapture::new(),
            encoder: VideoEncoder::new(),
            timeline: Timeline::new(30, 1920, 1080),
            timeline_state: TimelineState::new(),
            state: RecordingState::Idle,
            capture_mode: if monitors.is_empty() { CaptureMode::Region } else { CaptureMode::Monitor },
            selected_monitor: if monitors.is_empty() { None } else { Some(0) },
            selected_window: None,
            capture_region: None,
            area_selector: None,
            selector_phase: None,
            selector_bg: None,
            selector_tex: None,
            prev_outer_pos: None,
            recording_start_time: None,
            paused_duration: Duration::ZERO,
            current_frame_count: 0,
            target_fps: 30,
            output_path: output_dir.join(format!("recording_{}.mp4", Local::now().format("%Y%m%d_%H%M%S"))),
            output_dir,
            monitors,
            windows,
            show_settings: false,
            show_area_selector: false,
            error_message: None,
            error_seen: None,
            status_message: Some("Ready — pick a source, then press Record.".to_string()),
            record_system_audio: true,
            record_mic: true,
            mic_device: default_mic,
            system_volume: 1.5,
            mic_volume: 1.5,
            audio_devices,
            audio_capture: None,
            session_dir: None,
            video_only_path: None,
            last_audio_result: None,
            audio_hist: VecDeque::new(),
            audio_hist_last: None,
            last_frame: None,
            preview_texture: None,
            preview_size: None,
            last_preview_update: None,
            still_tex: None,
            still_rx: None,
            still_pending: None,
            still_probe: None,
            media_gen: 0,
            filmstrip: Vec::new(),
            thumb_textures: Vec::new(),
            last_thumb_time: 0.0,
            waveform: Vec::new(),
            waveform_rate: 20.0,
            playback_started: None,
            playback_from: 0.0,
            playback_end: None,
            player: None,
            audio_player: None,
            play_tex: None,
            preview_wanted_w: 0,
            preview_decode_w: 0,
            player_started: None,
            preview_zoom: 1.0,
            fade_in: 0.0,
            fade_out: 0.0,
            master_volume: 1.0,
            show_volume_popup: false,
            show_editor: false,
            esc_prev_down: false,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.state == RecordingState::Recording || self.state == RecordingState::Paused
    }

    fn ensure_preview_capture(&mut self) {
        if self.capture.is_capturing() || self.is_recording() {
            return;
        }
        let (width, height) = match self.capture_mode {
            CaptureMode::Monitor => {
                if let Some(idx) = self.selected_monitor {
                    if let Some(m) = self.monitors.get(idx) {
                        (m.width, m.height)
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            }
            CaptureMode::Window => {
                if let Some(title) = &self.selected_window {
                    if let Some(w) = self.windows.iter().find(|w| &w.title == title) {
                        (w.width.max(64), w.height.max(64))
                    } else {
                        return;
                    }
                } else {
                    return;
                }
            }
            CaptureMode::Region => {
                if let Some((_, _, w, h)) = self.capture_region {
                    (w.max(64), h.max(64))
                } else {
                    return;
                }
            }
        };
        let _ = self.start_preview_capture(width, height);
    }

    fn start_preview_capture(&mut self, _width: u32, _height: u32) -> Result<(), String> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| format!("tokio: {e}"))?;
        let result = match self.capture_mode {
            CaptureMode::Monitor => {
                rt.block_on(self.capture.start_monitor_capture(self.selected_monitor.unwrap()))
            }
            CaptureMode::Window => {
                rt.block_on(self.capture.start_window_capture(self.selected_window.as_ref().unwrap()))
            }
            CaptureMode::Region => {
                let (x, y, w, h) = self.capture_region.unwrap();
                rt.block_on(self.capture.start_region_capture(x, y, w, h))
            }
        };
        result.map_err(|e| format!("Failed to start preview capture: {}", e))
    }

    /// Live wall-clock duration of the current take (or the finished timeline).
    pub fn live_duration_secs(&self) -> f64 {
        if self.is_recording() {
            self.get_recording_duration().as_secs_f64()
        } else {
            self.timeline.total_duration
        }
    }

    pub fn start_recording(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let (width, height) = match self.capture_mode {
            CaptureMode::Monitor => {
                if let Some(idx) = self.selected_monitor {
                    if let Some(m) = self.monitors.get(idx) {
                        (m.width, m.height)
                    } else {
                        return Err("Invalid monitor selected".to_string());
                    }
                } else {
                    return Err("Pick a monitor first (or use Region / Window).".to_string());
                }
            }
            CaptureMode::Window => {
                if let Some(title) = &self.selected_window {
                    if let Some(w) = self.windows.iter().find(|w| &w.title == title) {
                        (w.width.max(64), w.height.max(64))
                    } else {
                        return Err("Window not found — press Refresh and re-select.".to_string());
                    }
                } else {
                    return Err("No window selected".to_string());
                }
            }
            CaptureMode::Region => {
                if let Some((_, _, w, h)) = self.capture_region {
                    (w.max(64), h.max(64))
                } else {
                    return Err("No region selected — click “Select Area”.".to_string());
                }
            }
        };

        self.timeline = Timeline::new(self.target_fps, width, height);
        self.timeline_state = TimelineState::new();
        self.bump_media();
        self.current_frame_count = 0;
        self.last_frame = None;
        self.preview_texture = None;
        self.preview_size = None;
        self.filmstrip.clear();
        self.thumb_textures.clear();
        self.last_thumb_time = 0.0;
        self.waveform.clear();
        self.playback_started = None;
        self.playback_from = 0.0;
        self.playback_end = None;
        self.fade_in = 0.0;
        self.fade_out = 0.0;
        self.preview_zoom = 1.0;
        self.paused_duration = Duration::ZERO;
        self.audio_hist.clear();
        self.audio_hist_last = None;
        self.error_message = None;
        // A new take hides the editor; it reappears only via Edit video.
        self.show_editor = false;
        self.stop_playback();
        // Stop any preview capture before starting the recording capture.
        self.capture.stop();

        let timestamp = Local::now().format("%Y%m%d_%H%M%S");
        let out_dir = self.output_dir.clone();
        let _ = std::fs::create_dir_all(&out_dir);
        self.output_path = out_dir.join(format!("recording_{}.mp4", timestamp));

        // Temp session: video-only mp4 + wavs live here until muxed.
        let session = std::env::temp_dir()
            .join("pipit-screen-recorder")
            .join(format!("session_{}", timestamp));
        let _ = std::fs::create_dir_all(&session);
        let video_only = session.join("video.mp4");
        let _ = std::fs::remove_file(&video_only);
        self.session_dir = Some(session.clone());
        self.video_only_path = Some(video_only.clone());

        self.encoder.start(&video_only, width, height, self.target_fps)
            .map_err(|e| format!("Failed to start encoder: {}", e))?;

        // Audio runs alongside video; failures degrade to video-only with a warning.
        if self.record_system_audio || self.record_mic {
            let cap = AudioCapture::start(
                self.record_system_audio,
                self.record_mic,
                self.mic_device.clone(),
                &session,
            );
            if cap.is_active() {
                self.audio_capture = Some(cap);
            } else {
                self.last_audio_result = Some(AudioSummary {
                    has_system: false,
                    has_mic: false,
                    warnings: vec!["No audio device found — recording video only.".into()],
                });
            }
        } else {
            self.audio_capture = None;
        }

        let start_video = |mode: CaptureMode,
                           mon: Option<usize>,
                           win: Option<String>,
                           reg: Option<(i32, i32, u32, u32)>| {
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| format!("tokio: {e}"))?;
            match mode {
                CaptureMode::Monitor => {
                    rt.block_on(self.capture.start_monitor_capture(mon.unwrap()))
                }
                CaptureMode::Window => {
                    rt.block_on(self.capture.start_window_capture(&win.unwrap()))
                }
                CaptureMode::Region => {
                    let (x, y, w, h) = reg.unwrap();
                    rt.block_on(self.capture.start_region_capture(x, y, w, h))
                }
            }
            .map_err(|e| format!("Failed to start capture: {}", e))
        };

        if let Err(e) = start_video(
            self.capture_mode,
            self.selected_monitor,
            self.selected_window.clone(),
            self.capture_region,
        ) {
            // Roll back partial audio/encoder state.
            if let Some(cap) = self.audio_capture.take() {
                let _ = cap.stop();
            }
            let _ = self.encoder.stop();
            self.session_dir = None;
            self.video_only_path = None;
            return Err(e);
        }

        self.recording_start_time = Some(Instant::now());
        self.state = RecordingState::Recording;
        let mut msg = "● Recording".to_string();
        if self.record_system_audio {
            msg.push_str(" + system sound");
        }
        if self.record_mic {
            msg.push_str(" + mic");
        }
        msg.push_str(" — press Stop when done.");
        self.status_message = Some(msg);

        // Minimize the main window so it stays out of the capture; the
        // floating mini-controller + area marker stay visible instead.
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        // Seed ESC edge detection so a held-down key doesn't instantly stop.
        self.esc_prev_down = esc_down_global();

        Ok(())
    }

    /// Short label for the current capture source (mini-controller + overlay).
    pub fn recording_source_label(&self) -> String {
        match self.capture_mode {
            CaptureMode::Monitor => {
                if let Some(idx) = self.selected_monitor {
                    if let Some(m) = self.monitors.get(idx) {
                        return format!("{} ({}×{})", m.name, m.width, m.height);
                    }
                }
                "Monitor".to_string()
            }
            CaptureMode::Window => self
                .selected_window
                .clone()
                .unwrap_or_else(|| "Window".to_string()),
            CaptureMode::Region => {
                if let Some((x, y, w, h)) = self.capture_region {
                    format!("Region {}×{} @ ({},{})", w, h, x, y)
                } else {
                    "Region".to_string()
                }
            }
        }
    }

    /// Screen-pixel rect of the area being recorded (for the border marker).
    /// Region/monitor are relative to the primary monitor at (0,0).
    /// Window captures have no reliable screen position -> None (no marker).
    fn recording_screen_rect(&self) -> Option<(i32, i32, u32, u32)> {
        match self.capture_mode {
            CaptureMode::Region => self.capture_region,
            CaptureMode::Monitor => {
                let idx = self.selected_monitor?;
                let m = self.monitors.get(idx)?;
                Some((0, 0, m.width.max(64), m.height.max(64)))
            }
            CaptureMode::Window => None,
        }
    }

    /// Floating windows shown while recording (main window is minimized):
    /// a small Start/Pause/Continue/Stop controller + a rectangle marker
    /// showing which area is being recorded.
    ///
    /// The marker is 4 thin opaque always-on-top edge windows (no fullscreen
    /// transparent overlay, which rendered as an opaque black cover on some
    /// GPUs). ESC in any of our windows, or a global ESC poll, stops the take.
    fn show_recording_floaters(&mut self, ctx: &egui::Context) {
        let is_paused = self.state == RecordingState::Paused;
        let dur_str = crate::ui::format_duration(self.get_recording_duration());
        let src_label = self.recording_source_label();

        // Sample levels ~20/s for the controller's live wave (the main
        // window's meters are hidden while it is minimized).
        let (sys_lvl, mic_lvl) = match &self.audio_capture {
            Some(cap) => (cap.system_level(), cap.mic_level()),
            None => (0.0, 0.0),
        };
        let now = Instant::now();
        if self
            .audio_hist_last
            .map(|t| now.duration_since(t) >= Duration::from_millis(50))
            .unwrap_or(true)
        {
            self.audio_hist_last = Some(now);
            self.audio_hist.push_back([sys_lvl, mic_lvl]);
            while self.audio_hist.len() > 120 {
                self.audio_hist.pop_front();
            }
        }
        let hist: Vec<[f32; 2]> = self.audio_hist.iter().copied().collect();
        let rec_sys = self.record_system_audio;
        let rec_mic = self.record_mic;

        // ---- 1) mini controller: only Start/Pause/Continue/Stop ----
        // 0 = none, 1 = pause, 2 = resume, 3 = stop
        let mut action: u8 = 0;
        let ctrl_id = egui::ViewportId::from_hash_of("pipit_rec_controls");
        let mut ctrl_builder = egui::ViewportBuilder::default()
            .with_title(REC_CTRL_TITLE)
            .with_inner_size([360.0, 158.0])
            .with_min_inner_size([320.0, 120.0])
            .with_resizable(false)
            .with_always_on_top()
            .with_taskbar(true);
        if let Some(app_icon) = crate::icon::app_icon() {
            ctrl_builder = ctrl_builder.with_icon(app_icon);
        }
        ctx.show_viewport_immediate(ctrl_id, ctrl_builder, |vctx, _class| {
            // Keep the controller out of the recorded frames (retries until
            // the native window exists).
            let _ = exclude_window_from_capture(REC_CTRL_TITLE);
            // ESC while the controller is focused stops the take.
            if vctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                action = 3;
            }
            egui::CentralPanel::default().show(vctx, |ui| {
                ui.horizontal(|ui| {
                    if is_paused {
                        ui.label(
                            egui::RichText::new("⏸ PAUSED")
                                .color(theme::PAUSED)
                                .strong(),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("● REC")
                                .color(theme::REC_SOFT)
                                .strong(),
                        );
                    }
                    ui.monospace(&dur_str);
                });
                ui.label(egui::RichText::new(theme::truncate(&src_label, 46)).color(theme::TEXT_DIM))
                    .on_hover_text(&src_label);
                // Live audio wave: system (blue, under) + mic (green, over),
                // ~6s of history — proof the mic is hearing you while the
                // main window (with its meters) is minimized.
                ui.add_space(3.0);
                let (rect, wave_resp) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 42.0),
                    egui::Sense::hover(),
                );
                wave_resp.on_hover_text(format!(
                    "System {:.0}%  •  Mic {:.0}%\nBlue = system, green = microphone",
                    sys_lvl.clamp(0.0, 1.0) * 100.0,
                    mic_lvl.clamp(0.0, 1.0) * 100.0,
                ));
                let painter = ui.painter();
                painter.rect_filled(rect, 4.0, Color32::from_rgb(18, 22, 30));
                let n = hist.len();
                if n >= 2 && (rec_sys || rec_mic) {
                    let line = |ch: usize, color: Color32| {
                        let mut pts = Vec::with_capacity(n);
                        for (i, s) in hist.iter().enumerate() {
                            let x = rect.left() + rect.width() * i as f32 / (n - 1) as f32;
                            let y = rect.bottom() - rect.height() * s[ch].clamp(0.0, 1.0);
                            pts.push(egui::pos2(x, y));
                        }
                        egui::Shape::line(pts, egui::Stroke::new(1.5_f32, color))
                    };
                    if rec_sys {
                        painter.add(line(0, Color32::from_rgb(96, 165, 250)));
                    }
                    if rec_mic {
                        painter.add(line(1, Color32::from_rgb(52, 211, 153)));
                    }
                }
                let small = egui::FontId::proportional(10.0);
                if rec_mic {
                    painter.text(
                        rect.left_top() + egui::vec2(6.0, 3.0),
                        egui::Align2::LEFT_TOP,
                        "● mic",
                        small.clone(),
                        Color32::from_rgb(52, 211, 153),
                    );
                }
                if rec_sys {
                    painter.text(
                        rect.right_top() + egui::vec2(-6.0, 3.0),
                        egui::Align2::RIGHT_TOP,
                        "● system",
                        small.clone(),
                        Color32::from_rgb(96, 165, 250),
                    );
                }
                if !rec_mic && !rec_sys {
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "audio off — video only",
                        small.clone(),
                        Color32::from_rgb(148, 163, 184),
                    );
                } else if rec_mic && n >= 60 {
                    // After ~3s, a flat mic line means Windows/the device is
                    // muted or the wrong input is picked — say so.
                    let mic_peak = hist.iter().map(|s| s[1]).fold(0.0f32, f32::max);
                    if mic_peak < 0.02 {
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "no mic signal — speak to test",
                            small.clone(),
                            Color32::from_rgb(255, 120, 120),
                        );
                    }
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if is_paused {
                        // "Resume" continues the same take.
                        let resume = theme::resume_btn("▶ Resume");
                        if ui.add_sized([110.0, theme::BTN_H], resume).clicked() {
                            action = 2;
                        }
                    } else {
                        if ui
                            .add_sized([90.0, theme::BTN_H], egui::Button::new("⏸ Pause"))
                            .clicked()
                        {
                            action = 1;
                        }
                    }
                    let stop = theme::record_btn("■ Stop");
                    if ui.add_sized([90.0, theme::BTN_H], stop).clicked() {
                        action = 3;
                    }
                    ui.label(
                        egui::RichText::new("Esc stops")
                            .color(theme::TEXT_DIM)
                            .small(),
                    );
                });
            });
            // Keep timers/labels ticking while minimized; 50ms keeps the
            // mic wave visibly scrolling.
            vctx.request_repaint_after(Duration::from_millis(50));
        });
        match action {
            1 => self.pause_recording(),
            2 => self.resume_recording(),
            3 => {
                if let Err(e) = self.stop_recording(ctx) {
                    self.error_message = Some(e);
                }
            }
            _ => {}
        }

        // ---- 2) rectangle mark: 4 thin opaque edge windows ----
        // No fullscreen transparent overlay: on some GPUs it composites as an
        // opaque black cover over the whole screen. The thin red bars do sit
        // on the captured edges, so each one is additionally hidden from the
        // capture itself (WDA_EXCLUDEFROMCAPTURE) — visible on the monitor,
        // never part of the recorded video.
        if self.is_recording() {
            if let Some((rx, ry, rw, rh)) = self.recording_screen_rect() {
                // Viewport position/size are logical points; convert from
                // screen pixels with the main viewport's scale.
                let ppp = ctx.pixels_per_point().max(1.0);
                let t = 4.0_f32; // edge thickness in points
                let x = rx as f32 / ppp;
                let y = ry as f32 / ppp;
                let w = rw as f32 / ppp;
                let h = rh as f32 / ppp;
                if w > t * 2.0 && h > t * 2.0 {
                    let edges: [(&str, f32, f32, f32, f32); 4] = [
                        ("top", x, y, w, t),
                        ("bottom", x, y + h - t, w, t),
                        ("left", x, y, t, h),
                        ("right", x + w - t, y, t, h),
                    ];
                    for (name, ex, ey, ew, eh) in edges {
                        let id = egui::ViewportId::from_hash_of(format!(
                            "pipit_rec_edge_{name}"
                        ));
                        let title = edge_window_title(name);
                        let builder = egui::ViewportBuilder::default()
                            .with_title(title.clone())
                            .with_position([ex, ey])
                            .with_inner_size([ew, eh])
                            .with_decorations(false)
                            .with_resizable(false)
                            .with_active(false)
                            .with_always_on_top()
                            .with_taskbar(false)
                            .with_mouse_passthrough(true);
                        ctx.show_viewport_immediate(id, builder, |vctx, _class| {
                            egui::CentralPanel::default()
                                .frame(
                                    egui::Frame::none()
                                        .fill(Color32::from_rgb(229, 57, 53)),
                                )
                                .show(vctx, |_ui| {});
                        });
                        // Keep the bar out of the recorded frames; retried
                        // every repaint until the native window exists.
                        let _ = exclude_window_from_capture(&title);
                    }
                }
            }
            let _ = (src_label, dur_str, is_paused);
        }
    }

    pub fn pause_recording(&mut self) {
        if self.state == RecordingState::Recording {
            self.state = RecordingState::Paused;
            if let Some(start) = self.recording_start_time {
                self.paused_duration += start.elapsed();
            }
            self.recording_start_time = None;
            self.status_message = Some("Paused — press Resume to continue.".to_string());
        }
    }

    pub fn resume_recording(&mut self) {
        if self.state == RecordingState::Paused {
            self.recording_start_time = Some(Instant::now());
            self.state = RecordingState::Recording;
            self.status_message = Some("● Recording…".to_string());
        }
    }

    pub fn stop_recording(&mut self, ctx: &egui::Context) -> Result<(), String> {
        self.capture.stop();
        self.encoder.stop().map_err(|e| format!("Failed to stop encoder: {}", e))?;

        // Finish audio, then mux into the final mp4.
        let audio: AudioResult = match self.audio_capture.take() {
            Some(cap) => cap.stop(),
            None => crate::audio::AudioResult {
                system_wav: None,
                mic_wav: None,
                errors: Vec::new(),
            },
        };
        let mut warnings = audio.errors.clone();
        // Per-source diagnosis so a missing mic isn't silently ignored.
        // (Silence-only loopback files are filtered inside stop().)
        if self.record_mic && audio.mic_wav.is_none() {
            warnings.push(
                "Mic requested but no mic audio captured — check the mic device, volume, and Windows mic privacy setting.".to_string(),
            );
        }
        if self.record_system_audio && audio.system_wav.is_none() {
            warnings.push(
                "No system sound captured (nothing was playing, or loopback unavailable).".to_string(),
            );
        }
        if (self.record_system_audio || self.record_mic) && !audio.has_audio() {
            warnings.push(
                "No audible audio was captured (silent / no device) — saved video only.".to_string(),
            );
        }
        // Log captured wav sizes for diagnosis (visible in the console).
        for (tag, opt) in [("system", &audio.system_wav), ("mic", &audio.mic_wav)] {
            match opt {
                Some(p) => {
                    let bytes = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                    eprintln!("[audio:{tag}] kept {} ({} bytes)", p.display(), bytes);
                }
                None => eprintln!("[audio:{tag}] no wav kept",),
            }
        }

        // Distinguish "track missing" from "track captured but silent": a muted
        // or very-low-gain mic still yields a large wav of near-zeros, which
        // used to look identical (flat waveform, quiet video) with no hint.
        if let Some(p) = &audio.mic_wav {
            let peak = crate::audio::wav_peak(p).unwrap_or(0.0);
            eprintln!("[audio:mic] peak amplitude: {peak:.4} ({:.1}%)", peak * 100.0);
            if peak < 0.02 {
                warnings.push(format!(
                    "Mic track is nearly silent (peak {:.0}%) — unmute the mic, raise Windows Sound → Input volume, move closer, or pick another mic, then ⟳.",
                    (peak * 100.0).max(0.0)
                ));
            }
        }
        if let Some(p) = &audio.system_wav {
            let peak = crate::audio::wav_peak(p).unwrap_or(0.0);
            eprintln!("[audio:system] peak amplitude: {peak:.4} ({:.1}%)", peak * 100.0);
            if peak < 0.005 {
                warnings.push(
                    "System-sound track is nearly silent — nothing audible was playing.".to_string(),
                );
            }
        }

        let duration = self.current_frame_count as f64 / self.target_fps as f64;
        let video_only = self
            .video_only_path
            .take()
            .unwrap_or_else(|| self.output_path.clone());

        // Rebuild an accurate waveform from the captured wavs BEFORE the
        // session dir is cleaned up (falls back to live levels if no audio).
        let wav_for_waveform: Vec<std::path::PathBuf> = [&audio.system_wav, &audio.mic_wav]
            .into_iter()
            .filter_map(|o: &Option<std::path::PathBuf>| o.clone())
            .collect();

        // Compute the waveform NOW: both branches below DELETE the session
        // wavs, so reading them afterwards always yielded an all-zero ("no
        // wave") result. total_duration doesn't include this take yet
        // (add_segment runs later), so add its length here.
        let new_waveform = if wav_for_waveform.is_empty() {
            Vec::new()
        } else {
            let total = self.timeline.total_duration + duration;
            crate::audio::waveform_from_wavs(&wav_for_waveform, self.waveform_rate, total.max(0.1))
        };

        if audio.has_audio() {
            let ffmpeg = crate::encoder::get_ffmpeg_path();
            match crate::audio::mux_audio_into_video(
                &ffmpeg,
                &video_only,
                &self.output_path,
                audio.system_wav.as_deref(),
                audio.mic_wav.as_deref(),
                self.system_volume,
                self.mic_volume,
            ) {
                Ok(()) => {
                    self.last_audio_result = Some(AudioSummary {
                        has_system: audio.system_wav.is_some(),
                        has_mic: audio.mic_wav.is_some(),
                        warnings: warnings.clone(),
                    });
                }
                Err(e) => {
                    // Fall back to the silent video so nothing is lost.
                    let _ = std::fs::copy(&video_only, &self.output_path);
                    warnings.push(format!("Audio mux failed ({e}) — saved video only."));
                    self.last_audio_result = Some(AudioSummary {
                        has_system: false,
                        has_mic: false,
                        warnings: warnings.clone(),
                    });
                }
            }
            // Clean temp session files.
            if let Some(dir) = self.session_dir.take() {
                let _ = std::fs::remove_file(dir.join("system.wav"));
                let _ = std::fs::remove_file(dir.join("mic.wav"));
                let _ = std::fs::remove_file(&video_only);
                let _ = std::fs::remove_dir(&dir);
            } else {
                let _ = std::fs::remove_file(&video_only);
            }
        } else {
            if video_only != self.output_path {
                let _ = std::fs::copy(&video_only, &self.output_path);
                let _ = std::fs::remove_file(&video_only);
            }
            if let Some(dir) = self.session_dir.take() {
                let _ = std::fs::remove_dir_all(&dir);
            }
            self.last_audio_result = Some(AudioSummary {
                has_system: false,
                has_mic: false,
                warnings: warnings.clone(),
            });
        }

        self.state = RecordingState::Idle;
        self.recording_start_time = None;

        self.timeline.add_segment(
            self.output_path.to_string_lossy().to_string(),
            0.0,
            duration.max(0.0),
        );
        self.bump_media();

        // Accurate waveform from captured audio (computed above, while the
        // session wavs still existed; replaces live meter samples).
        let total = self.timeline.total_duration.max(0.1);
        if !new_waveform.is_empty() {
            self.waveform = new_waveform;
        }
        // Pad/trim live waveform to the final duration.
        let want = (total * self.waveform_rate).ceil() as usize;
        if self.waveform.len() < want {
            let last = *self.waveform.last().unwrap_or(&0.0);
            self.waveform.resize(want, last);
        } else {
            self.waveform.truncate(want);
        }
        self.timeline_state.show_all();
        // Park at the end so the preview shows the sharp full-res still
        // (playback wraps to 0 when started from here).
        self.timeline_state.playhead = Some(self.timeline.total_duration);

        let mut msg = format!("Saved to: {}", self.output_path.display());
        if let Some(sum) = &self.last_audio_result {
            let mut tags = Vec::new();
            if sum.has_system {
                tags.push("system sound");
            }
            if sum.has_mic {
                tags.push("mic");
            }
            if tags.is_empty() {
                msg.push_str(" (video only)");
            } else {
                msg.push_str(&format!(" (with {})", tags.join(" + ")));
            }
            if !sum.warnings.is_empty() {
                msg.push_str(&format!(" — note: {}", sum.warnings.join("; ")));
            }
        }
        msg.push_str(" — press ✂ Edit video to edit.");
        self.status_message = Some(msg);
        // Bring the main window back (it was minimized when recording started).
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        Ok(())
    }

    fn push_frame(&mut self, frame: &FrameData) {
        let pts = self.current_frame_count as i64;
        if let Err(e) = self
            .encoder
            .send_frame(frame.buffer.clone(), frame.width, frame.height, pts)
        {
            self.error_message = Some(format!("Encoding error: {e}"));
        } else {
            self.last_frame = Some(frame.clone());
            self.current_frame_count += 1;
        }
    }

    pub fn process_frame(&mut self) {
        // Always drain the capture channel to keep preview fresh, even when not recording.
        while let Some(frame) = self.capture.try_recv_frame() {
            self.push_frame(&frame);
        }

        if self.state != RecordingState::Recording {
            return;
        }

        let target =
            (self.get_recording_duration().as_secs_f64() * self.target_fps as f64) as u64;
        while self.current_frame_count < target {
            match self.last_frame.clone() {
                Some(last) => self.push_frame(&last),
                None => break,
            }
        }
    }

    pub fn get_recording_duration(&self) -> Duration {
        match self.state {
            RecordingState::Recording => {
                self.paused_duration + self.recording_start_time.map(|s| s.elapsed()).unwrap_or_default()
            }
            RecordingState::Paused => self.paused_duration,
            _ => Duration::ZERO,
        }
    }

    pub fn start_area_selection(&mut self, ctx: &egui::Context) {
        if self.is_recording() {
            return;
        }
        // Remember where the window is so we can put it back afterwards.
        self.prev_outer_pos = ctx.input(|i| i.viewport().outer_rect.map(|r| r.min));
        self.state = RecordingState::SelectingArea;
        self.area_selector = Some(crate::ui::AreaSelector::new());
        self.selector_bg = None;
        self.selector_tex = None;
        self.selector_phase = Some(SelectorPhase::Hiding { since: Instant::now() });
        self.show_area_selector = true;
        // Park the window off-screen so the frozen screenshot is clean.
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::Pos2::new(
            10000.0, 10000.0,
        )));
        self.status_message = Some("Select an area on your screen…".to_string());
    }

    fn restore_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        if let Some(p) = self.prev_outer_pos.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(p));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    pub fn cancel_area_selection(&mut self, ctx: &egui::Context) {
        self.restore_window(ctx);
        self.state = RecordingState::Idle;
        self.area_selector = None;
        self.selector_phase = None;
        self.selector_bg = None;
        self.selector_tex = None;
        self.show_area_selector = false;
        self.status_message = Some("Area selection cancelled.".to_string());
    }

    pub fn complete_area_selection(
        &mut self,
        ctx: &egui::Context,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) {
        self.restore_window(ctx);
        self.capture_region = Some((x, y, width, height));
        self.capture_mode = CaptureMode::Region;
        self.state = RecordingState::Idle;
        self.area_selector = None;
        self.selector_phase = None;
        self.selector_bg = None;
        self.selector_tex = None;
        self.show_area_selector = false;
        self.status_message = Some(format!("Region set: {}×{} @ ({}, {})", width, height, x, y));
    }

    pub fn cut_selected_range(&mut self) {
        if let Some((start, end)) = self.timeline_state.get_selection() {
            if (end - start) < 0.01 {
                self.status_message = Some("Selection too small to cut.".to_string());
                return;
            }
            self.stop_playback();
            self.timeline.delete_range(start, end);
            let rate = self.waveform_rate;
            crate::timeline::splice_uniform(&mut self.waveform, rate, start, end);
            crate::timeline::splice_stamped(&mut self.filmstrip, start, end);
            self.thumb_textures.clear();
            self.timeline_state.clear_selection();
            self.timeline_state.playhead = Some(start.min(self.timeline.total_duration));
            self.timeline_state.clamp_view(self.timeline.total_duration);
            self.bump_media();
            self.status_message = Some(format!("Cut range {:.2}s – {:.2}s", start, end));
        }
    }

    pub fn trim_selection(&mut self) {
        if let Some((s, e)) = self.timeline_state.get_selection() {
            if (e - s) < 0.05 {
                self.status_message = Some("Selection too small to trim.".to_string());
                return;
            }
            self.stop_playback();
            let total = self.timeline.total_duration;
            // Delete tail first so head indices stay valid.
            if e < total {
                self.timeline.delete_range(e, total);
                let rate = self.waveform_rate;
                crate::timeline::splice_uniform(&mut self.waveform, rate, e, total);
                crate::timeline::splice_stamped(&mut self.filmstrip, e, total);
            }
            if s > 0.0 {
                self.timeline.delete_range(0.0, s);
                let rate = self.waveform_rate;
                crate::timeline::splice_uniform(&mut self.waveform, rate, 0.0, s);
                crate::timeline::splice_stamped(&mut self.filmstrip, 0.0, s);
                // splice_stamped already re-based later entries; uniform buckets are
                // compacted in place, which equals re-based for uniform sampling.
            }
            self.thumb_textures.clear();
            self.timeline_state.clear_selection();
            self.timeline_state.show_all();
            self.timeline_state.playhead = Some(0.0);
            self.bump_media();
            self.status_message = Some(format!("Trimmed to {:.2}s – {:.2}s", s, e));
        }
    }

    /// Auto-detect silence in the waveform and remove it (longest-first).
    pub fn delete_silence(&mut self, threshold: f32, min_secs: f64) {
        if self.timeline.total_duration <= 0.0 || self.waveform.is_empty() {
            self.status_message = Some("Nothing to scan — record a clip first.".to_string());
            return;
        }
        let rate = self.waveform_rate;
        let min_buckets = (min_secs * rate).ceil() as usize;
        let mut ranges: Vec<(f64, f64)> = Vec::new();
        let mut run_start: Option<usize> = None;
        for (i, &v) in self.waveform.iter().enumerate() {
            if v < threshold {
                if run_start.is_none() {
                    run_start = Some(i);
                }
            } else if let Some(s) = run_start.take() {
                if i - s >= min_buckets {
                    ranges.push((s as f64 / rate, i as f64 / rate));
                }
            }
        }
        if let Some(s) = run_start {
            if self.waveform.len() - s >= min_buckets {
                ranges.push((s as f64 / rate, self.waveform.len() as f64 / rate));
            }
        }
        if ranges.is_empty() {
            self.status_message = Some("No silence found at this threshold.".to_string());
            return;
        }
        self.stop_playback();
        // Delete latest-first so earlier times stay valid.
        ranges.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        let mut removed = 0.0;
        for (s, e) in &ranges {
            self.timeline.delete_range(*s, *e);
            crate::timeline::splice_uniform(&mut self.waveform, rate, *s, *e);
            crate::timeline::splice_stamped(&mut self.filmstrip, *s, *e);
            removed += e - s;
        }
        self.thumb_textures.clear();
        self.timeline_state.clear_selection();
        self.timeline_state.show_all();
        self.bump_media();
        self.status_message = Some(format!(
            "Removed {} silent part{} ({:.1}s)",
            ranges.len(),
            if ranges.len() == 1 { "" } else { "s" },
            removed
        ));
    }

    // ---------- playback (visual scrub over thumbs + playhead) ----------
    pub fn toggle_play(&mut self) {
        if self.timeline_state.is_playing {
            self.pause_playback();
        } else {
            self.start_playback();
        }
    }

    pub fn start_playback(&mut self) {
        let total = self.timeline.total_duration.max(self.live_duration_secs());
        if total <= 0.05 || self.is_recording() {
            return;
        }
        // With a dragged selection ▶ plays only that range: start at its head
        // (or at the playhead when it already sits inside) and stop at its tail.
        let range = self
            .timeline_state
            .get_selection()
            .map(|(s, e)| (s.max(0.0), e.min(total)))
            .filter(|(s, e)| e - s > 0.05);
        let (start, end) = range.unwrap_or((0.0, total));
        let mut from = self.timeline_state.playhead.unwrap_or(start);
        // Parked at (or past) the end means "replay"; so does a playhead
        // outside the selected range — snap back to its head.
        if from >= end - 0.05 || from < start {
            from = start;
        }
        from = from.clamp(start, (end - 0.01).max(start));
        self.playback_from = from;
        self.playback_end = Some(end);
        self.playback_started = Some(Instant::now());
        self.timeline_state.playhead = Some(from);
        self.timeline_state.is_playing = true;
        // Drop the last session's frame and start a background decode so the
        // preview shows real video instead of ~1 fps chase stills.
        self.play_tex = None;
        self.spawn_player(from);
        // Matching audio: decode the same segments so ▶ has sound too.
        self.spawn_audio_player(from, end);
    }

    pub fn pause_playback(&mut self) {
        if let (Some(t0), Some(ph)) =
            (self.playback_started, self.timeline_state.playhead)
        {
            let _ = (t0, ph);
        }
        self.playback_started = None;
        self.playback_end = None;
        self.timeline_state.is_playing = false;
        self.player = None;
        self.audio_player = None;
    }

    pub fn stop_playback(&mut self) {
        self.playback_started = None;
        self.playback_end = None;
        self.timeline_state.is_playing = false;
        self.player = None;
        self.audio_player = None;
    }

    /// Snapshot of the timeline's segments for the background players.
    fn play_segments(&self) -> Vec<crate::player::PlaySegment> {
        self.timeline
            .segments
            .iter()
            .map(|s| crate::player::PlaySegment {
                source_path: PathBuf::from(&s.source_path),
                source_start: s.source_start,
                source_end: s.source_end,
                timeline_start: s.timeline_start,
                timeline_end: s.timeline_end,
            })
            .collect()
    }

    /// Start preview audio alongside the video decoder (silent when no
    /// output device is available). Decoding stops at `until` so a selected
    /// range doesn't keep sounding past its end.
    fn spawn_audio_player(&mut self, from: f64, until: f64) {
        self.audio_player = None;
        if self.timeline.segments.is_empty() {
            return;
        }
        let segments = self.play_segments();
        self.audio_player = crate::audioplay::PreviewAudio::start(segments, from, until);
    }

    /// Kick off a background decoder for playback. The timeline's segments
    /// are snapshotted here so edits can't race the thread.
    fn spawn_player(&mut self, from: f64) {
        self.player = None;
        let Some(t0) = self.playback_started else { return };
        if self.timeline.segments.is_empty() {
            return;
        }
        // Decode at roughly the preview's on-screen width: sharp when it is
        // drawn 1:1, and cheap when the panel (or zoom) is small.
        self.preview_decode_w = if self.preview_wanted_w > 0 {
            self.preview_wanted_w
        } else {
            1280
        };
        let spec = crate::player::PlaySpec {
            segments: self.play_segments(),
            fps: self.timeline.fps.max(1) as f64,
            width: self.timeline.width,
            height: self.timeline.height,
            from,
            t0,
            total: self.playback_end.unwrap_or(self.timeline.total_duration),
            target_width: self.preview_decode_w,
        };
        self.player = crate::player::PreviewPlayer::start(spec);
        if self.player.is_some() {
            self.player_started = Some(Instant::now());
        }
    }

    /// Re-spawn the decoder when the preview's on-screen width changed a lot
    /// (window/sidebar resize, zoom) so playback stays sharp without decoding
    /// more than the panel actually shows.
    fn maybe_retarget_player(&mut self) {
        if self.player.is_none() || self.preview_wanted_w == 0 {
            return;
        }
        let wanted = self.preview_wanted_w;
        let cur = self.preview_decode_w;
        let tol = (cur / 5).max(64);
        if (wanted as i64 - cur as i64).abs() <= tol as i64 {
            return;
        }
        // Debounce live resizes; the check reruns every tick, so the final
        // size is applied once the resize stops.
        if let Some(t) = self.player_started {
            if t.elapsed() < Duration::from_millis(400) {
                return;
            }
        }
        self.spawn_player(self.playback_from);
    }

    /// Drain decoded playback frames and upload the newest as a texture.
    fn pump_player(&mut self, ctx: &egui::Context) {
        if !self.timeline_state.is_playing {
            // Safety net for transport actions that bypass stop_playback
            // (e.g. "go to start" flips the flags directly).
            self.player = None;
            self.audio_player = None;
            return;
        }
        self.maybe_retarget_player();
        let mut ended = false;
        let mut frame = None;
        if let Some(player) = &self.player {
            match player.poll() {
                crate::player::Poll::Frame(f) => frame = Some(f),
                crate::player::Poll::Idle => {}
                crate::player::Poll::Ended => ended = true,
            }
        }
        if ended {
            self.player = None;
        }
        let Some(f) = frame else { return };
        let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
        match &mut self.play_tex {
            Some((t, g, aspect, tex)) if *g == self.media_gen => {
                *t = f.tl_t;
                *aspect = f.aspect;
                tex.set(img, egui::TextureOptions::LINEAR);
            }
            slot => {
                let tex = ctx.load_texture("preview-stream", img, egui::TextureOptions::LINEAR);
                *slot = Some((f.tl_t, self.media_gen, f.aspect, tex));
            }
        }
    }

    /// Whether there is a finished take worth opening the editor for.
    pub fn has_editable_video(&self) -> bool {
        !self.timeline.segments.is_empty() && self.timeline.total_duration > 0.05
    }

    pub fn open_editor(&mut self) {
        if self.is_recording() || !self.has_editable_video() {
            return;
        }
        self.show_editor = true;
    }

    pub fn close_editor(&mut self) {
        self.stop_playback();
        self.show_editor = false;
    }

    fn update_playback(&mut self) {
        if !self.timeline_state.is_playing {
            return;
        }
        let total = self.timeline.total_duration;
        if total <= 0.0 {
            self.stop_playback();
            return;
        }
        // Stop where the selected range ends (whole timeline when none).
        let end = self.playback_end.unwrap_or(total).clamp(0.0, total);
        if let Some(t0) = self.playback_started {
            let t = self.playback_from + t0.elapsed().as_secs_f64();
            if t >= end {
                self.timeline_state.playhead = Some(end);
                self.stop_playback();
            } else {
                self.timeline_state.playhead = Some(t);
                // Keep the playhead in view while playing.
                let len = self.timeline_state.view_len(total);
                if t < self.timeline_state.view_start
                    || t > self.timeline_state.view_start + len
                {
                    self.timeline_state.view_start =
                        (t - len * 0.2).clamp(0.0, (total - len).max(0.0));
                }
            }
        }
    }

    /// Save a trimmed copy with volume + fades applied (asks for a path).
    pub fn save_edited_copy(&mut self) {
        if self.timeline.segments.is_empty() {
            self.status_message = Some("Nothing to save yet.".to_string());
            return;
        }
        let dlg = rfd::FileDialog::new()
            .set_file_name(format!(
                "pipit_edited_{}.mp4",
                Local::now().format("%Y%m%d_%H%M%S")
            ))
            .add_filter("MP4 video", &["mp4"])
            .save_file();
        let Some(path) = dlg else { return };
        self.stop_playback();
        let segs: Vec<(f64, f64)> = self
            .timeline
            .segments
            .iter()
            .map(|s| (s.source_start, s.source_end))
            .collect();
        let vol = (self.system_volume * self.master_volume).clamp(0.0, 2.0);
        match crate::encoder::export_timeline(
            &self.output_path,
            &path,
            &segs,
            vol,
            self.fade_in,
            self.fade_out,
        ) {
            Ok(()) => {
                self.status_message =
                    Some(format!("Exported edited copy to: {}", path.display()));
                // "Save and Close" closes the editing window on success.
                self.close_editor();
            }
            Err(e) => self.error_message = Some(format!("Export failed: {e}")),
        }
    }

    // ---------- editor track capture ----------
    fn downscale_to_thumb(buf: &[u8], w: u32, h: u32) -> Option<egui::ColorImage> {
        // Wide enough that the scrub preview (upscaled ~3x) stays readable.
        const TW: usize = 192;
        if w == 0 || h == 0 || buf.len() < (w * h * 4) as usize {
            return None;
        }
        let th = ((TW as u32 * h / w.max(1)).clamp(48, 160)) as usize;
        let mut pixels = Vec::with_capacity(TW * th);
        for y in 0..th {
            let sy = (y as u32 * h / th as u32).min(h - 1) as usize;
            for x in 0..TW {
                let sx = (x as u32 * w / TW as u32).min(w - 1) as usize;
                let o = (sy * w as usize + sx) * 4;
                pixels.push(egui::Color32::from_rgba_unmultiplied(
                    buf[o], buf[o + 1], buf[o + 2], 255,
                ));
            }
        }
        Some(egui::ColorImage {
            size: [TW, th],
            pixels,
        })
    }

    /// Called each UI tick while recording: thumbs ~2Hz, waveform ~20Hz.
    fn capture_editor_tracks(&mut self) {
        if self.state != RecordingState::Recording {
            return;
        }
        let t = self.get_recording_duration().as_secs_f64();
        if t - self.last_thumb_time >= 0.5 {
            if let Some(f) = &self.last_frame {
                if let Some(img) = Self::downscale_to_thumb(&f.buffer, f.width, f.height) {
                    self.filmstrip.push((t, img));
                    // Textures are (re)built lazily in ensure_thumb_textures.
                    self.last_thumb_time = t;
                }
            }
        }
        let want = (t * self.waveform_rate).floor() as usize;
        if self.waveform.len() < want {
            let lvl = match &self.audio_capture {
                Some(cap) => cap.system_level().max(cap.mic_level()),
                None => 0.0,
            };
            // Live levels are already perceptually (dB) mapped in audio.rs, so
            // store them as-is to match the file-derived waveform built at Stop.
            let v = lvl.clamp(0.0, 1.0);
            while self.waveform.len() < want {
                self.waveform.push(v);
            }
        }
    }

    fn ensure_thumb_textures(&mut self, ctx: &egui::Context) {
        if self.thumb_textures.len() == self.filmstrip.len() {
            return;
        }
        if self.thumb_textures.len() > self.filmstrip.len() {
            // Edits shrank the strip: rebuild.
            self.thumb_textures.clear();
        }
        // Incremental: only upload new thumbs (avoids re-uploading all each 0.5s).
        while self.thumb_textures.len() < self.filmstrip.len() {
            let i = self.thumb_textures.len();
            let tex = ctx.load_texture(
                format!("thumb{i}"),
                self.filmstrip[i].1.clone(),
                egui::TextureOptions::LINEAR,
            );
            self.thumb_textures.push(tex);
        }
    }

    pub fn refresh_monitors(&mut self) {
        self.monitors = crate::capture::get_monitors().unwrap_or_default();
        if self.selected_monitor.map(|i| i >= self.monitors.len()).unwrap_or(false) {
            self.selected_monitor = if self.monitors.is_empty() { None } else { Some(0) };
        }
    }

    pub fn refresh_windows(&mut self) {
        self.windows = crate::capture::get_windows().unwrap_or_default();
    }

    /// Timeline media changed (new take / edit): drop cached scrub stills.
    fn bump_media(&mut self) {
        self.media_gen = self.media_gen.wrapping_add(1);
        self.still_tex = None;
        self.play_tex = None;
        // The decoder thread holds a snapshot of the old segments; dropping
        // it kills its ffmpeg child and ends that session.
        self.player = None;
        self.audio_player = None;
        // Dropping the receiver orphans any in-flight decode; its send then
        // fails harmlessly and the thread exits.
        self.still_rx = None;
        self.still_pending = None;
        self.still_probe = None;
    }

    /// Poll finished scrub-still decodes and request a new one once the
    /// playhead has settled (debounced so drags don't spam ffmpeg).
    fn update_scrub_still(&mut self, ctx: &egui::Context) {
        if self.still_rx.is_some() {
            let done = self
                .still_rx
                .as_ref()
                .and_then(|rx| rx.try_recv().ok());
            if let Some(res) = done {
                self.still_rx = None;
                self.still_pending = None;
                if res.gen == self.media_gen {
                    if let Some((buf, w, h)) = res.frame {
                        if w > 0
                            && h > 0
                            && buf.len() == (w as usize) * (h as usize) * 4
                        {
                            let aspect = h as f32 / w as f32;
                            let img = egui::ColorImage::from_rgba_unmultiplied(
                                [w as usize, h as usize],
                                &buf,
                            );
                            let tex = ctx.load_texture(
                                "scrub-still",
                                img,
                                egui::TextureOptions::LINEAR,
                            );
                            self.still_tex =
                                Some((res.timeline_t, res.gen, aspect, tex));
                        }
                    }
                }
            }
        }

        let playing = self.timeline_state.is_playing;
        let dragging = self.timeline_state.is_dragging_selection;
        if !self.is_recording() && playing && !dragging {
            // The streaming player (pump_player) feeds the preview at the
            // video's real frame rate; only fall back to the ~1/s chase
            // stills when the stream is missing or has gone stale.
            let stream_ok = match (&self.play_tex, self.timeline_state.playhead) {
                (Some((t, g, _, _)), Some(ph)) if *g == self.media_gen => {
                    *t <= ph + 0.35 && ph - *t < 1.0
                }
                _ => false,
            };
            if !stream_ok && self.still_rx.is_none() {
                if let Some(ph) = self.timeline_state.playhead {
                    let need = match &self.still_tex {
                        Some((t, g, _, _)) if *g == self.media_gen => {
                            ph - *t > 1.0 || *t > ph + 0.05
                        }
                        _ => true,
                    };
                    if need {
                        self.maybe_request_still(ph);
                    }
                }
            }
            self.still_probe = None;
            return;
        }

        let idle = !self.is_recording() && !playing && !dragging;
        let ph = if idle {
            self.timeline_state.playhead
        } else {
            None
        };
        match (ph, self.still_probe) {
            (Some(p), Some((last, since))) if (p - last).abs() < 1e-3 => {
                if since.elapsed() > Duration::from_millis(200) {
                    self.maybe_request_still(p);
                }
            }
            (Some(p), _) => {
                self.still_probe = Some((p, Instant::now()));
            }
            (None, _) => {
                self.still_probe = None;
            }
        }
    }

    fn maybe_request_still(&mut self, timeline_t: f64) {
        let gen = self.media_gen;
        // Already have (or are decoding) a still for here.
        if let Some((t, g, _, _)) = &self.still_tex {
            if *g == gen && (*t - timeline_t).abs() < 0.25 {
                return;
            }
        }
        if let Some((t, g)) = self.still_pending {
            if g == gen && (t - timeline_t).abs() < 0.25 {
                return;
            }
        }
        if self.still_rx.is_some() {
            return;
        }
        // Parked at the end with the full-res last frame available: no need
        // to decode (the "last take" still branch covers it).
        let last_thumb_t = self.filmstrip.last().map(|(t, _)| *t);
        let parked_at_end = match (self.timeline_state.playhead, last_thumb_t) {
            (Some(ph), Some(last_t)) => ph >= last_t,
            _ => true,
        };
        if parked_at_end && self.preview_texture.is_some() {
            return;
        }

        let seg = match self.timeline.get_segment_at_time(timeline_t) {
            Some(s) => s.clone(),
            None => return,
        };
        let span = (seg.timeline_end - seg.timeline_start).max(1e-6);
        let frac = ((timeline_t - seg.timeline_start) / span).clamp(0.0, 1.0);
        let src_t = seg.source_start + frac * (seg.source_end - seg.source_start);
        let path = PathBuf::from(&seg.source_path);
        if !path.exists() {
            return;
        }
        let (w, h) = (self.timeline.width, self.timeline.height);
        if w == 0 || h == 0 {
            return;
        }
        let ffmpeg = crate::encoder::get_ffmpeg_path();
        let (tx, rx) = std::sync::mpsc::channel();
        self.still_rx = Some(rx);
        self.still_pending = Some((timeline_t, gen));
        let ok = std::thread::Builder::new()
            .name("scrub-still".into())
            .spawn(move || {
                let frame =
                    crate::encoder::extract_frame(&ffmpeg, &path, src_t, w, h).ok();
                let _ = tx.send(StillResult {
                    timeline_t,
                    gen,
                    frame,
                });
            })
            .is_ok();
        if !ok {
            self.still_rx = None;
            self.still_pending = None;
        }
    }

    pub fn refresh_audio(&mut self) {
        self.audio_devices = crate::audio::list_audio_devices();
        if self.mic_device.is_none() {
            self.mic_device = self.audio_devices.default_mic.clone();
        }
    }

    /// Switch the folder new takes are written to (ignored while recording).
    /// Also retargets the pending `output_path` preview so "Save to" updates
    /// immediately; the choice is remembered across restarts.
    pub fn set_output_dir(&mut self, dir: PathBuf) {
        if self.is_recording() {
            return;
        }
        if std::fs::create_dir_all(&dir).is_err() {
            self.error_message = Some(format!("Could not use folder: {}", dir.display()));
            return;
        }
        self.output_dir = dir.clone();
        save_output_dir(&dir);
        // Keep the same pending filename, just relocated.
        if let Some(name) = self.output_path.file_name().map(|n| n.to_os_string()) {
            self.output_path = dir.join(name);
        } else {
            self.output_path = dir.join(format!(
                "recording_{}.mp4",
                Local::now().format("%Y%m%d_%H%M%S")
            ));
        }
        self.status_message = Some(format!("Save folder: {}", dir.display()));
    }

    /// Blocking folder picker; call from a button handler and apply after.
    fn pick_output_dir_dialog(current: &PathBuf) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_directory(current)
            .pick_folder()
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        if let Some(frame) = &self.last_frame {
            let (w, h) = (frame.width as usize, frame.height as usize);
            if w == 0 || h == 0 || frame.buffer.len() < w * h * 4 {
                return;
            }
            // Downscale before the CPU RGBA->Color32 convert + GPU upload.
            // A 1920x1080 frame is ~8MB per upload; capping the preview at
            // 640px wide cuts CPU + bandwidth ~9x vs full-res with no
            // visible loss in the small preview panel.
            const PREVIEW_MAX_W: usize = 640;
            let scale = if w > PREVIEW_MAX_W {
                PREVIEW_MAX_W as f32 / w as f32
            } else {
                1.0
            };
            let image = if scale >= 0.999 {
                egui::ColorImage::from_rgba_unmultiplied([w, h], &frame.buffer)
            } else {
                let tw = ((w as f32 * scale).round() as usize).max(1);
                let th = ((h as f32 * scale).round() as usize).max(1);
                let mut pixels = Vec::with_capacity(tw * th);
                for y in 0..th {
                    let sy = (y * h / th).min(h - 1);
                    let row_off = sy * w * 4;
                    for x in 0..tw {
                        let sx = (x * w / tw).min(w - 1);
                        let o = row_off + sx * 4;
                        // from_rgb skips the alpha-unmultiply math: cheaper
                        // per pixel and identical when alpha is opaque.
                        pixels.push(Color32::from_rgb(
                            frame.buffer[o],
                            frame.buffer[o + 1],
                            frame.buffer[o + 2],
                        ));
                    }
                }
                egui::ColorImage {
                    size: [tw, th],
                    pixels,
                }
            };
            // NEAREST skips GPU bilinear filtering on the upload path;
            // at 640px in a small panel the quality loss is negligible.
            let tex = ctx.load_texture(
                "live-preview",
                image,
                egui::TextureOptions::NEAREST,
            );
            self.preview_texture = Some(tex);
            self.preview_size = Some((frame.width, frame.height));
            self.last_preview_update = Some(Instant::now());
        }
    }
}

fn apply_theme(ctx: &egui::Context) {
    use crate::theme as th;
    // Bright, colorful light theme (was all-black dark theme).
    let mut visuals = egui::Visuals::light();
    visuals.dark_mode = false;

    // ---- surfaces: bright whites with a hint of blue ----
    visuals.window_fill = th::PANEL_CENTER;
    visuals.panel_fill = th::SURFACE;
    visuals.faint_bg_color = th::PLACEHOLDER_BG;
    visuals.extreme_bg_color = th::SUBTLE_BG;
    visuals.code_bg_color = th::SUBTLE_BG;
    visuals.hyperlink_color = th::ACCENT;
    visuals.warn_fg_color = th::WARN_TEXT;
    visuals.error_fg_color = th::ERROR_TEXT;
    visuals.window_stroke = egui::Stroke::new(1.0_f32, th::BORDER);
    visuals.text_cursor.stroke = egui::Stroke::new(2.0_f32, th::ACCENT);

    // ---- rounded corners everywhere ----
    visuals.window_rounding = egui::Rounding::same(12.0);
    visuals.menu_rounding = egui::Rounding::same(10.0);
    visuals.widgets.noninteractive.rounding = egui::Rounding::same(th::RADIUS);
    visuals.widgets.inactive.rounding = egui::Rounding::same(th::RADIUS);
    visuals.widgets.hovered.rounding = egui::Rounding::same(th::RADIUS);
    visuals.widgets.active.rounding = egui::Rounding::same(th::RADIUS);
    visuals.widgets.open.rounding = egui::Rounding::same(th::RADIUS);

    // ---- widget colors: white buttons, blue hover, vivid blue active ----
    visuals.widgets.noninteractive.bg_fill = th::SUBTLE_BG;
    visuals.widgets.noninteractive.weak_bg_fill = th::SUBTLE_BG;
    visuals.widgets.noninteractive.bg_stroke =
        egui::Stroke::new(1.0_f32, th::BORDER);
    visuals.widgets.noninteractive.fg_stroke =
        egui::Stroke::new(1.0_f32, th::TEXT);

    visuals.widgets.inactive.bg_fill = th::SURFACE;
    visuals.widgets.inactive.weak_bg_fill = th::SURFACE;
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0_f32, th::BORDER_STRONG);
    visuals.widgets.inactive.fg_stroke =
        egui::Stroke::new(1.0_f32, th::TEXT);

    visuals.widgets.hovered.bg_fill = th::ACCENT_SOFT;
    visuals.widgets.hovered.weak_bg_fill = th::ACCENT_SOFT;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, th::ACCENT);
    visuals.widgets.hovered.fg_stroke =
        egui::Stroke::new(1.5_f32, th::TEXT);

    visuals.widgets.active.bg_fill = th::ACCENT;
    visuals.widgets.active.weak_bg_fill = th::ACCENT;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, th::ACCENT_DARK);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.5_f32, Color32::WHITE);

    visuals.widgets.open.bg_fill = th::ACCENT_SOFT;
    visuals.widgets.open.weak_bg_fill = th::ACCENT_SOFT;
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0_f32, th::ACCENT);
    visuals.widgets.open.fg_stroke =
        egui::Stroke::new(1.5_f32, th::TEXT);

    // ---- selection: accent blue, same as the buttons ----
    visuals.selection.bg_fill = th::ACCENT;
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, Color32::WHITE);

    ctx.set_visuals(visuals);
    let mut style = (*ctx.style()).clone();
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    // Minimum hit target: plain (non-`small`) widgets — checkbox, slider
    // handles, DragValue, regular buttons — never get shorter than this.
    style.spacing.interact_size = egui::vec2(28.0, 26.0);
    // Room for "name (3840×2160)" style selected texts without clipping.
    style.spacing.combo_width = 150.0;
    style.spacing.window_margin = egui::Margin::same(10.0);
    ctx.set_style(style);
}

fn level_bar(ui: &mut egui::Ui, label: &str, level: f32, color: Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 2.0);
        // Fixed-width label so the Sys/Mic bars line up column-wise.
        ui.add_sized(
            egui::vec2(24.0, 12.0),
            egui::Label::new(egui::RichText::new(label).color(theme::TEXT_DIM).small()),
        );
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(110.0, 8.0), egui::Sense::hover());
        resp.on_hover_text(format!("{label} level: {:.0}%", level.clamp(0.0, 1.0) * 100.0));
        ui.painter().rect_filled(rect, 4.0, theme::METER_TRACK);
        let w = (rect.width() * level.clamp(0.0, 1.0)).max(if level > 0.01 { 3.0 } else { 0.0 });
        if w > 0.0 {
            let fill = egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height()));
            ui.painter().rect_filled(fill, 4.0, color);
        }
    });
}

/// Draw `tex_id` (with `height / width == aspect`) as large as possible inside
/// the remaining `ui` space while preserving aspect ratio, then apply `zoom`
/// (1.0 = fit). Zoomed previews grow past the panel and scroll, so details
/// can be inspected without losing the always-fits default.
///
/// Returns the width the image ended up at (what the decoder should target).
///
/// The old code sized the image from width alone
/// (`avail_w.min(720) * aspect`, clamped to 120..420px tall) and then put it
/// in a `centered_and_justified` that fills the whole CentralPanel height.
/// On tall windows that left a few hundred px of empty Frame background above
/// the (max 420px tall) image — the gap in the screenshot. Fitting to
/// `available_size()` removes it; only true aspect-ratio letterboxing remains.
fn show_preview_fit(
    ui: &mut egui::Ui,
    tex_id: egui::TextureId,
    aspect: f32,
    zoom: f32,
) -> f32 {
    let aspect = aspect.clamp(0.1, 4.0);
    let avail = ui.available_size();
    let max_w = avail.x.max(80.0);
    // `available_size().y` here is the rest of the CentralPanel height, so the
    // image grows to fill the panel instead of topping out at 420px.
    let max_h = (avail.y - 4.0).max(80.0);
    let mut w = max_w;
    let mut h = w * aspect;
    if h > max_h {
        h = max_h;
        w = h / aspect;
    }
    if zoom > 1.001 {
        let size = egui::vec2(w * zoom, h * zoom);
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.image((tex_id, size));
            });
        size.x
    } else {
        ui.centered_and_justified(|ui| {
            ui.image((tex_id, egui::vec2(w.max(80.0), h.max(80.0))));
        });
        w.max(80.0)
    }
}

impl eframe::App for ScreenRecorderApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Only run preview capture when recording (live preview needed)
        if self.is_recording() {
            self.ensure_preview_capture();
        } else {
            // Stop any idle preview capture when not recording
            self.capture.stop();
        }
        self.process_frame();
        if self.is_recording() {
            self.update_preview(ctx);
        }
        self.update_scrub_still(ctx);
        self.capture_editor_tracks();
        self.update_playback();
        self.pump_player(ctx);
        self.ensure_thumb_textures(ctx);

        // ---------- age out stale errors ----------
        // 15s gives the user time to read (and the ✕ still dismisses early);
        // a *new* error resets the clock because the text differs.
        let err_now = self.error_message.clone();
        match (&err_now, &mut self.error_seen) {
            (Some(msg), seen) => match seen {
                Some((prev, at)) if prev == msg => {
                    if at.elapsed() > Duration::from_secs(15) {
                        self.error_message = None;
                        *seen = None;
                    }
                }
                _ => *seen = Some((msg.clone(), Instant::now())),
            },
            (None, seen) => *seen = None,
        }

        // ---------- fullscreen area selector (frozen-screen overlay) ----------
        if self.show_area_selector {
            // Keep the phase machine ticking (hide delay, repaint for drag).
            ctx.request_repaint_after(Duration::from_millis(33));

            // ESC works in every phase.
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.cancel_area_selection(ctx);
                return;
            }

            // Phase 1: window is parked off-screen. Wait a beat for the move
            // to apply, then freeze the primary monitor.
            let hide_elapsed = match &self.selector_phase {
                Some(SelectorPhase::Hiding { since }) => Some(since.elapsed()),
                _ => None,
            };
            if let Some(elapsed) = hide_elapsed {
                if elapsed > Duration::from_millis(350) && self.selector_bg.is_none() {
                    match self
                        .capture
                        .grab_primary_screenshot(Duration::from_secs(2))
                    {
                        Ok(frame) => {
                            let img = egui::ColorImage::from_rgba_unmultiplied(
                                [frame.width as usize, frame.height as usize],
                                &frame.buffer,
                            );
                            self.selector_tex = Some(ctx.load_texture(
                                "area-selector-bg",
                                img,
                                egui::TextureOptions::LINEAR,
                            ));
                            self.selector_bg = Some(SelectorBg {
                                width: frame.width,
                                height: frame.height,
                            });
                            self.selector_phase = Some(SelectorPhase::Armed);
                            // Cover the primary monitor for free selection.
                            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                                egui::Pos2::ZERO,
                            ));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        }
                        Err(e) => {
                            self.error_message =
                                Some(format!("Could not freeze the screen: {e:#}"));
                            self.cancel_area_selection(ctx);
                            return;
                        }
                    }
                }
                // Still hiding: keep the parked window blank.
                if !matches!(self.selector_phase, Some(SelectorPhase::Armed)) {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let (resp, painter) = ui.allocate_painter(
                            ui.available_size(),
                            egui::Sense::hover(),
                        );
                        painter.rect_filled(resp.rect, 0.0, egui::Color32::BLACK);
                    });
                    return;
                }
            }

            // Phase 2: fullscreen overlay over the frozen screenshot.
            let armed = matches!(self.selector_phase, Some(SelectorPhase::Armed));
            let tex_id = self.selector_tex.as_ref().map(|t| t.id());
            let (img_w, img_h) = self
                .selector_bg
                .as_ref()
                .map(|b| (b.width, b.height))
                .unwrap_or((0, 0));
            if armed && img_w > 0 {
                if let Some(tex_id) = tex_id {
                let mut finished: Option<egui::Rect> = None;
                let mut view_rect = egui::Rect::NOTHING;
                let mut cancelled = false;
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response =
                        ui.allocate_response(ui.available_size(), egui::Sense::click_and_drag());
                    view_rect = response.rect;

                    let mut selector = self
                        .area_selector
                        .take()
                        .unwrap_or_else(crate::ui::AreaSelector::new);
                    if response.drag_started() {
                        if let Some(p) = response.interact_pointer_pos() {
                            selector.begin_drag(p);
                        }
                    }
                    if response.dragged() {
                        if let Some(p) = response.interact_pointer_pos() {
                            selector.update_drag(p);
                        }
                    }
                    if response.drag_stopped() {
                        finished = selector.end_drag();
                    }
                    let live = selector.live_rect();
                    self.area_selector = Some(selector);

                    crate::ui::paint_area_overlay(ui, tex_id, view_rect, live);

                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                    }

                    // Cancel button (top-right) for mouse-only users.
                    let btn_rect = egui::Rect::from_min_size(
                        view_rect.right_top() + egui::vec2(-122.0, 10.0),
                        egui::vec2(112.0, 30.0),
                    );
                    if ui.put(btn_rect, egui::Button::new("Cancel (Esc)")).clicked() {
                        cancelled = true;
                    }
                });
                if cancelled {
                    self.cancel_area_selection(ctx);
                    return;
                }
                if let Some(rect) = finished {
                    match crate::ui::map_overlay_rect_to_pixels(
                        rect, view_rect, img_w, img_h,
                    ) {
                        Some((x, y, w, h)) => {
                            self.complete_area_selection(ctx, x, y, w, h);
                        }
                        None => {
                            self.status_message = Some(
                                "Selection too small — drag a larger area.".to_string(),
                            );
                        }
                    }
                    return;
                }
                return;
                }
            }

            // Fallback: shouldn't normally be reached (e.g. phase lost);
            // bail out cleanly instead of trapping the user off-screen.
            self.cancel_area_selection(ctx);
            return;
        }

        // ---------- recording mode: main window minimized, only mini controls ----------
        // The floating controller + rectangle marker are the primary UI while
        // minimized; the main window collapses to Start/Pause/Continue/Stop
        // only so restoring it never leaks the full editor into the capture.
        if self.is_recording() {
            // ESC stops the take: local key (our windows focused) + global
            // Win32 poll (any app focused) with edge detection.
            let esc_local = ctx.input(|i| i.key_pressed(egui::Key::Escape));
            let esc_now = esc_down_global();
            let esc_edge = esc_now && !self.esc_prev_down;
            self.esc_prev_down = esc_now;
            if esc_local || esc_edge {
                if let Err(e) = self.stop_recording(ctx) {
                    self.error_message = Some(e);
                }
                self.esc_prev_down = esc_down_global();
            } else {
                self.show_recording_floaters(ctx);
            }
            // Stopped from the floating controller / ESC: fall through to full UI
            // (stop_recording already restored the main window).
            if self.is_recording() {
                let paused = self.state == RecordingState::Paused;
                let dur = self.get_recording_duration();
                let dur_str = crate::ui::format_duration(dur);
                let src = self.recording_source_label();
                egui::TopBottomPanel::top("rec_toolbar")
                    .frame(
                        egui::Frame::none()
                            .fill(theme::PANEL_TOOLBAR)
                            .stroke(egui::Stroke::new(1.0_f32, theme::ACCENT_BORDER))
                            .inner_margin(theme::panel_margin()),
                    )
                    .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.heading(if paused {
                            egui::RichText::new("⏸ Paused").color(theme::PAUSED)
                        } else {
                            egui::RichText::new("● REC").color(theme::REC_SOFT)
                        });
                        ui.monospace(dur_str.clone());
                        ui.separator();
                        if paused {
                            let resume = theme::resume_btn("▶ Resume");
                            if ui.add_sized([110.0, theme::BTN_H], resume).clicked() {
                                self.resume_recording();
                            }
                        } else {
                            if ui
                                .add_sized([90.0, theme::BTN_H], egui::Button::new("⏸ Pause"))
                                .clicked()
                            {
                                self.pause_recording();
                            }
                        }
                        let stop = theme::record_btn("■ Stop");
                        if ui.add_sized([90.0, theme::BTN_H], stop).clicked() {
                            if let Err(e) = self.stop_recording(ctx) {
                                self.error_message = Some(e);
                            }
                        }
                        theme::hint(ui, "Esc stops • the red border marks the captured area");
                    });
                });
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::none()
                            .fill(theme::PANEL_CENTER)
                            .inner_margin(egui::Margin::same(8.0)),
                    )
                    .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(24.0);
                        ui.heading(theme::heading("Recording… main window is minimized"));
                        ui.label(format!("{} • {}", dur_str, src));
                        theme::hint(
                            ui,
                            "Use the floating Pipit controller to Pause / Resume / Stop (or press ESC).",
                        );
                        theme::hint(ui, "The red border on screen marks the captured area.");
                        if let Some(msg) = &self.status_message {
                            ui.add_space(8.0);
                            theme::hint(ui, msg);
                        }
                        if let Some(err) = &self.error_message {
                            ui.colored_label(theme::ERROR_TEXT, format!("⚠ {}", err));
                        }
                    });
                });
                // The placeholder window only shows the tenths-of-seconds
                // counter, so 10 Hz is plenty (the floating controller runs
                // at 20 Hz on its own viewport).
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
        }

        // ---------- top toolbar ----------
        egui::TopBottomPanel::top("toolbar")
            .frame(
                egui::Frame::none()
                    .fill(theme::PANEL_TOOLBAR)
                    .stroke(egui::Stroke::new(1.0_f32, theme::ACCENT_BORDER))
                    .inner_margin(theme::panel_margin()),
            )
            .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading(
                    egui::RichText::new("🐦 Pipit").color(theme::ACCENT_DARK),
                );
                ui.label(egui::RichText::new("screen recorder").color(theme::BRAND).small());
                ui.separator();

                let recording = self.is_recording();
                ui.add_enabled_ui(!recording, |ui| {
                    let src = egui::ComboBox::from_label("Source")
                        .selected_text(match self.capture_mode {
                            CaptureMode::Monitor => "🖥 Monitor",
                            CaptureMode::Window => "🪟 Window",
                            CaptureMode::Region => "✂ Region",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.capture_mode, CaptureMode::Monitor, "🖥 Monitor");
                            ui.selectable_value(&mut self.capture_mode, CaptureMode::Window, "🪟 Window");
                            ui.selectable_value(&mut self.capture_mode, CaptureMode::Region, "✂ Region");
                        });
                    src.response
                        .on_hover_text("What to record: a whole monitor, one window, or a region")
                        .on_disabled_hover_text("Locked while recording — press Stop first");

                    match self.capture_mode {
                        CaptureMode::Monitor => {
                            let label = self
                                .selected_monitor
                                .and_then(|i| self.monitors.get(i))
                                .map(|m| theme::truncate(&format!("{} ({}×{})", m.name, m.width, m.height), 32))
                                .unwrap_or_else(|| "Select…".to_string());
                            egui::ComboBox::from_id_salt("monitor_pick")
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    for (i, m) in self.monitors.iter().enumerate() {
                                        ui.selectable_value(
                                            &mut self.selected_monitor,
                                            Some(i),
                                            theme::truncate(&format!("{} ({}×{})", m.name, m.width, m.height), 48),
                                        );
                                    }
                                });
                            if ui.add(theme::icon_btn("⟳"))
                                .on_hover_text("Refresh monitors")
                                .clicked()
                            {
                                self.refresh_monitors();
                            }
                        }
                        CaptureMode::Window => {
                            let label = self
                                .selected_window
                                .clone()
                                .map(|t| theme::truncate(&t, 32))
                                .unwrap_or_else(|| "Select…".to_string());
                            egui::ComboBox::from_id_salt("window_pick")
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    for w in &self.windows {
                                        let short = theme::truncate(&w.title, 48);
                                        ui.selectable_value(&mut self.selected_window, Some(w.title.clone()), short);
                                    }
                                });
                            if ui.add(theme::icon_btn("⟳"))
                                .on_hover_text("Refresh windows")
                                .clicked()
                            {
                                self.refresh_windows();
                            }
                        }
                        CaptureMode::Region => {
                            if let Some((x, y, w, h)) = self.capture_region {
                                ui.label(format!("{}×{} @ ({},{})", w, h, x, y));
                            } else {
                                ui.label("No region yet");
                            }
                            if ui
                                .button("Select Area")
                                .on_hover_text("Freeze the screen, then drag out the region to record")
                                .clicked()
                            {
                                self.start_area_selection(ctx);
                            }
                        }
                    }
                });

                ui.separator();
                ui.add_enabled_ui(!recording, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut self.target_fps)
                            .range(15..=60)
                            .suffix(" FPS"),
                    )
                    .on_hover_text("Frames per second of the recorded video (15–60)")
                    .on_disabled_hover_text("Locked while recording");
                });
                ui.separator();

                match self.state {
                    RecordingState::Idle => {
                        let btn = theme::record_btn("● Record");
                        if ui
                            .add_sized([110.0, theme::BTN_H], btn)
                            .on_hover_text("Start recording (Esc stops while recording)")
                            .clicked()
                        {
                            if let Err(e) = self.start_recording(ctx) {
                                self.error_message = Some(e);
                            }
                        }
                    }
                    RecordingState::Recording => {
                        if ui
                            .add_sized([90.0, theme::BTN_H], egui::Button::new("⏸ Pause"))
                            .on_hover_text("Pause the take (resumes without a new clip)")
                            .clicked()
                        {
                            self.pause_recording();
                        }
                        let stop = theme::record_btn("■ Stop");
                        if ui
                            .add_sized([90.0, theme::BTN_H], stop)
                            .on_hover_text("Stop and save this take (Esc)")
                            .clicked()
                        {
                            if let Err(e) = self.stop_recording(ctx) {
                                self.error_message = Some(e);
                            }
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "● REC {}",
                                format_duration(self.get_recording_duration())
                            ))
                            .color(theme::REC_SOFT)
                            .strong(),
                        );
                    }
                    RecordingState::Paused => {
                        let resume = theme::resume_btn("▶ Resume");
                        if ui
                            .add_sized([100.0, theme::BTN_H], resume)
                            .on_hover_text("Continue the same take (Esc stops)")
                            .clicked()
                        {
                            self.resume_recording();
                        }
                        let stop = theme::record_btn("■ Stop");
                        if ui
                            .add_sized([90.0, theme::BTN_H], stop)
                            .on_hover_text("Stop and save this take (Esc)")
                            .clicked()
                        {
                            if let Err(e) = self.stop_recording(ctx) {
                                self.error_message = Some(e);
                            }
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "⏸ {}",
                                format_duration(self.get_recording_duration())
                            ))
                            .color(theme::PAUSED)
                            .strong(),
                        );
                    }
                    RecordingState::SelectingArea => {}
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let settings = ui
                        .selectable_label(self.show_settings, "⚙ Settings")
                        .on_hover_text("Audio levels, output folder, FPS and more");
                    if settings.clicked() {
                        self.show_settings = !self.show_settings;
                    }
                    let can_edit = !self.is_recording() && self.has_editable_video();
                    ui.add_enabled_ui(can_edit, |ui| {
                        let label = if self.show_editor { "✂ Editing…" } else { "✂ Edit video" };
                        let r = ui
                            .button(label)
                            .on_hover_text(if self.show_editor {
                                "Hide the editing timeline (Space plays, Del deletes a selection)"
                            } else {
                                "Show the editing timeline: cut, trim, silence, fades"
                            })
                            .on_disabled_hover_text("Record a clip first — then you can edit it");
                        if r.clicked() {
                            if self.show_editor {
                                self.close_editor();
                            } else {
                                self.open_editor();
                            }
                        }
                    });
                });
            });
        });

        // ---------- left sidebar ----------
        egui::SidePanel::left("sidebar")
            .resizable(false)
            .default_width(264.0)
            .frame(
                egui::Frame::none()
                    .fill(theme::PANEL_SIDEBAR)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .inner_margin(egui::Margin::symmetric(6.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.label(theme::heading("⚙ Setup").size(19.0).strong());
                ui.add_space(4.0);

                // Vertical scrollbar for short windows: everything below the
                // title scrolls, the title itself stays put.
                egui::ScrollArea::vertical()
                    .id_salt("setup_sidebar_scroll")
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(
                        egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                    )
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
                        ui.style_mut().spacing.slider_width = 110.0;
                        ui.style_mut().spacing.combo_width = 150.0;

                        // ---- recording status ----
                        theme::titled_card(
                            ui,
                            theme::CARD_BLUE,
                            egui::RichText::new("⏺ Recording").color(theme::REC_TEXT).strong(),
                            |ui| {
                                theme::kv(
                                    ui,
                                    "State",
                                    match self.state {
                                        RecordingState::Idle => "Idle",
                                        RecordingState::SelectingArea => "Selecting area…",
                                        RecordingState::Recording => "Recording",
                                        RecordingState::Paused => "Paused",
                                    },
                                );
                                theme::kv(
                                    ui,
                                    "Duration",
                                    format_duration(self.get_recording_duration()),
                                );
                                theme::kv(ui, "Frames", self.current_frame_count.to_string());
                                if let Some((w, h)) = self.capture.get_dimensions() {
                                    theme::kv(ui, "Capturing", format!("{}×{}", w, h));
                                } else if let Some((_, _, w, h)) = self.capture_region {
                                    if self.capture_mode == CaptureMode::Region {
                                        theme::kv(ui, "Region", format!("{}×{}", w, h));
                                    }
                                }
                                theme::kv(ui, "Output FPS", self.target_fps.to_string());
                            },
                        );

                        ui.add_space(theme::CARD_GAP);

                        // ---- audio ----
                        theme::card(ui, theme::CARD_GREEN, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("🔊 Audio")
                                        .color(theme::CARD_TEXT_GREEN)
                                        .small()
                                        .strong(),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("⟳")
                                            .on_hover_text("Rescan audio devices")
                                            .clicked()
                                        {
                                            self.refresh_audio();
                                            self.status_message =
                                                Some("Audio devices rescanned.".into());
                                        }
                                    },
                                );
                            });
                            let rec = self.is_recording();
                            ui.add_enabled_ui(!rec, |ui| {
                                ui.checkbox(
                                    &mut self.record_system_audio,
                                    egui::RichText::new("System sound (speakers)").small(),
                                );
                                ui.checkbox(
                                    &mut self.record_mic,
                                    egui::RichText::new("Microphone").small(),
                                );
                                if self.record_mic {
                                    let mics = self.audio_devices.microphones.clone();
                                    let current =
                                        self.mic_device.clone().unwrap_or_else(|| "Default".into());
                                    egui::ComboBox::from_id_salt("mic_pick")
                                        .selected_text(theme::truncate(&current, 24))
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut self.mic_device, None, "Default");
                                            for m in mics {
                                                ui.selectable_value(
                                                    &mut self.mic_device,
                                                    Some(m.clone()),
                                                    m,
                                                );
                                            }
                                        });
                                }
                                ui.add(
                                    egui::Slider::new(&mut self.system_volume, 0.0..=1.5)
                                        .text("System vol"),
                                );
                                ui.add(
                                    egui::Slider::new(&mut self.mic_volume, 0.0..=1.5)
                                        .text("Mic vol"),
                                );
                            });
                            if rec {
                                theme::hint(ui, "Audio toggles lock while recording.");
                            }
                            // Live meters (also animate before recording when idle? show last levels or 0).
                            let (sys_lvl, mic_lvl) = match &self.audio_capture {
                                Some(cap) => (cap.system_level(), cap.mic_level()),
                                None => (0.0, 0.0),
                            };
                            if self.record_system_audio {
                                level_bar(ui, "Sys", sys_lvl, theme::METER_BLUE);
                            }
                            if self.record_mic {
                                level_bar(ui, "Mic", mic_lvl, theme::METER_GREEN);
                            }
                            if !self.record_system_audio && !self.record_mic {
                                theme::hint(ui, "Audio off — video only.");
                            }
                            if self.record_mic && self.audio_devices.microphones.is_empty() {
                                ui.label(
                                    egui::RichText::new(
                                        "No mic found — check connection / mic privacy, then ⟳.",
                                    )
                                    .color(theme::WARN_TEXT)
                                    .small(),
                                );
                            }
                            if let Some(sum) = &self.last_audio_result {
                                for w in &sum.warnings {
                                    ui.label(
                                        egui::RichText::new(format!("⚠ {w}"))
                                            .color(theme::ERROR_TEXT)
                                            .small(),
                                    );
                                }
                            }
                        });

                        ui.add_space(theme::CARD_GAP);

                        // ---- selection ----
                        theme::titled_card(
                            ui,
                            theme::CARD_AMBER,
                            egui::RichText::new("✂ Selection").color(theme::PAUSED).strong(),
                            |ui| {
                                let rec = self.is_recording();
                                if let Some((s, e)) = self.timeline_state.get_selection() {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{:.2}s → {:.2}s ({:.2}s)",
                                            s,
                                            e,
                                            e - s
                                        ))
                                        .small(),
                                    );
                                    ui.horizontal(|ui| {
                                        ui.add_enabled_ui(!rec, |ui| {
                                            if ui
                                                .small_button("Delete (Del)")
                                                .on_hover_text(
                                                    "Remove the selected range from the timeline",
                                                )
                                                .on_disabled_hover_text("Stop recording first")
                                                .clicked()
                                            {
                                                self.cut_selected_range();
                                            }
                                            if ui
                                                .small_button("Trim")
                                                .on_hover_text(
                                                    "Keep only the selected range, drop the rest",
                                                )
                                                .on_disabled_hover_text("Stop recording first")
                                                .clicked()
                                            {
                                                self.trim_selection();
                                            }
                                        });
                                        if ui
                                            .small_button("Clear")
                                            .on_hover_text("Deselect the current range")
                                            .clicked()
                                        {
                                            self.timeline_state.clear_selection();
                                        }
                                    });
                                } else {
                                    theme::hint(ui, "Drag on the timeline to select.");
                                }
                                if let Some(ph) = self.timeline_state.playhead {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Playhead: {}",
                                            crate::ui::format_tc(ph)
                                        ))
                                        .small(),
                                    );
                                }
                                ui.separator();
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Clips: {}  •  Total: {:.1}s",
                                        self.timeline.segments.len(),
                                        self.live_duration_secs()
                                    ))
                                    .small(),
                                );
                                if self.fade_in > 0.05 || self.fade_out > 0.05 {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "Fades: in {:.1}s / out {:.1}s",
                                            self.fade_in, self.fade_out
                                        ))
                                        .small(),
                                    );
                                }
                                if self.show_editor {
                                    theme::hint(ui, "Full editor tools are docked below the preview.");
                                } else if self.has_editable_video() && !self.is_recording() {
                                    if ui
                                        .small_button("✂ Edit video")
                                        .on_hover_text("Show the editing timeline (cut, trim, fades)")
                                        .clicked()
                                    {
                                        self.open_editor();
                                    }
                                } else {
                                    theme::hint(ui, "Record a clip, then press Edit video to edit.");
                                }
                            },
                        );

                        ui.add_space(theme::CARD_GAP);
                        theme::card(ui, theme::CARD_VIOLET, |ui| {
                            ui.label(
                                egui::RichText::new("💾 Save folder")
                                    .color(theme::CARD_TEXT_VIOLET)
                                    .small()
                                    .strong(),
                            );
                            let dir = self.output_dir.display().to_string();
                            ui.label(
                                egui::RichText::new(theme::truncate(&dir, 38))
                                    .color(theme::TEXT_MUTED)
                                    .small(),
                            )
                            .on_hover_text(&dir);
                            ui.horizontal(|ui| {
                                ui.add_enabled_ui(!self.is_recording(), |ui| {
                                    if ui
                                        .small_button("Browse…")
                                        .on_hover_text("Choose where new recordings are saved")
                                        .clicked()
                                    {
                                        if let Some(dir) =
                                            Self::pick_output_dir_dialog(&self.output_dir)
                                        {
                                            self.set_output_dir(dir);
                                        }
                                    }
                                });
                                if ui
                                    .small_button("Open")
                                    .on_hover_text("Open the save folder in Explorer")
                                    .clicked()
                                {
                                    let _ = std::process::Command::new("explorer")
                                        .arg(&self.output_dir)
                                        .spawn();
                                }
                            });
                            if self.is_recording() {
                                theme::hint(ui, "Locked while recording.");
                            } else {
                                theme::hint(ui, "Next: recording_<date>_<time>.mp4");
                            }
                        });
                        ui.add_space(2.0);
                        let last = format!("Last file: {}", self.output_path.display());
                        ui.label(
                            egui::RichText::new(theme::truncate(&last, 42))
                                .color(theme::TEXT_DIM)
                                .small(),
                        )
                        .on_hover_text(last);
                    });
            });

        // ---------- status bar (very bottom) ----------
        // Bottom panels are shown BEFORE the CentralPanel: egui's CentralPanel
        // does not shrink its available rect, so bottom panels shown after it
        // would overlap the preview (the audio timeline hid the video bottom).
        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::none()
                    .fill(theme::PANEL_STATUS)
                    .stroke(egui::Stroke::new(1.0_f32, theme::ACCENT_BORDER))
                    .inner_margin(egui::Margin::symmetric(8.0, 5.0)),
            )
            .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(msg) = &self.status_message {
                    ui.label(
                        egui::RichText::new(theme::truncate(msg, 140)).color(theme::ACCENT_DARK),
                    )
                    .on_hover_text(msg);
                } else {
                    ui.label("Ready");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(err) = &self.error_message {
                        ui.colored_label(theme::ERROR_TEXT, format!("⚠ {}", err));
                        if ui
                            .add(theme::icon_btn("✕"))
                            .on_hover_text("Dismiss this error")
                            .clicked()
                        {
                            self.error_message = None;
                            self.error_seen = None;
                        }
                    }
                });
            });
        });

        // ---------- editor dock: only when the user chooses Edit video ----------
        // Shown after the status bar so it stacks above it (each later bottom
        // panel reserves space on top of the previous one).
        if self.show_editor {
            let is_rec = matches!(self.state, RecordingState::Recording | RecordingState::Paused);
            let live_opt = if is_rec {
                Some(self.get_recording_duration().as_secs_f64())
            } else {
                None
            };
            let total = self.timeline.total_duration.max(live_opt.unwrap_or(0.0));
            let has_timeline = total > 0.05;
            let has_sel = self.timeline_state.has_selection();
            let playing = self.timeline_state.is_playing;
            egui::TopBottomPanel::bottom("editor_dock")
                .resizable(false)
                .default_height(296.0)
                .frame(
                    egui::Frame::none()
                        .fill(theme::PANEL_EDITOR)
                        .stroke(egui::Stroke::new(1.0_f32, theme::CARD_AMBER))
                        .inner_margin(theme::panel_margin()),
                )
                .show(ctx, |ui| {
                    // Toolbar (ribbon row).
                    ui.horizontal_wrapped(|ui| {
                        let save = theme::primary_btn("💾 Save and Close");
                        if ui
                            .add_sized([150.0, theme::BTN_H], save)
                            .on_hover_text("Export the edited timeline to a new MP4 (volume + fades applied)")
                            .clicked()
                        {
                            self.save_edited_copy();
                        }
                        ui.separator();
                        ui.add_enabled_ui(has_sel && !is_rec, |ui| {
                            if ui
                                .button("❌ Delete")
                                .on_hover_text("Delete the selected range (Del)")
                                .on_disabled_hover_text(if is_rec {
                                    "Stop recording first"
                                } else {
                                    "Select a range on the timeline first"
                                })
                                .clicked()
                            {
                                self.cut_selected_range();
                            }
                            if ui
                                .button("✂ Trim")
                                .on_hover_text("Keep only the selected range, drop the rest")
                                .on_disabled_hover_text(if is_rec {
                                    "Stop recording first"
                                } else {
                                    "Select a range on the timeline first"
                                })
                                .clicked()
                            {
                                self.trim_selection();
                            }
                        });
                        ui.add_enabled_ui(!is_rec && has_timeline && !self.waveform.is_empty(), |ui| {
                            if ui
                                .button("🔇 Silence")
                                .on_hover_text("Auto-remove silent parts (−44 dB threshold, 0.4 s minimum gap)")
                                .on_disabled_hover_text(if is_rec {
                                    "Stop recording first"
                                } else {
                                    "Needs a recorded clip with audio"
                                })
                                .clicked()
                            {
                                self.delete_silence(0.12, 0.4);
                            }
                        });
                        if ui
                            .button(format!("🔊 Volume ×{:.2}", self.master_volume))
                            .on_hover_text("Master volume + fades, applied when you Save")
                            .clicked()
                        {
                            self.show_volume_popup = !self.show_volume_popup;
                        }
                        if ui
                            .button(format!("Fade In {:.1}s", self.fade_in))
                            .on_hover_text("Click to cycle the fade-in: off → 0.5s → 1s → 2s")
                            .clicked()
                        {
                            self.fade_in = match self.fade_in {
                                x if x < 0.1 => 0.5,
                                x if x < 0.75 => 1.0,
                                x if x < 1.5 => 2.0,
                                _ => 0.0,
                            };
                        }
                        if ui
                            .button(format!("Fade Out {:.1}s", self.fade_out))
                            .on_hover_text("Click to cycle the fade-out: off → 0.5s → 1s → 2s")
                            .clicked()
                        {
                            self.fade_out = match self.fade_out {
                                x if x < 0.1 => 0.5,
                                x if x < 0.75 => 1.0,
                                x if x < 1.5 => 2.0,
                                _ => 0.0,
                            };
                        }
                        ui.separator();
                        ui.add_enabled_ui(has_timeline, |ui| {
                            if ui
                                .button("🔍 Zoom Selection")
                                .on_hover_text("Fit the selected range to the visible width")
                                .clicked()
                            {
                                self.timeline_state.zoom_to_selection(total);
                            }
                            if ui
                                .button("↔ Show All")
                                .on_hover_text("Zoom out so the whole timeline fits (zoom 1×)")
                                .clicked()
                            {
                                self.timeline_state.show_all();
                            }
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("✕ Close")
                                .on_hover_text(
                                    "Hide the editing timeline (the video and clips are kept)",
                                )
                                .clicked()
                            {
                                self.close_editor();
                            }
                        });
                    });
                    ui.separator();

                    // Tracks.
                    let playhead = self.timeline_state.playhead;
                    let thumb_times: Vec<f64> = self.filmstrip.iter().map(|(t, _)| *t).collect();
                    let w = ui.available_width();
                    let timeline = &self.timeline;
                    let tracks = crate::ui::TimelineTracks {
                        thumb_textures: &self.thumb_textures,
                        thumb_times: &thumb_times,
                        waveform: &self.waveform,
                        waveform_rate: self.waveform_rate,
                        playhead,
                        fade_in: self.fade_in,
                        fade_out: self.fade_out,
                        total,
                    };
                    crate::ui::draw_timeline(ui, timeline, &mut self.timeline_state, w, 168.0, live_opt, is_rec, &tracks);

                    // Transport.
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(has_timeline && !is_rec, |ui| {
                            if ui
                                .add(theme::icon_btn("⏮"))
                                .on_hover_text("Go to start (or to the selection start)")
                                .clicked()
                            {
                                self.stop_playback();
                                // With a range selected, "start" is its head.
                                let target = self
                                    .timeline_state
                                    .get_selection()
                                    .filter(|(s, e)| e - s > 0.05)
                                    .map(|(s, _)| s)
                                    .unwrap_or(0.0);
                                self.timeline_state.playhead = Some(target);
                            }
                            let range_selected = self
                                .timeline_state
                                .get_selection()
                                .is_some_and(|(s, e)| e - s > 0.05);
                            let play_hint = if range_selected {
                                "Play only the selected range (Space)"
                            } else {
                                "Play / pause preview (Space)"
                            };
                            if ui
                                .add(theme::icon_btn(if playing { "⏸" } else { "▶" }))
                                .on_hover_text(play_hint)
                                .clicked()
                            {
                                self.toggle_play();
                            }
                            if ui
                                .add(theme::icon_btn("⏹"))
                                .on_hover_text("Stop playback (Space)")
                                .clicked()
                            {
                                self.stop_playback();
                            }
                        });
                        let cur = self.timeline_state.playhead.unwrap_or(0.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{} / {}",
                                crate::ui::format_tc(cur),
                                crate::ui::format_tc(total)
                            ))
                            .monospace(),
                        )
                        .on_hover_text("Playhead / total duration");
                        ui.label(
                            egui::RichText::new(format!(
                                "Total duration: {}",
                                format_duration(Duration::from_secs_f64(total.max(0.0)))
                            ))
                            .color(theme::TEXT_DIM),
                        );
                        ui.label(
                            egui::RichText::new("Space play • Del delete • +/− zoom • drag select")
                                .color(theme::TEXT_DIM)
                                .small(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("Open folder")
                                .on_hover_text("Open the folder containing the last export")
                                .clicked()
                            {
                                if let Some(parent) = self.output_path.parent() {
                                    let _ = std::process::Command::new("explorer").arg(parent).spawn();
                                }
                            }
                            ui.label(
                                egui::RichText::new(format!(
                                    "Clips: {} • Thumbs: {}",
                                    self.timeline.segments.len(),
                                    self.filmstrip.len()
                                ))
                                .color(theme::TEXT_DIM),
                            );
                            let mut zoom = self.timeline_state.zoom;
                            let zoom_label = format!("{:.0}×", zoom);
                            ui.add(
                                egui::Slider::new(&mut zoom, 1.0..=20.0)
                                    .show_value(false)
                                    .text(zoom_label),
                            )
                            .on_hover_text("Timeline zoom (also + / − with the editor focused)");
                            self.timeline_state.zoom = zoom;
                            self.timeline_state.clamp_view(total.max(0.1));
                        });
                    });
                });
        }

        // ---------- center: preview only (timeline is docked below) ----------
        let has_recorded_video = self.has_editable_video();
        if self.is_recording() || has_recorded_video {
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::none()
                        .fill(theme::PANEL_CENTER)
                        .inner_margin(egui::Margin::same(8.0)),
                )
                .show(ctx, |ui| {
                egui::Frame::group(ui.style())
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::new(1.5_f32, theme::ACCENT))
                    .rounding(egui::Rounding::same(12.0))
                    .show(ui, |ui| {
                    if self.is_recording() {
                        // Live preview during recording
                        if let Some(tex) = &self.preview_texture {
                            let (fw, fh) = self.preview_size.unwrap_or((16, 9));
                            let aspect = fh as f32 / fw.max(1) as f32;
                            let id = tex.id();
                            show_preview_fit(ui, id, aspect, 1.0);
                        } else {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 220.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 8.0, theme::PLACEHOLDER_BG);
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "Waiting for first frame…",
                                egui::FontId::proportional(14.0),
                                theme::TEXT_DIM,
                            );
                        }
                    } else {
                        // Playback preview from timeline
                        let playing_now = self.timeline_state.is_playing;
                        let last_thumb_t = self.filmstrip.last().map(|(t, _)| *t);
                        let parked_at_end = match (self.timeline_state.playhead, last_thumb_t) {
                            (Some(ph), Some(last_t)) => ph >= last_t,
                            _ => true,
                        };
                        // The frozen "last take" frame only makes sense once
                        // playback has stopped and the playhead is parked.
                        let show_still = !playing_now
                            && parked_at_end
                            && self.preview_texture.is_some();
                        let show_still_src: Option<(egui::TextureId, f32)> = if show_still {
                            self.preview_texture.as_ref().map(|tex| {
                                let (fw, fh) = self.preview_size.unwrap_or((16, 9));
                                (tex.id(), fh as f32 / fw.max(1) as f32)
                            })
                        } else {
                            None
                        };

                        // Newest frame from the background decoder (real playback).
                        let stream: Option<(egui::TextureId, f32)> = match &self.play_tex {
                            Some((t, g, aspect, tex)) if *g == self.media_gen => {
                                match self.timeline_state.playhead {
                                    Some(ph) => {
                                        let fresh = if playing_now {
                                            *t <= ph + 0.35 && ph - *t < 1.0
                                        } else {
                                            (*t - ph).abs() < 0.4
                                        };
                                        fresh.then(|| (tex.id(), *aspect))
                                    }
                                    None => None,
                                }
                            }
                            _ => None,
                        };

                        let scrub: Option<(egui::TextureId, f32)> = if !show_still {
                            if let Some(ph) = self.timeline_state.playhead {
                                if !self.thumb_textures.is_empty() {
                                    let mut idx = 0usize;
                                    for (i, (tt, _)) in self.filmstrip.iter().enumerate() {
                                        if *tt <= ph {
                                            idx = i;
                                        } else {
                                            break;
                                        }
                                    }
                                    self.thumb_textures.get(idx).map(|h| h.id()).and_then(|id| {
                                        let size = self.filmstrip.get(idx).map(|(_, img)| img.size)?;
                                        (size[0] > 0 && size[1] > 0).then(|| (id, size[1] as f32 / size[0] as f32))
                                    })
                                } else { None }
                            } else { None }
                        } else { None };

                        let scrub_still: Option<(egui::TextureId, f32)> = match &self.still_tex {
                            Some((t, g, aspect, tex)) if *g == self.media_gen => {
                                match self.timeline_state.playhead {
                                    Some(ph) => {
                                        let tol = if self.timeline_state.is_playing { 1.5 } else { 0.3 };
                                        (*t <= ph + 0.05 && ph - *t < tol).then(|| (tex.id(), *aspect))
                                    }
                                    _ => None,
                                }
                            }
                            _ => None,
                        };

                        ui.horizontal(|ui| {
                            ui.heading(theme::heading("👁 Preview"));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if self.timeline_state.is_playing {
                                    if let Some(ph) = self.timeline_state.playhead {
                                        ui.label(egui::RichText::new(format!("▶ {}", crate::ui::format_tc(ph))).strong());
                                    } else {
                                        ui.label(egui::RichText::new("▶ playing").strong());
                                    }
                                } else if show_still {
                                    ui.label(egui::RichText::new("last take").color(theme::TEXT_DIM));
                                } else if scrub_still.is_some() {
                                    ui.label(egui::RichText::new("scrub HD").color(theme::TEXT_DIM));
                                } else if let Some(ph) = self.timeline_state.playhead {
                                    if !self.filmstrip.is_empty() {
                                        ui.label(egui::RichText::new(format!("scrub {}", crate::ui::format_tc(ph))).color(theme::TEXT_DIM));
                                    } else {
                                        ui.label(egui::RichText::new("no signal").color(theme::TEXT_DIM));
                                    }
                                } else {
                                    ui.label(egui::RichText::new("no signal").color(theme::TEXT_DIM));
                                }
                                // Current zoom level — only when actually magnified,
                                // so the default "fit" view stays clean.
                                if self.preview_zoom > 1.01 {
                                    ui.label(
                                        egui::RichText::new(format!("{:.0}%", self.preview_zoom * 100.0))
                                            .color(theme::ACCENT_DARK)
                                            .strong(),
                                    );
                                }
                                // Right-to-left: added later = further left, so
                                // the buttons sit left of the status label.
                                ui.add(theme::icon_btn("+"))
                                    .on_hover_text("Zoom in (up to 800%)")
                                    .clicked()
                                    .then(|| self.preview_zoom = (self.preview_zoom * 1.4).min(8.0));
                                ui.add(theme::icon_btn("⛶"))
                                    .on_hover_text("Fit preview (100%)")
                                    .clicked()
                                    .then(|| self.preview_zoom = 1.0);
                                ui.add(theme::icon_btn("−"))
                                    .on_hover_text("Zoom out (min 100%)")
                                    .clicked()
                                    .then(|| self.preview_zoom = (self.preview_zoom / 1.4).max(1.0));
                            });
                        });

                        // Priority: while playing, the live stream wins (the
                        // chase stills only arrive ~1/s); when paused, prefer
                        // the sharp still / last-take frame and fall back to
                        // the streamed frame while those decode.
                        let shown: Option<(egui::TextureId, f32)> = if playing_now {
                            [stream, scrub_still, scrub, show_still_src]
                        } else {
                            [scrub_still, show_still_src, stream, scrub]
                        }
                        .into_iter()
                        .flatten()
                        .next();

                        if let Some((tid, aspect)) = shown {
                            let disp_w = show_preview_fit(ui, tid, aspect, self.preview_zoom);
                            // Decode at what's actually on screen; window/sidebar
                            // resizes and zoom changes re-spawn the decoder to
                            // match (debounced in maybe_retarget_player).
                            let src_w = self.timeline.width.div_ceil(2) * 2;
                            self.preview_wanted_w = disp_w.ceil().max(320.0) as u32;
                            if src_w > 0 {
                                self.preview_wanted_w = self.preview_wanted_w.min(src_w);
                            }
                        } else {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 220.0), egui::Sense::hover());
                            ui.painter().rect_filled(rect, 8.0, theme::PLACEHOLDER_BG);
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "No video at this position",
                                egui::FontId::proportional(14.0),
                                theme::TEXT_DIM,
                            );
                        }
                    }
                });

                // Entry point to the (hidden-by-default) editing timeline.
                if !self.show_editor && !self.is_recording() && has_recorded_video {
                    ui.add_space(6.0);
                    ui.vertical_centered(|ui| {
                        if ui
                            .add(theme::primary_btn("✂ Edit video"))
                            .on_hover_text("Show the editing timeline (cut, trim, fades)")
                            .clicked()
                        {
                            self.open_editor();
                        }
                    });
                }
            });
        } else {
            // No preview panel when not recording and no recorded video
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::none()
                        .fill(theme::PANEL_TOOLBAR)
                        .inner_margin(egui::Margin::same(8.0)),
                )
                .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(48.0);
                    ui.heading(theme::heading("🐦 Pipit Screen Recorder"));
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("No video recorded yet")
                            .color(theme::BRAND)
                            .size(16.0),
                    );
                    ui.add_space(6.0);
                    theme::hint(ui, "Pick a source in the toolbar, then record.");
                    ui.add_space(18.0);
                    // The primary action lives here too — an empty screen
                    // should offer the obvious next step instead of just
                    // describing it.
                    let rec = theme::record_btn("●  Record now");
                    if ui.add_sized([170.0, 40.0], rec).clicked() {
                        if let Err(e) = self.start_recording(ctx) {
                            self.error_message = Some(e);
                        }
                    }
                    ui.add_space(6.0);
                    theme::hint(ui, "Esc stops a recording");
                });
            });
        }

        // Shortcuts apply to the editing timeline, so only when visible.
        // `wants_keyboard_input` keeps Space/+/- from firing while a slider,
        // drag value or text field has keyboard focus.
        let typing = ctx.wants_keyboard_input();
        if (self.show_settings || self.show_volume_popup)
            && !typing
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.show_settings = false;
            self.show_volume_popup = false;
        }
        if self.show_editor && !typing {
            if ctx.input(|i| i.key_pressed(egui::Key::Delete)) && self.timeline_state.has_selection() {
                self.cut_selected_range();
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                self.toggle_play();
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Plus) | i.key_pressed(egui::Key::Equals)) {
                let total = self.timeline.total_duration.max(0.1);
                self.timeline_state.zoom_by(1.25, total);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Minus)) {
                let total = self.timeline.total_duration.max(0.1);
                self.timeline_state.zoom_by(0.8, total);
            }
        }

        if self.show_settings {
            // Edit via locals to avoid double-borrowing `self` in the closure.
            let mut fps = self.target_fps;
            let mut rec_sys = self.record_system_audio;
            let mut rec_mic = self.record_mic;
            let mut sys_vol = self.system_volume;
            let mut mic_vol = self.mic_volume;
            let mic_name = self.mic_device.clone().unwrap_or_else(|| "Default".into());
            let out_folder = self.output_dir.display().to_string();
            let rec_locked = self.is_recording();
            let mut rescan = false;
            let mut browse_output = false;
            let mut open = self.show_settings;
            egui::Window::new("Settings")
                .open(&mut open)
                .resizable(true)
                .default_width(360.0)
                .min_size([320.0, 260.0])
                .show(ctx, |ui| {
                    ui.heading(theme::heading("Video"));
                    ui.add(egui::Slider::new(&mut fps, 15..=60).text("FPS"));
                    ui.separator();
                    ui.heading(theme::heading("Audio"));
                    ui.checkbox(&mut rec_sys, "Record system sound");
                    ui.checkbox(&mut rec_mic, "Record microphone");
                    ui.add(egui::Slider::new(&mut sys_vol, 0.0..=1.5).text("System volume"));
                    ui.add(egui::Slider::new(&mut mic_vol, 0.0..=1.5).text("Mic volume"));
                    if ui
                        .button("Rescan audio devices")
                        .on_hover_text("Re-plugged a mic or speaker? Rescan to pick it up")
                        .clicked()
                    {
                        rescan = true;
                    }
                    ui.label(egui::RichText::new(format!("Mic: {}", theme::truncate(&mic_name, 44))).color(theme::TEXT_MUTED))
                        .on_hover_text(format!("Mic: {mic_name}"));
                    ui.separator();
                    ui.heading(theme::heading("Output"));
                    ui.label(egui::RichText::new(format!("Folder: {}", theme::truncate(&out_folder, 48))).color(theme::TEXT_MUTED))
                        .on_hover_text(format!("Folder: {out_folder}"));
                    ui.add_enabled_ui(!rec_locked, |ui| {
                        if ui.button("Browse…").on_hover_text("Choose where new recordings are saved").clicked() {
                            browse_output = true;
                        }
                    });
                    if rec_locked {
                        theme::hint(ui, "Locked while recording.");
                    }
                    ui.separator();
                    theme::hint(
                        ui,
                        "Pipit Screen Recorder — Rust + egui • video via windows-capture + ffmpeg • audio via WASAPI",
                    );
                });
            self.show_settings = open;
            self.target_fps = fps;
            self.record_system_audio = rec_sys;
            self.record_mic = rec_mic;
            self.system_volume = sys_vol;
            self.mic_volume = mic_vol;
            if rescan {
                self.refresh_audio();
            }
            if browse_output {
                if let Some(dir) = Self::pick_output_dir_dialog(&self.output_dir) {
                    self.set_output_dir(dir);
                }
            }
        }

        if self.show_volume_popup {
            let mut vol = self.master_volume;
            let mut fi = self.fade_in;
            let mut fo = self.fade_out;
            let total = self.timeline.total_duration;
            let mut open = self.show_volume_popup;
            egui::Window::new("🔊 Volume")
                .open(&mut open)
                .resizable(false)
                .default_width(300.0)
                .show(ctx, |ui| {
                    ui.add(egui::Slider::new(&mut vol, 0.0..=2.0).text("Master volume"));
                    ui.add(egui::Slider::new(&mut fi, 0.0..=5.0).text("Fade in (s)"));
                    ui.add(egui::Slider::new(&mut fo, 0.0..=5.0).text("Fade out (s)"));
                    ui.separator();
                    theme::hint(ui, format!("Timeline: {:.1}s", total));
                    theme::hint(ui, "Applied when you press Save and Close.");
                });
            self.show_volume_popup = open;
            self.master_volume = vol;
            self.fade_in = fi;
            self.fade_out = fo;
        }

        // Repaint budget: hold 60 fps only while something is actually
        // animating (live capture, playback, a scrub decode in flight, the
        // area-selector overlay). An idle editor drops to ~4 fps — egui
        // still repaints instantly on any input, so nothing feels laggy and
        // the loop stops burning CPU when the app is just sitting there.
        let animating = self.is_recording()
            || self.timeline_state.is_playing
            || self.still_rx.is_some()
            || self.show_area_selector;
        ctx.request_repaint_after(Duration::from_millis(if animating { 16 } else { 250 }));
    }
}
