//! Design tokens + small widget helpers shared by `app` and `ui`.
//!
//! One source of truth for colors, metrics and button styles so the chrome,
//! the editor dock and the timeline stop drifting apart (three different
//! reds, two greens, five panel tints…).

use egui::{Color32, RichText, Stroke};

// ---------- accent / brand ----------
pub const ACCENT: Color32 = Color32::from_rgb(79, 70, 229); // indigo-600
pub const ACCENT_DARK: Color32 = Color32::from_rgb(67, 56, 202); // indigo-700
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(224, 231, 255); // indigo-100
pub const ACCENT_BORDER: Color32 = Color32::from_rgb(199, 210, 254); // indigo-200
pub const BRAND: Color32 = Color32::from_rgb(219, 39, 119); // pink-600

// ---------- status ----------
pub const REC: Color32 = Color32::from_rgb(211, 47, 47); // record / stop fill
pub const REC_SOFT: Color32 = Color32::from_rgb(255, 82, 82); // "● REC" text
pub const REC_TEXT: Color32 = Color32::from_rgb(220, 38, 38); // red-600, on white
pub const OK: Color32 = Color32::from_rgb(56, 142, 60); // resume / continue fill
pub const PAUSED: Color32 = Color32::from_rgb(217, 119, 6); // amber-600
pub const WARN_TEXT: Color32 = Color32::from_rgb(180, 83, 9); // amber-700 (readable)
pub const ERROR_TEXT: Color32 = Color32::from_rgb(185, 28, 28); // red-700 (readable)
pub const METER_BLUE: Color32 = Color32::from_rgb(59, 130, 246);
pub const METER_GREEN: Color32 = Color32::from_rgb(16, 185, 129);
/// Soft channel tints for the audio mixer strips.
pub const TINT_BLUE_FILL: Color32 = Color32::from_rgb(239, 246, 255); // blue-50
pub const TINT_BLUE_BORDER: Color32 = Color32::from_rgb(191, 219, 254); // blue-200
pub const TINT_GREEN_FILL: Color32 = Color32::from_rgb(236, 253, 245); // emerald-50
pub const TINT_GREEN_BORDER: Color32 = Color32::from_rgb(167, 243, 208); // emerald-200
/// Toggle-switch track when off.
pub const TOGGLE_OFF: Color32 = Color32::from_rgb(203, 213, 225); // slate-300

// ---------- surfaces ----------
pub const SURFACE: Color32 = Color32::WHITE;
pub const PANEL_TOOLBAR: Color32 = Color32::from_rgb(239, 246, 255); // blue-50
pub const PANEL_SIDEBAR: Color32 = Color32::from_rgb(248, 250, 252); // slate-50
pub const PANEL_STATUS: Color32 = Color32::from_rgb(239, 246, 255); // blue-50
pub const PANEL_EDITOR: Color32 = Color32::from_rgb(255, 252, 240); // cream
pub const PANEL_CENTER: Color32 = Color32::from_rgb(246, 248, 252);
pub const PLACEHOLDER_BG: Color32 = Color32::from_rgb(226, 232, 240);
pub const METER_TRACK: Color32 = Color32::from_rgb(226, 232, 240);
/// Input backgrounds / timeline canvas — one tint for every "empty area".
pub const SUBTLE_BG: Color32 = Color32::from_rgb(241, 245, 249); // slate-100

// ---------- borders ----------
pub const BORDER: Color32 = Color32::from_rgb(203, 213, 225); // slate-300
pub const BORDER_SOFT: Color32 = Color32::from_rgb(226, 232, 240); // slate-200
pub const BORDER_STRONG: Color32 = Color32::from_rgb(148, 163, 184); // slate-400
pub const CARD_BLUE: Color32 = Color32::from_rgb(147, 197, 253);
pub const CARD_AMBER: Color32 = Color32::from_rgb(253, 224, 71);
/// Soft status-pill fills (state badges in the setup sidebar). Pair with the
/// existing `SUBTLE_BG` (slate) and `ACCENT_SOFT` (blue) for the other states.
pub const PILL_RED: Color32 = Color32::from_rgb(254, 242, 242); // red-50
pub const PILL_AMBER: Color32 = Color32::from_rgb(254, 243, 199); // amber-100

// ---------- text ----------
pub const TEXT: Color32 = Color32::from_rgb(15, 23, 42); // slate-900
pub const TEXT_MUTED: Color32 = Color32::from_rgb(71, 85, 105); // slate-600
pub const TEXT_DIM: Color32 = Color32::from_rgb(100, 116, 139); // slate-500

// ---------- metrics ----------
/// Height of every sized action button (Record, Stop, Resume…).
pub const BTN_H: f32 = 32.0;
/// Minimum hit target for icon-ish buttons (`⟳`, `⏮`, `−`…).
pub const ICON_BTN_H: f32 = 26.0;
pub const ICON_BTN_W: f32 = 30.0;
pub const RADIUS: f32 = 10.0;
pub const CARD_RADIUS: f32 = 14.0;
pub const CARD_GAP: f32 = 10.0;
/// Shared outer margin for the top/bottom/center panels.
pub fn panel_margin() -> egui::Margin {
    egui::Margin::symmetric(8.0, 6.0)
}

// ---------- text helpers ----------
pub fn heading(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(ACCENT_DARK)
}

// ---------- buttons ----------
/// Solid button with bold white text; hover/active feedback comes from the
/// theme's stroke colors.
pub fn filled(label: impl Into<String>, fill: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label.into()).color(Color32::WHITE).strong()).fill(fill)
}

pub fn record_btn(label: impl Into<String>) -> egui::Button<'static> {
    filled(label, REC)
}

pub fn resume_btn(label: impl Into<String>) -> egui::Button<'static> {
    filled(label, OK)
}

pub fn primary_btn(label: impl Into<String>) -> egui::Button<'static> {
    filled(label, ACCENT)
}

/// Icon/glyph button with a comfortable hit target (default `small_button`
/// is ~20px tall — below the usual 24–26px guideline).
pub fn icon_btn(label: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label.into()))
        .min_size(egui::vec2(ICON_BTN_W, ICON_BTN_H))
}

// ---------- cards ----------
/// Neutral section card: white fill, soft slate hairline, roomier padding.
/// Every sidebar section shares this one chrome so the panel reads as a
/// single column — color is reserved for *state* (pills, meters, warnings)
/// instead of being spent on decoration.
///
/// The closure draws the whole card (header included) so only one `&mut self`
/// capture is live at a time — egui's sibling closures would otherwise fight
/// over the borrow.
pub fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, BORDER_SOFT))
        .rounding(egui::Rounding::same(CARD_RADIUS))
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            // Fill the parent width: short content (e.g. "D:\", two buttons)
            // would otherwise leave the card hugging its content while sibling
            // cards stretch full width.
            ui.set_width(ui.available_width());
            // Slightly airier rows than the old 4/3 — the card padding grew too.
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            body(ui);
        });
}

/// Section header inside a card: title on the left, optional trailing content
/// (state pill, rescan button, clip count) pinned to the right edge.
pub fn section_header(
    ui: &mut egui::Ui,
    title: impl Into<String>,
    trailing: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title.into())
                .small()
                .strong()
                .color(TEXT),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing);
    });
    ui.add_space(2.0);
}

/// Rounded status pill for state badges (Idle / ● REC / Paused …).
pub fn pill(ui: &mut egui::Ui, text: &str, fill: Color32, text_color: Color32) {
    egui::Frame::none()
        .fill(fill)
        .rounding(egui::Rounding::same(9.0))
        .inner_margin(egui::Margin::symmetric(8.0, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(text_color).small().strong());
        });
}

/// Modern toggle switch (a friendlier checkbox). The knob slides with a
/// short animation via `animate_bool`, so it feels alive without any manual
/// repaint bookkeeping — egui repaints while the animation runs.
pub fn toggle(ui: &mut egui::Ui, on: &mut bool, id_salt: &str, accent: Color32) -> egui::Response {
    let w = 36.0_f32;
    let h = 20.0_f32;
    let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    // 0 = off … 1 = on, animated.
    let t = ui.ctx().animate_bool(egui::Id::new(id_salt), *on);
    let enabled = ui.is_enabled();
    let track = if !enabled {
        METER_TRACK
    } else if t > 0.5 {
        if t >= 0.999 { accent } else { accent.lerp_to_gamma(TOGGLE_OFF, 1.0 - t) }
    } else {
        TOGGLE_OFF
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, h / 2.0, track);
    let r = h / 2.0 - 2.5;
    let cx = rect.left() + h / 2.0 + t * (rect.width() - h);
    painter.circle_filled(egui::pos2(cx, rect.center().y), r + 1.0, Color32::from_black_alpha(24));
    painter.circle_filled(egui::pos2(cx, rect.center().y), r, Color32::WHITE);
    resp.on_hover_text(if *on { "On — click to turn off" } else { "Off — click to turn on" })
}

/// Muted hint line (instructions, secondary info) at readable contrast —
/// deliberately stronger than `ui.weak()`, which fails on tinted panels.
pub fn hint(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).color(TEXT_MUTED).small());
}

/// Ellipsize at `max` chars so long monitor/window titles can't blow up a
/// combo box or the toolbar row.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
