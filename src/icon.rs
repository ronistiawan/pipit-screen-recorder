//! Window icon for Pipit Screen Recorder.
//!
//! The PNG in `assets/` is embedded at compile time and decoded once
//! (cached) for use with [`egui::ViewportBuilder::with_icon`].

use std::sync::OnceLock;

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

fn load() -> Option<egui::IconData> {
    let img = image::load_from_memory(ICON_PNG).ok()?.to_rgba8();
    let (width, height) = (img.width(), img.height());
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

/// The app icon, decoded once and cloned on later calls.
/// Returns `None` only if the embedded PNG fails to decode.
pub fn app_icon() -> Option<egui::IconData> {
    static CACHE: OnceLock<Option<egui::IconData>> = OnceLock::new();
    CACHE.get_or_init(load).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icon_decodes_to_expected_size() {
        let icon = app_icon().expect("embedded icon.png must decode");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        // Not a blank image: alpha channel must be (mostly) opaque.
        let opaque = icon.rgba.chunks_exact(4).filter(|p| p[3] > 128).count();
        assert!(opaque > icon.rgba.len() / 4 / 2, "icon looks blank");
    }
}
