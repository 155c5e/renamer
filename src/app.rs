use std::path::PathBuf;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::engine::{self, Plan, RowStatus, Settings, UndoEntry};
use crate::metadata::FIELDS;

pub struct RenamerApp {
    source: Option<PathBuf>,
    output: Option<PathBuf>, // None => same as source
    template: String,
    recursive: bool,
    append_ext: bool,
    plan: Option<Plan>,
    message: String,
    /// Undo log for the most recent applied batch (in memory; also persisted
    /// to the output root). Drives the Undo button.
    last_undo: Vec<UndoEntry>,
}

impl Default for RenamerApp {
    fn default() -> Self {
        Self {
            source: None,
            output: None,
            template: "{date_taken:%Y}/{date_taken:%m}/{name}".to_string(),
            recursive: false,
            append_ext: true,
            plan: None,
            message: "Pick a source folder to begin.".to_string(),
            last_undo: Vec::new(),
        }
    }
}

impl RenamerApp {
    fn output_root(&self) -> Option<PathBuf> {
        let source = self.source.clone()?;
        Some(self.output.clone().unwrap_or(source))
    }

    /// Pull any persisted undo log from the current output root so undo works
    /// across sessions.
    fn load_undo_log(&mut self) {
        if let Some(root) = self.output_root() {
            let log = engine::read_undo_log(&root);
            if !log.is_empty() {
                self.message = format!("Loaded undo log: {} move(s) can be reverted.", log.len());
            }
            self.last_undo = log;
        }
    }

    fn settings(&self) -> Option<Settings> {
        let source = self.source.clone()?;
        let output_root = self.output.clone().unwrap_or_else(|| source.clone());
        Some(Settings {
            source,
            output_root,
            template: self.template.clone(),
            recursive: self.recursive,
            append_ext: self.append_ext,
        })
    }

    fn do_preview(&mut self) {
        match self.settings() {
            Some(s) => {
                let plan = engine::build_plan(&s);
                self.message = format!(
                    "{} file(s): {} ready, {} problem(s).",
                    plan.rows.len(),
                    plan.ok_count(),
                    plan.problem_count()
                );
                self.plan = Some(plan);
            }
            None => self.message = "Pick a source folder first.".to_string(),
        }
    }

    fn do_apply(&mut self) {
        let Some(plan) = self.plan.as_mut() else {
            self.message = "Run Preview before Apply.".to_string();
            return;
        };
        let log = engine::apply_plan(plan);
        let n = log.len();
        if let Some(root) = self.output_root() {
            if let Err(e) = engine::write_undo_log(&root, &log) {
                self.message = format!("Renamed {n} file(s), but undo log not saved: {e}");
            } else {
                self.message =
                    format!("Renamed {n} file(s). Undo available. Re-run Preview to refresh.");
            }
        }
        self.last_undo = log;
    }

    fn do_undo(&mut self) {
        if self.last_undo.is_empty() {
            self.message = "Nothing to undo.".to_string();
            return;
        }
        let (reverted, errors) = engine::undo(&self.last_undo);
        if let Some(root) = self.output_root() {
            engine::clear_undo_log(&root);
        }
        self.last_undo.clear();
        self.plan = None;
        self.message = if errors.is_empty() {
            format!("Undo complete: {reverted} file(s) restored.")
        } else {
            format!(
                "Undo: {reverted} restored, {} failed (e.g. {}).",
                errors.len(),
                errors.first().cloned().unwrap_or_default()
            )
        };
    }
}

impl eframe::App for RenamerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::SidePanel::right("fields")
            .resizable(false)
            .show(ctx, |ui| {
                ui.heading("Fields");
                ui.label("Use in template as {field}");
                ui.separator();
                for f in FIELDS {
                    ui.monospace(format!("{{{f}}}"));
                }
                ui.separator();
                ui.label("Extras:");
                ui.monospace("{n:03}");
                ui.small("sequence counter, zero-padded");
                ui.separator();
                ui.label("Date format example:");
                ui.monospace("{date_taken:%Y-%m-%d}");
                ui.small("strftime specifiers");
                ui.separator();
                ui.label("Slashes make subfolders,");
                ui.label("created automatically.");
            });

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Source folder…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.source = Some(p);
                        self.load_undo_log();
                    }
                }
                ui.label(
                    self.source
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".to_string()),
                );
            });
            ui.horizontal(|ui| {
                if ui.button("Output folder…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.output = Some(p);
                        self.load_undo_log();
                    }
                }
                if ui.button("× same as source").clicked() {
                    self.output = None;
                    self.load_undo_log();
                }
                ui.label(
                    self.output
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(same as source)".to_string()),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Template:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.template)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.recursive, "Recurse subfolders");
                ui.checkbox(&mut self.append_ext, "Keep extension");
                if ui.button("Preview").clicked() {
                    self.do_preview();
                }
                let can_apply = self
                    .plan
                    .as_ref()
                    .map(|p| p.ok_count() > 0)
                    .unwrap_or(false);
                if ui
                    .add_enabled(can_apply, egui::Button::new("Apply rename"))
                    .clicked()
                {
                    self.do_apply();
                }
                let can_undo = !self.last_undo.is_empty();
                if ui
                    .add_enabled(
                        can_undo,
                        egui::Button::new(format!("Undo ({})", self.last_undo.len())),
                    )
                    .clicked()
                {
                    self.do_undo();
                }
            });
            ui.add_space(2.0);
            ui.label(&self.message);
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(plan) = self.plan.as_ref() else {
                ui.centered_and_justified(|ui| {
                    ui.label("No preview yet. Set a template and click Preview.");
                });
                return;
            };

            TableBuilder::new(ui)
                .striped(true)
                .column(Column::auto().at_least(160.0))
                .column(Column::remainder())
                .column(Column::auto().at_least(120.0))
                .header(20.0, |mut header| {
                    header.col(|ui| {
                        ui.strong("Original");
                    });
                    header.col(|ui| {
                        ui.strong("New path");
                    });
                    header.col(|ui| {
                        ui.strong("Status");
                    });
                })
                .body(|mut body| {
                    for row in &plan.rows {
                        body.row(18.0, |mut r| {
                            r.col(|ui| {
                                let name = row
                                    .src
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("");
                                ui.monospace(name);
                            });
                            r.col(|ui| {
                                ui.monospace(&row.rel_target);
                            });
                            r.col(|ui| {
                                let (txt, color) = status_label(&row.status);
                                ui.colored_label(color, txt);
                            });
                        });
                    }
                });
        });
    }
}

fn status_label(s: &RowStatus) -> (String, egui::Color32) {
    match s {
        RowStatus::Ok => ("ready".into(), egui::Color32::from_rgb(120, 200, 120)),
        RowStatus::Done => ("done".into(), egui::Color32::from_rgb(120, 200, 255)),
        RowStatus::Collision => ("collision".into(), egui::Color32::from_rgb(240, 180, 60)),
        RowStatus::Exists => ("target exists".into(), egui::Color32::from_rgb(240, 180, 60)),
        RowStatus::Error(e) => (format!("error: {e}"), egui::Color32::from_rgb(240, 100, 100)),
        RowStatus::Failed(e) => (format!("failed: {e}"), egui::Color32::from_rgb(240, 100, 100)),
    }
}
