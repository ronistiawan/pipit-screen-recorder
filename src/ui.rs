use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use crate::theme;

/// Drag state for the fullscreen area-selection overlay.
///
/// Pointer positions are kept in the overlay viewport's local points; the
/// caller maps the finished rect to screen pixels through the frozen
/// screenshot image (rect fraction × image size), so DPI scaling and the
/// overlay window's position never leak into the recorded region.
pub struct AreaSelector {
    pub start_pos: Option<Pos2>,
    pub current_pos: Option<Pos2>,
    pub is_selecting: bool,
    /// Minimum drag size in overlay points before a rect counts.
    pub min_size: f32,
}

impl AreaSelector {
    pub fn new() -> Self {
        Self {
            start_pos: None,
            current_pos: None,
            is_selecting: false,
            min_size: 8.0,
        }
    }

    pub fn begin_drag(&mut self, pos: Pos2) {
        self.start_pos = Some(pos);
        self.current_pos = Some(pos);
        self.is_selecting = true;
    }

    pub fn update_drag(&mut self, pos: Pos2) {
        if self.is_selecting {
            self.current_pos = Some(pos);
        }
    }

    /// Finish the drag. Returns the selected rect in overlay-local points, or
    /// `None` when the drag was too small (treated as "no selection").
    pub fn end_drag(&mut self) -> Option<Rect> {
        self.is_selecting = false;
        let rect = self.live_rect();
        if rect.map(|r| r.width() >= self.min_size && r.height() >= self.min_size)
            .unwrap_or(false)
        {
            rect
        } else {
            self.reset();
            None
        }
    }

    /// Rect of the in-progress drag in overlay-local points.
    pub fn live_rect(&self) -> Option<Rect> {
        let (Some(start), Some(end)) = (self.start_pos, self.current_pos) else {
            return None;
        };
        Some(Rect::from_min_max(
            Pos2::new(start.x.min(end.x), start.y.min(end.y)),
            Pos2::new(start.x.max(end.x), start.y.max(end.y)),
        ))
    }

    pub fn reset(&mut self) {
        self.start_pos = None;
        self.current_pos = None;
        self.is_selecting = false;
    }
}

/// Map an overlay-local selection rect to screenshot pixels.
///
/// `view` is the overlay rect the screenshot image exactly fills, `img_w/h`
/// is the screenshot size in physical pixels. Returns `(x, y, w, h)`.
pub fn map_overlay_rect_to_pixels(
    sel: Rect,
    view: Rect,
    img_w: u32,
    img_h: u32,
) -> Option<(i32, i32, u32, u32)> {
    if view.width() <= 0.0 || view.height() <= 0.0 {
        return None;
    }
    let fx0 = ((sel.min.x - view.min.x) / view.width()).clamp(0.0, 1.0);
    let fy0 = ((sel.min.y - view.min.y) / view.height()).clamp(0.0, 1.0);
    let fx1 = ((sel.max.x - view.min.x) / view.width()).clamp(0.0, 1.0);
    let fy1 = ((sel.max.y - view.min.y) / view.height()).clamp(0.0, 1.0);
    let x = (fx0 * img_w as f32).round() as i32;
    let y = (fy0 * img_h as f32).round() as i32;
    let w = ((fx1 - fx0) * img_w as f32).round().max(0.0) as u32;
    let h = ((fy1 - fy0) * img_h as f32).round().max(0.0) as u32;
    if w < 10 || h < 10 {
        return None;
    }
    // Clamp to the image bounds.
    let x = x.clamp(0, img_w as i32 - 1);
    let y = y.clamp(0, img_h as i32 - 1);
    let w = w.min(img_w.saturating_sub(x as u32));
    let h = h.min(img_h.saturating_sub(y as u32));
    if w < 10 || h < 10 {
        return None;
    }
    Some((x, y, w, h))
}

/// Rounded, semi-transparent chip behind a line of overlay text so it stays
/// legible over any desktop content underneath.
fn text_chip(
    painter: &egui::Painter,
    mut center: Pos2,
    galley: std::sync::Arc<egui::Galley>,
    view: Rect,
) {
    let size = galley.size() + Vec2::new(16.0, 10.0);
    let half = size / 2.0;
    if view.width() > size.x + 8.0 {
        center.x = center.x.clamp(view.min.x + half.x + 4.0, view.max.x - half.x - 4.0);
    }
    if view.height() > size.y + 8.0 {
        center.y = center.y.clamp(view.min.y + half.y + 4.0, view.max.y - half.y - 4.0);
    }
    let rect = Rect::from_center_size(center, size);
    painter.rect_filled(rect, half.y, Color32::from_rgba_unmultiplied(15, 23, 42, 205));
    painter.galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// Paint the frozen screenshot + dim layer + selection cutout + hints.
/// `view` must be the exact rect the background image fills.
pub fn paint_area_overlay(
    ui: &mut Ui,
    tex_id: egui::TextureId,
    view: Rect,
    selection: Option<Rect>,
) {
    let painter = ui.painter();
    let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));

    // Frozen desktop.
    painter.image(tex_id, view, full_uv, Color32::WHITE);
    // Dim everything…
    painter.rect_filled(view, 0.0, Color32::from_black_alpha(110));

    if let Some(sel) = selection {
        let sel = sel.intersect(view);
        if sel.is_positive() {
            // …then punch the selection back through at full brightness.
            let uv = Rect::from_min_max(
                Pos2::new(
                    ((sel.min.x - view.min.x) / view.width()).clamp(0.0, 1.0),
                    ((sel.min.y - view.min.y) / view.height()).clamp(0.0, 1.0),
                ),
                Pos2::new(
                    ((sel.max.x - view.min.x) / view.width()).clamp(0.0, 1.0),
                    ((sel.max.y - view.min.y) / view.height()).clamp(0.0, 1.0),
                ),
            );
            painter.image(tex_id, sel, uv, Color32::WHITE);
            painter.rect_stroke(
                sel,
                0.0,
                egui::Stroke::new(2.0_f32, theme::REC),
            );
            // Size chip above the selection.
            let galley = painter.layout_no_wrap(
                format!("{} × {}", sel.width().round(), sel.height().round()),
                egui::FontId::proportional(15.0),
                Color32::WHITE,
            );
            let h = galley.size().y + 10.0;
            let center = sel.center_top() + Vec2::new(0.0, -(h / 2.0 + 8.0));
            text_chip(painter, center, galley, view);
        }
    }

    let hint = painter.layout_no_wrap(
        "Drag to select the area to record   •   Release to confirm   •   Esc to cancel".into(),
        egui::FontId::proportional(16.0),
        Color32::WHITE,
    );
    text_chip(
        painter,
        view.center_top() + Vec2::new(0.0, 34.0),
        hint,
        view,
    );
}

fn nice_step(total: f64) -> f64 {
    // Pick a ruler step so there are ~6-10 ticks.
    let target = (total / 8.0).max(0.5);
    let steps = [0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0];
    for &s in &steps {
        if s >= target {
            return s;
        }
    }
    600.0
}

/// mm:ss.d timecode for the ruler (00:04.5).
pub fn format_tc(secs: f64) -> String {
    let s = secs.max(0.0);
    let m = (s / 60.0).floor() as u32;
    let rest = s - m as f64 * 60.0;
    format!("{:02}:{:04.1}", m, rest)
}

/// Editor tracks passed to [`draw_timeline`].
pub struct TimelineTracks<'a> {
    pub thumb_textures: &'a [egui::TextureHandle],
    pub thumb_times: &'a [f64],
    pub waveform: &'a [f32],
    pub waveform_rate: f64,
    pub playhead: Option<f64>,
    pub fade_in: f64,
    pub fade_out: f64,
    pub total: f64,
}

#[allow(clippy::too_many_arguments)]
pub fn draw_timeline(
    ui: &mut Ui,
    timeline: &crate::timeline::Timeline,
    timeline_state: &mut crate::timeline::TimelineState,
    available_width: f32,
    height: f32,
    live_time: Option<f64>,
    is_recording: bool,
    tracks: &TimelineTracks,
) -> egui::Response {
    let (response, painter) = ui.allocate_painter(Vec2::new(available_width, height), Sense::click_and_drag());

    let rect = response.rect;
    let live = live_time.unwrap_or(0.0).max(0.0);
    let full_total = timeline.total_duration.max(live).max(tracks.total).max(0.0);
    timeline_state.clamp_view(full_total.max(0.1));
    let view_len = timeline_state.view_len(full_total.max(1.0));
    let vs = timeline_state.view_start;
    let ve = vs + view_len;
    // Horizontal scrollbar strip: only when zooming makes the content wider
    // than the view (zoom = 1 always fits, so no bar).
    let sb_h = if full_total > view_len * 1.001 && full_total > 0.0 {
        18.0
    } else {
        0.0
    };

    // Timeline palette: same slate/blue family as the rest of the app, so
    // the editor no longer looks like a second theme bolted on.
    let bg = theme::SUBTLE_BG;
    let track_bg = theme::SURFACE;
    let grid = theme::BORDER;
    let text_c = theme::TEXT_MUTED;
    painter.rect_filled(rect, 2.0_f32, bg);
    painter.rect_stroke(rect, 2.0_f32, Stroke::new(1.0_f32, grid));

    let time_to_x = |t: f64| {
        rect.left() + (((t - vs) / view_len.max(1e-6)) as f32 * rect.width())
    };
    let x_to_time = |x: f32| {
        vs + (((x - rect.left()) / rect.width().max(1.0)) as f64 * view_len)
    };

    let ruler_h = 20.0;
    let gutter = 52.0;
    let video_h = ((height - ruler_h - 8.0) * 0.55).clamp(48.0, 96.0);
    let video_top = rect.top() + ruler_h + 2.0;
    let audio_top = video_top + video_h + 2.0;
    let audio_h = (rect.bottom() - audio_top - 2.0 - sb_h).max(30.0);
    let track_left = rect.left() + gutter;

    // ---- ruler ----
    painter.rect_filled(
        Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.top() + ruler_h)),
        0.0,
        theme::PANEL_SIDEBAR,
    );
    let step = nice_step(view_len);
    // Align first tick to a multiple of step.
    let mut t = (vs / step).floor() * step;
    if t < 0.0 {
        t = 0.0;
    }
    while t <= ve + 1e-9 {
        if t >= 0.0 {
            let x = time_to_x(t);
            if x >= rect.left() - 20.0 && x <= rect.right() + 1.0 {
                painter.line_segment(
                    [Pos2::new(x, rect.top() + 4.0), Pos2::new(x, rect.top() + ruler_h)],
                    Stroke::new(1.0_f32, grid),
                );
                painter.text(
                    Pos2::new((x + 2.0).min(rect.right() - 46.0).max(rect.left() + 2.0), rect.top() + 3.0),
                    egui::Align2::LEFT_TOP,
                    format_tc(t),
                    egui::FontId::proportional(10.0),
                    text_c,
                );
            }
        }
        t += step;
    }

    // Gutter labels.
    painter.text(
        Pos2::new(rect.left() + 4.0, video_top + 2.0),
        egui::Align2::LEFT_TOP,
        "Video",
        egui::FontId::proportional(11.0),
        text_c,
    );
    painter.text(
        Pos2::new(rect.left() + 4.0, audio_top + 2.0),
        egui::Align2::LEFT_TOP,
        "Audio",
        egui::FontId::proportional(11.0),
        text_c,
    );

    let video_rect = Rect::from_min_max(
        Pos2::new(track_left, video_top),
        Pos2::new(rect.right() - 2.0, video_top + video_h),
    );
    let audio_rect = Rect::from_min_max(
        Pos2::new(track_left, audio_top),
        Pos2::new(rect.right() - 2.0, audio_top + audio_h),
    );
    painter.rect_filled(video_rect, 2.0_f32, track_bg);
    painter.rect_filled(audio_rect, 2.0_f32, track_bg);
    painter.rect_stroke(video_rect, 2.0_f32, Stroke::new(1.0_f32, grid));
    painter.rect_stroke(audio_rect, 2.0_f32, Stroke::new(1.0_f32, grid));

    // Vertical grid lines across tracks.
    let mut gt = (vs / step).floor() * step;
    while gt <= ve + 1e-9 {
        if gt >= 0.0 {
            let x = time_to_x(gt).clamp(video_rect.left(), video_rect.right());
            painter.line_segment(
                [Pos2::new(x, video_top), Pos2::new(x, audio_rect.bottom())],
                Stroke::new(1.0_f32, theme::BORDER_SOFT),
            );
        }
        gt += step;
    }

    // ---- video filmstrip ----
    let mut drew_thumb = false;
    if !tracks.thumb_textures.is_empty() {
        for (i, tex) in tracks.thumb_textures.iter().enumerate() {
            let Some(&tt) = tracks.thumb_times.get(i) else {
                continue;
            };
            if tt < vs - 1.0 || tt > ve + 1.0 {
                continue;
            }
            // Slot extends to the next thumb (or +0.5s for the last).
            let next = tracks.thumb_times.get(i + 1).copied().unwrap_or(tt + 0.5);
            let x1 = time_to_x(tt).max(video_rect.left());
            let x2 = time_to_x(next).min(video_rect.right());
            if x2 - x1 < 6.0 {
                continue;
            }
            let r = Rect::from_min_max(
                Pos2::new(x1, video_rect.top() + 1.0),
                Pos2::new(x2, video_rect.bottom() - 1.0),
            );
            painter.image(tex.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            painter.rect_stroke(r, 0.0, Stroke::new(1.0_f32, theme::BORDER));
            drew_thumb = true;
        }
    }
    if !drew_thumb {
        // Fallback: clip blocks (or live REC block).
        if is_recording {
            let x1 = time_to_x(0.0).max(video_rect.left());
            let x2 = time_to_x(live).min(video_rect.right()).max(x1 + 8.0);
            let r = Rect::from_min_max(
                Pos2::new(x1, video_rect.top() + 1.0),
                Pos2::new(x2, video_rect.bottom() - 1.0),
            );
            painter.rect_filled(r, 2.0_f32, theme::REC);
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                format!("● REC {:.1}s", live),
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
        } else if !timeline.segments.is_empty() {
            for (idx, seg) in timeline.segments.iter().enumerate() {
                let x1 = time_to_x(seg.timeline_start).max(video_rect.left());
                let x2 = time_to_x(seg.timeline_end).min(video_rect.right());
                if x2 - x1 < 4.0 {
                    continue;
                }
                let r = Rect::from_min_max(
                    Pos2::new(x1, video_rect.top() + 1.0),
                    Pos2::new(x2, video_rect.bottom() - 1.0),
                );
                let fill = if idx % 2 == 0 {
                    Color32::from_rgb(120, 170, 130)
                } else {
                    Color32::from_rgb(110, 160, 190)
                };
                painter.rect_filled(r, 2.0_f32, fill);
                if r.width() > 70.0 {
                    painter.text(
                        r.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("clip {} • {:.1}s", idx + 1, seg.timeline_end - seg.timeline_start),
                        egui::FontId::proportional(11.0),
                        Color32::WHITE,
                    );
                }
            }
        } else {
            painter.text(
                video_rect.center(),
                egui::Align2::CENTER_CENTER,
                "No clips yet — press ● Record",
                egui::FontId::proportional(12.0),
                text_c,
            );
        }
    }

    // ---- audio waveform ----
    let mid = (audio_rect.top() + audio_rect.bottom()) / 2.0;
    let half = (audio_rect.height() / 2.0 - 3.0).max(4.0);
    painter.line_segment(
        [Pos2::new(audio_rect.left(), mid), Pos2::new(audio_rect.right(), mid)],
        Stroke::new(1.0_f32, theme::BORDER_SOFT),
    );
    if !tracks.waveform.is_empty() && tracks.waveform_rate > 0.0 {
        let rate = tracks.waveform_rate;
        let i0 = ((vs * rate).floor() as usize).saturating_sub(1);
        let i1 = ((ve * rate).ceil() as usize + 1).min(tracks.waveform.len());
        let buckets = (i1.saturating_sub(i0)).max(1);
        let px_per = audio_rect.width() / buckets as f32;
        for (k, &v) in tracks.waveform.iter().enumerate().take(i1).skip(i0) {
            let tc = (k as f64 + 0.5) / rate;
            let x = time_to_x(tc);
            if x < audio_rect.left() || x > audio_rect.right() {
                continue;
            }
            let amp = v.clamp(0.0, 1.0);
            if amp < 0.008 {
                continue;
            }
            let h = (amp * half).max(1.0);
            let col = if amp < 0.12 {
                theme::CARD_BLUE
            } else {
                theme::METER_BLUE
            };
            painter.line_segment(
                [Pos2::new(x, mid - h), Pos2::new(x, mid + h)],
                Stroke::new(px_per.clamp(1.0, 3.0), col),
            );
        }
    } else if !is_recording && timeline.segments.is_empty() {
        painter.text(
            audio_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Waveform appears here after recording",
            egui::FontId::proportional(11.0),
            text_c,
        );
    }
    // Fade overlays.
    if tracks.fade_in > 0.05 && full_total > 0.0 {
        let x = time_to_x(tracks.fade_in).min(audio_rect.right());
        painter.rect_filled(
            Rect::from_min_max(audio_rect.min, Pos2::new(x, audio_rect.bottom())),
            0.0,
            Color32::from_rgba_unmultiplied(255, 200, 60, 60),
        );
        painter.text(
            Pos2::new(audio_rect.left() + 3.0, audio_rect.top() + 2.0),
            egui::Align2::LEFT_TOP,
            "fade in",
            egui::FontId::proportional(10.0),
            text_c,
        );
    }
    if tracks.fade_out > 0.05 && full_total > 0.0 {
        let x = time_to_x((full_total - tracks.fade_out).max(0.0)).max(audio_rect.left());
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(x, audio_rect.top()), audio_rect.max),
            0.0,
            Color32::from_rgba_unmultiplied(255, 200, 60, 60),
        );
        painter.text(
            Pos2::new(audio_rect.right() - 3.0, audio_rect.top() + 2.0),
            egui::Align2::RIGHT_TOP,
            "fade out",
            egui::FontId::proportional(10.0),
            text_c,
        );
    }

    // ---- selection (timeline yellow) ----
    if let (Some(a), Some(b)) = (timeline_state.selection_start, timeline_state.selection_end) {
        let (s, e) = (a.min(b), a.max(b));
        if (e - s) > 1e-3 {
            let x1 = time_to_x(s).clamp(track_left, rect.right());
            let x2 = time_to_x(e).clamp(track_left, rect.right());
            for r in [video_rect, audio_rect] {
                let sel = Rect::from_min_max(
                    Pos2::new(x1, r.top()),
                    Pos2::new(x2, r.bottom()),
                );
                painter.rect_filled(sel, 0.0, Color32::from_rgba_unmultiplied(255, 200, 60, 90));
            }
            painter.rect_stroke(
                Rect::from_min_max(Pos2::new(x1, video_rect.top()), Pos2::new(x2, audio_rect.bottom())),
                0.0,
                Stroke::new(1.5_f32, Color32::from_rgb(230, 150, 20)),
            );
            painter.text(
                Pos2::new((x1 + x2) / 2.0, rect.top() + ruler_h - 2.0),
                egui::Align2::CENTER_BOTTOM,
                format!("{:.1}s", e - s),
                egui::FontId::proportional(10.0),
                Color32::from_rgb(140, 90, 10),
            );
        }
    }

    // ---- playhead (blue pin) ----
    let head_t = if is_recording {
        Some(live)
    } else {
        tracks.playhead
    };
    if let Some(ph) = head_t {
        if ph >= vs - 1.0 && ph <= ve + 1.0 && full_total > 0.0 {
            let x = time_to_x(ph).clamp(track_left, rect.right() - 1.0);
            // Pin on the ruler.
            let pin = [
                Pos2::new(x - 5.0, rect.top() + 2.0),
                Pos2::new(x + 5.0, rect.top() + 2.0),
                Pos2::new(x, rect.top() + 11.0),
            ];
            painter.add(egui::Shape::convex_polygon(
                pin.to_vec(),
                theme::METER_BLUE,
                Stroke::NONE,
            ));
            painter.line_segment(
                [Pos2::new(x, rect.top() + 11.0), Pos2::new(x, audio_rect.bottom())],
                Stroke::new(1.5_f32, theme::METER_BLUE),
            );
        }
    }

    // ---- horizontal scrollbar (zoomed view vs full content) ----
    let sb_rect = if sb_h > 0.0 {
        Some(Rect::from_min_max(
            Pos2::new(track_left, rect.bottom() - sb_h + 1.0),
            Pos2::new(rect.right() - 2.0, rect.bottom() - 2.0),
        ))
    } else {
        None
    };
    if let Some(sb) = sb_rect {
        let trough_w = sb.width().max(1.0);
        let thumb_w = (trough_w * (view_len / full_total.max(1e-6)) as f32)
            .clamp(24.0, trough_w);
        let thumb_x = sb.left() + ((vs / full_total.max(1e-6)) as f32) * trough_w;
        painter.rect_filled(sb, 4.0, theme::PLACEHOLDER_BG);
        let hovered = response.hover_pos().map(|p| sb.contains(p)).unwrap_or(false);
        let fill = if timeline_state.scrollbar_drag {
            Color32::from_rgb(71, 85, 105)
        } else if hovered {
            Color32::from_rgb(100, 116, 139)
        } else {
            theme::BORDER_STRONG
        };
        let thumb = Rect::from_min_size(
            Pos2::new(thumb_x, sb.top()),
            egui::vec2(thumb_w, sb.height()),
        );
        painter.rect_filled(thumb, 4.0, fill);
    }

    // ---- hover tooltip ----
    if let Some(hp) = response.hover_pos() {
        if hp.x >= track_left && hp.y >= rect.top() {
            let t = x_to_time(hp.x).clamp(0.0, full_total.max(0.0));
            timeline_state.hover_time = Some(t);
            let x = time_to_x(t);
            painter.line_segment(
                [Pos2::new(x, video_rect.top()), Pos2::new(x, audio_rect.bottom())],
                Stroke::new(1.0_f32, theme::TEXT_DIM),
            );
            painter.text(
                Pos2::new(
                    x.clamp(track_left + 28.0, rect.right() - 28.0),
                    rect.top() + ruler_h - 3.0,
                ),
                egui::Align2::CENTER_BOTTOM,
                format_tc(t),
                egui::FontId::proportional(10.0),
                theme::TEXT,
            );
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
    }

    // ---- interaction: click = playhead, drag = select / scrollbar ----
    let in_sb = |pos: Pos2| sb_rect.is_some_and(|r| r.contains(pos));
    if let (Some(_), Some(hp)) = (sb_rect, response.hover_pos()) {
        if in_sb(hp) {
            ui.ctx().set_cursor_icon(if timeline_state.scrollbar_drag {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Grab
            });
        }
    }
    if response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            if in_sb(pos) {
                // Grab the scrollbar: centre the view on the pointer and
                // remember where inside the thumb the grab happened.
                let sb = sb_rect.unwrap();
                let trough_w = sb.width().max(1.0);
                let frac = ((pos.x - sb.left()) / trough_w).clamp(0.0, 1.0);
                let target = frac as f64 * full_total - view_len / 2.0;
                timeline_state.view_start =
                    target.clamp(0.0, (full_total - view_len).max(0.0));
                let tx = sb.left()
                    + ((timeline_state.view_start / full_total.max(1e-6)) as f32)
                        * trough_w;
                timeline_state.scrollbar_grab_dx = pos.x - tx;
                timeline_state.scrollbar_drag = true;
            } else if pos.x >= track_left {
                timeline_state.start_selection(x_to_time(pos.x).max(0.0));
            }
        }
    }
    if response.drag_stopped() {
        if timeline_state.scrollbar_drag {
            timeline_state.scrollbar_drag = false;
        } else {
            timeline_state.end_selection();
            // Collapse accidental micro-drags into a playhead move.
            if let Some((s, e)) = timeline_state.get_selection() {
                if (e - s) < 0.03 {
                    timeline_state.clear_selection();
                    timeline_state.playhead = Some(s.clamp(0.0, full_total.max(0.0)));
                } else {
                    timeline_state.playhead = Some(e.clamp(0.0, full_total.max(0.0)));
                }
            }
        }
    } else if timeline_state.scrollbar_drag {
        // Dragging the thumb: scroll the view with the pointer.
        if let (Some(sb), Some(pos)) = (sb_rect, response.interact_pointer_pos()) {
            let trough_w = sb.width().max(1.0);
            let thumb_left = pos.x - timeline_state.scrollbar_grab_dx;
            let new_vs = ((thumb_left - sb.left()) / trough_w) as f64 * full_total;
            timeline_state.view_start =
                new_vs.clamp(0.0, (full_total - view_len).max(0.0));
        }
    } else if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            if !in_sb(pos) {
                timeline_state.update_selection(x_to_time(pos.x).max(0.0));
            }
        }
    }
    if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            if pos.x >= track_left && !in_sb(pos) {
                let t = x_to_time(pos.x).clamp(0.0, full_total.max(0.0));
                timeline_state.clear_selection();
                timeline_state.playhead = Some(t);
            }
        }
    }

    // ---- wheel: pan the zoomed view (ScrollArea direction: up = earlier) ----
    if sb_rect.is_some() && response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        let dy = scroll.y + scroll.x;
        if dy != 0.0 {
            let secs_per_px = view_len / rect.width().max(1.0) as f64;
            let target = timeline_state.view_start - dy as f64 * secs_per_px;
            timeline_state.view_start =
                target.clamp(0.0, (full_total - view_len).max(0.0));
        }
    }

    response
}

pub fn format_duration(duration: std::time::Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let millis = duration.subsec_millis();

    if hours > 0 {
        format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, seconds, millis)
    } else {
        format!("{:02}:{:02}.{:03}", minutes, seconds, millis)
    }
}
