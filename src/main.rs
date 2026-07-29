#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod catalog;
mod colour;
mod dupes;
mod engine;
mod hashing;
mod indexer;
mod kind;
mod metadata;
mod paths;
mod pcloud;
mod quarantine;
mod template;
mod thumbs;

use app::RenamerApp;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 720.0])
            .with_min_inner_size([700.0, 460.0])
            .with_title("Renamer — photo library organizer"),
        ..Default::default()
    };
    eframe::run_native(
        "Renamer",
        native_options,
        Box::new(|_cc| Ok(Box::new(RenamerApp::default()))),
    )
}
