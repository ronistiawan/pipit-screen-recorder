use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_capture::capture::{Context as CaptureContext, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame as CaptureFrame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

pub struct ScreenCapture {
    inner: parking_lot::Mutex<Option<CaptureInner>>,
}

struct CaptureInner {
    control: windows_capture::capture::CaptureControl<CaptureHandler, String>,
    frame_receiver: crossbeam_channel::Receiver<FrameData>,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug)]
pub struct FrameData {
    pub buffer: Vec<u8>,
    pub width: u32,
    pub height: u32,
    #[allow(dead_code)]
    pub timestamp: Instant,
}

/// Crop rectangle in pixels, relative to the captured monitor's top-left.
#[derive(Clone, Copy, Debug)]
struct CropRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

struct CaptureFlags {
    sender: crossbeam_channel::Sender<FrameData>,
    crop: Option<CropRect>,
}

struct CaptureHandler {
    sender: crossbeam_channel::Sender<FrameData>,
    crop: Option<CropRect>,
    scratch: Vec<u8>,
}

fn crop_rgba(src: &[u8], sw: u32, sh: u32, c: CropRect) -> Option<(Vec<u8>, u32, u32)> {
    let x = c.x.min(sw);
    let y = c.y.min(sh);
    let w = c.w.min(sw.saturating_sub(x));
    let h = c.h.min(sh.saturating_sub(y));
    if w == 0 || h == 0 {
        return None;
    }
    let row_bytes = (w as usize) * 4;
    let mut out = vec![0u8; row_bytes * (h as usize)];
    for row in 0..h as usize {
        let src_off = ((y as usize + row) * (sw as usize) + (x as usize)) * 4;
        let dst_off = row * row_bytes;
        out[dst_off..dst_off + row_bytes]
            .copy_from_slice(&src[src_off..src_off + row_bytes]);
    }
    Some((out, w, h))
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = CaptureFlags;
    type Error = String;

    fn new(ctx: CaptureContext<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            sender: ctx.flags.sender,
            crop: ctx.flags.crop,
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut CaptureFrame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let fb = frame.buffer().map_err(|e| e.to_string())?;
        let (fw, fh) = (fb.width(), fb.height());
        // Tightly-packed copy (drops GPU row padding, if any).
        let tight = fb.as_nopadding_buffer(&mut self.scratch).to_vec();

        let (data, w, h) = match self.crop {
            Some(c) => match crop_rgba(&tight, fw, fh, c) {
                Some(v) => v,
                None => return Ok(()), // region outside frame; skip
            },
            None => (tight, fw, fh),
        };

        // Never block the capture thread; drop the frame if the UI is behind.
        let _ = self.sender.try_send(FrameData {
            buffer: data,
            width: w,
            height: h,
            timestamp: Instant::now(),
        });
        Ok(())
    }
}

impl ScreenCapture {
    pub fn new() -> Self {
        Self {
            inner: parking_lot::Mutex::new(None),
        }
    }

    fn start_item<T>(&self, item: T, crop: Option<CropRect>, width: u32, height: u32) -> Result<()>
    where
        T: TryInto<GraphicsCaptureItemType> + Send + 'static,
        T::Error: std::fmt::Debug,
    {
        self.stop();
        let (tx, rx) = crossbeam_channel::unbounded();
        let settings = Settings::new(
            item,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(Duration::from_millis(16)),
            DirtyRegionSettings::ReportAndRender,
            ColorFormat::Rgba8,
            CaptureFlags { sender: tx, crop },
        );
        let control = CaptureHandler::start_free_threaded(settings)
            .map_err(|e| anyhow::anyhow!("failed to start screen capture: {e:?}"))?;
        *self.inner.lock() = Some(CaptureInner {
            control,
            frame_receiver: rx,
            width,
            height,
        });
        Ok(())
    }

    pub async fn start_monitor_capture(&self, monitor_index: usize) -> Result<()> {
        let monitors = Monitor::enumerate().context("failed to list monitors")?;
        let monitor = monitors
            .into_iter()
            .nth(monitor_index)
            .ok_or_else(|| anyhow::anyhow!("monitor {monitor_index} not found"))?;
        let (w, h) = (
            monitor.width().context("failed to read monitor width")?,
            monitor.height().context("failed to read monitor height")?,
        );
        self.start_item(monitor, None, w, h)
    }

    pub async fn start_window_capture(&self, window_title: &str) -> Result<()> {
        let windows = Window::enumerate().context("failed to list windows")?;
        let needle = window_title.to_lowercase();
        let window = windows
            .into_iter()
            .find(|w| w.title().map(|t| t.to_lowercase().contains(&needle)).unwrap_or(false))
            .ok_or_else(|| anyhow::anyhow!("window '{window_title}' not found"))?;
        let (w, h) = (
            window.width().unwrap_or(0).max(0) as u32,
            window.height().unwrap_or(0).max(0) as u32,
        );
        self.start_item(window, None, w, h)
    }

    pub async fn start_region_capture(&self, x: i32, y: i32, w: u32, h: u32) -> Result<()> {
        // windows-capture grabs a whole monitor; crop the region out of it.
        // (Monitor has no position API, so the region is interpreted relative
        // to the primary monitor's top-left — correct on single-monitor PCs.)
        let monitor = Monitor::primary().context("no primary monitor found")?;
        let (mw, mh) = (
            monitor.width().context("failed to read monitor width")?,
            monitor.height().context("failed to read monitor height")?,
        );
        let cx = x.max(0) as u32;
        let cy = y.max(0) as u32;
        let cw = w.min(mw.saturating_sub(cx));
        let ch = h.min(mh.saturating_sub(cy));
        if cw == 0 || ch == 0 {
            anyhow::bail!("region ({x},{y} {w}x{h}) is outside the primary monitor ({mw}x{mh})");
        }
        self.start_item(monitor, Some(CropRect { x: cx, y: cy, w: cw, h: ch }), cw, ch)
    }

    pub fn try_recv_frame(&self) -> Option<FrameData> {
        self.inner.lock().as_ref()?.frame_receiver.try_recv().ok()
    }

    /// Grab a single still frame of the primary monitor for the area-selection
    /// overlay. Blocks the calling thread (first frame + a short settle drain
    /// so we don't end up showing a blank startup frame).
    pub fn grab_primary_screenshot(&self, timeout: Duration) -> Result<FrameData> {
        let monitor = Monitor::primary().context("no primary monitor found")?;
        let (mw, mh) = (
            monitor.width().context("failed to read monitor width")?,
            monitor.height().context("failed to read monitor height")?,
        );
        self.start_item(monitor, None, mw, mh)?;

        let deadline = Instant::now() + timeout;
        let mut last: Option<FrameData> = None;
        while Instant::now() < deadline {
            let mut got_any = false;
            while let Some(f) = self.try_recv_frame() {
                last = Some(f);
                got_any = true;
            }
            if got_any {
                // Let one more burst arrive and keep the freshest frame.
                std::thread::sleep(Duration::from_millis(250));
                while let Some(f) = self.try_recv_frame() {
                    last = Some(f);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.stop();
        last.context("timed out waiting for a screen frame — is the desktop locked?")
    }

    pub fn stop(&self) {
        if let Some(inner) = self.inner.lock().take() {
            let _ = inner.control.stop();
        }
    }

    #[allow(dead_code)]
    pub fn is_capturing(&self) -> bool {
        self.inner.lock().is_some()
    }

    pub fn get_dimensions(&self) -> Option<(u32, u32)> {
        self.inner.lock().as_ref().map(|i| (i.width, i.height))
    }
}

impl Default for ScreenCapture {
    fn default() -> Self {
        Self::new()
    }
}

pub fn get_monitors() -> Result<Vec<MonitorInfo>> {
    let monitors = Monitor::enumerate().context("failed to list monitors")?;
    Ok(monitors
        .into_iter()
        .enumerate()
        .filter_map(|(i, m)| {
            Some(MonitorInfo {
                index: m.index().unwrap_or(i),
                name: m.name().unwrap_or_else(|_| format!("Monitor {}", i + 1)),
                width: m.width().ok()?,
                height: m.height().ok()?,
                x: 0,
                y: 0,
            })
        })
        .collect())
}

pub fn get_windows() -> Result<Vec<WindowInfo>> {
    let windows = Window::enumerate().context("failed to list windows")?;
    Ok(windows
        .into_iter()
        .filter_map(|w| {
            let title = w.title().ok()?;
            if title.trim().is_empty() {
                return None;
            }
            Some(WindowInfo {
                title,
                width: w.width().unwrap_or(0).max(0) as u32,
                height: w.height().unwrap_or(0).max(0) as u32,
                x: 0,
                y: 0,
            })
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    #[allow(dead_code)]
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    #[allow(dead_code)]
    pub x: i32,
    #[allow(dead_code)]
    pub y: i32,
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub title: String,
    pub width: u32,
    pub height: u32,
    #[allow(dead_code)]
    pub x: i32,
    #[allow(dead_code)]
    pub y: i32,
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<ScreenCapture>();
    let _ = Arc::new(ScreenCapture::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_real_frames_from_primary_monitor() {
        let cap = ScreenCapture::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(cap.start_monitor_capture(0))
            .expect("start capture on monitor 0");
        // WGC can deliver blank frames at startup; collect several and judge
        // the richest one.
        let deadline = Instant::now() + std::time::Duration::from_secs(8);
        let mut best: Option<(FrameData, usize)> = None;
        while Instant::now() < deadline {
            while let Some(f) = cap.try_recv_frame() {
                assert!(f.width > 0 && f.height > 0);
                assert_eq!(
                    f.buffer.len(),
                    (f.width * f.height * 4) as usize,
                    "frame bytes must match dimensions"
                );
                let distinct = f
                    .buffer
                    .chunks_exact(4)
                    .step_by(997)
                    .take(4096)
                    .collect::<std::collections::HashSet<_>>()
                    .len();
                if best.as_ref().map(|(_, d)| distinct > *d).unwrap_or(true) {
                    best = Some((f, distinct));
                }
                if distinct > 4 {
                    break;
                }
            }
            if best.as_ref().map(|(_, d)| *d > 4).unwrap_or(false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        cap.stop();
        let (f, distinct) =
            best.expect("no frame arrived within 8s — is the desktop unlocked?");
        // Real screen content is never a flat color; this guards against
        // regressions to synthetic mock frames.
        assert!(
            distinct > 4,
            "frames look synthetic, best had only {distinct} distinct pixels ({}x{})",
            f.width,
            f.height
        );
    }

    /// Diagnostics: proves `WDA_EXCLUDEFROMCAPTURE` removes a window from the
    /// WGC frames this crate delivers — the mechanism that keeps the red area
    /// marker (and the floating controller) out of the recorded video.
    ///
    /// Needs an unlocked desktop:
    /// `cargo test diag_exclude_from_capture -- --ignored --nocapture`
    #[test]
    #[ignore = "needs an interactive desktop"]
    fn diag_exclude_from_capture() {
        use windows::core::{HSTRING, w};
        use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND};
        use windows::Win32::Graphics::Gdi::CreateSolidBrush;
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
            PeekMessageW, RegisterClassExW, ShowWindow, SW_SHOW, TranslateMessage,
            WINDOW_EX_STYLE, WNDCLASSEXW, WS_POPUP, WS_VISIBLE,
        };

        const CLASS: windows::core::PCWSTR = w!("PipitDiagExcludeClass");
        const TITLE: &str = "Pipit diag exclude window";

        fn pump(ms: u64) {
            let end = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < end {
                unsafe {
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0, PM_REMOVE)
                        .as_bool()
                    {
                        let _ = TranslateMessage(&msg);
                        let _ = DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        unsafe extern "system" fn wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: windows::Win32::Foundation::WPARAM,
            lparam: windows::Win32::Foundation::LPARAM,
        ) -> windows::Win32::Foundation::LRESULT {
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }

        fn magenta_px(f: &FrameData) -> usize {
            f.buffer
                .chunks_exact(4)
                .filter(|p| p[0] > 250 && p[1] < 5 && p[2] > 250)
                .count()
        }

        let hinstance =
            HINSTANCE(unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW").0);
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            // Pure magenta never occurs in real screen content.
            hbrBackground: unsafe { CreateSolidBrush(COLORREF(0x00FF_00FF)) },
            lpszClassName: CLASS,
            ..Default::default()
        };
        let atom = unsafe { RegisterClassExW(&class) };
        assert!(atom != 0, "RegisterClassExW failed");

        let title = HSTRING::from(TITLE);
        let hwnd = unsafe {
            CreateWindowExW(
                // Topmost so no other window can cover the test square.
                WINDOW_EX_STYLE(0x0000_0008 | 0x0000_0080), // WS_EX_TOPMOST | WS_EX_TOOLWINDOW
                CLASS,
                &title,
                WS_POPUP | WS_VISIBLE,
                200,
                200,
                320,
                240,
                HWND(std::ptr::null_mut()),
                windows::Win32::UI::WindowsAndMessaging::HMENU(std::ptr::null_mut()),
                hinstance,
                None,
            )
            .expect("CreateWindowExW")
        };
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        pump(600);

        let cap = ScreenCapture::new();
        let frame = cap
            .grab_primary_screenshot(Duration::from_secs(6))
            .expect("capture with window visible");
        let before = magenta_px(&frame);
        if before <= 1000 {
            use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindowVisible};
            let (vis, rect) = unsafe {
                let mut r = windows::Win32::Foundation::RECT::default();
                let ok = GetWindowRect(hwnd, &mut r);
                (ok.is_ok() && IsWindowVisible(hwnd).as_bool(), r)
            };
            let distinct = frame
                .buffer
                .chunks_exact(4)
                .step_by(997)
                .take(4096)
                .collect::<std::collections::HashSet<_>>()
                .len();
            panic!(
                "control: test window not on screen ({before} magenta px); \
                 visible={vis} rect=({},{})-({},{}) frame={}x{} distinct={distinct}",
                rect.left, rect.top, rect.right, rect.bottom, frame.width, frame.height
            );
        }

        // The production helper: finds the window (title + our PID) and sets
        // WDA_EXCLUDEFROMCAPTURE on it.
        assert!(
            crate::app::exclude_window_from_capture(TITLE),
            "exclude_window_from_capture failed to find/exclude our own window"
        );
        pump(300);

        let after = magenta_px(
            &cap.grab_primary_screenshot(Duration::from_secs(6))
                .expect("capture after exclusion"),
        );
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        assert_eq!(
            after, 0,
            "excluded window still visible in capture ({after} magenta px)"
        );
        eprintln!("diag_exclude_from_capture: {before} px visible before, {after} px after");
    }
}
