//! Design tokens + small widget helpers shared by `app` and `ui`.
//!
//! One source of truth for colors, metrics and button styles so the chrome,
//! the editor dock and the timeline stop drifting apart (three different
//! reds, two greens, five panel tints…).

use egui::{Color32, RichText, Stroke};

// ---------- accent / brand ----------
pub const ACCENT: Color32 = Color32::from_rgb(37, 99, 235); // blue-600
pub const ACCENT_DARK: Color32 = Color32::from_rgb(29, 78, 216); // blue-700
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(219, 234, 254); // blue-100
pub const ACCENT_BORDER: Color32 = Color32::from_rgb(191, 219, 254); // blue-200
pub const BRAND: Color32 = Color32::from_rgb(219, 39, 119); // pink-600

// ---------- status ----------
pub const REC: Color32 = Color32::from_rgb(211, 47, 47); // record / stop fill
pub const REC_SOFT: Color32 = Color32::from_rgb(255, 82, 82); // "● REC" text
pub const REC_TEXT: Color32 = Color32::from_rgb(220, 38, 38); // red-600, on white
pub const OK: Color32 = Color32::from_rgb(56, 142, 60); // resume / continue fill
pub const PAUSED: Color32 = Color32::from_rgb(217, 119, 6); // amber-600
pub const WARN_TEXT: Color32 = Color32::from_rgb(180, 83, 9); // amber-700 (readable)
pub const ERROR_TEXT: Color32 = Color32::from_rgb(185, 28, 28); // red-700 (readable)
pub const METER_BLUE: Color32 = Color32::from_rgb(66, 133, 244);
pub const METER_GREEN: Color32 = Color32::from_rgb(52, 187, 120);

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
pub const CARD_GREEN: Color32 = Color32::from_rgb(110, 231, 183);
pub const CARD_AMBER: Color32 = Color32::from_rgb(253, 224, 71);
pub const CARD_VIOLET: Color32 = Color32::from_rgb(196, 181, 253);
/// Readable counterparts of the card border pastels, used for card titles
/// (the pastels themselves fail contrast as text).
pub const CARD_TEXT_GREEN: Color32 = Color32::from_rgb(5, 150, 105); // emerald-600
pub const CARD_TEXT_VIOLET: Color32 = Color32::from_rgb(124, 58, 237); // violet-600

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
pub const RADIUS: f32 = 8.0;
pub const CARD_GAP: f32 = 6.0;
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
/// White card with a colored hairline border. The closure draws the whole
/// card (title row included) so only one `&mut self` capture is live at a
/// time — egui's sibling closures would otherwise fight over the borrow.
pub fn card(ui: &mut egui::Ui, border: Color32, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, border))
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::same(6.0))
        .show(ui, |ui| {
            // Tighter rows inside cards so the sidebar fits without scrolling
            // on short windows.
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 3.0);
            body(ui);
        });
}

/// [`card`] with a strong title line already drawn.
pub fn titled_card(
    ui: &mut egui::Ui,
    border: Color32,
    title: RichText,
    body: impl FnOnce(&mut egui::Ui),
) {
    card(ui, border, move |ui| {
        ui.label(title.small().strong());
        ui.add_space(1.0);
        body(ui);
    });
}

/// Two-column key/value row with a muted, consistently-aligned key.
pub fn kv(ui: &mut egui::Ui, key: &str, value: impl Into<egui::WidgetText>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 2.0);
        ui.add_sized(
            egui::vec2(82.0, 16.0),
            egui::Label::new(RichText::new(key).color(TEXT_DIM).small()),
        );
        ui.label(value.into().small());
    });
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
