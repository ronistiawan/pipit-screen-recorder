mod app;
mod audio;
mod audioplay;
mod capture;
mod encoder;
mod icon;
mod player;
mod timeline;
mod theme;
mod ui;

use app::ScreenRecorderApp;
use eframe::Error;

fn main() -> Result<(), Error> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1000.0, 700.0])
        .with_min_inner_size([800.0, 600.0])
        .with_title("Pipit Screen Recorder");
    if let Some(app_icon) = icon::app_icon() {
        viewport = viewport.with_icon(app_icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Pipit Screen Recorder",
        options,
        Box::new(|cc| Ok(Box::new(ScreenRecorderApp::new(cc)))),
    )
}
