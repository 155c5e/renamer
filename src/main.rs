#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod engine;
mod metadata;
mod template;

use app::RenamerApp;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([920.0, 640.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("Renamer"),
        ..Default::default()
    };
    eframe::run_native(
        "Renamer",
        native_options,
        Box::new(|_cc| Ok(Box::new(RenamerApp::default()))),
    )
}
