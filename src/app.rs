use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::engine::{self, Plan, Progress, RowStatus, Settings, UndoEntry};
use crate::metadata::FIELDS;

/// Result handed back from a worker thread to the GUI thread.
enum BgResult {
    Preview(Plan),
    Apply {
        plan: Plan,
        undo: Vec<UndoEntry>,
        log_err: Option<String>,
    },
    Undo {
        reverted: usize,
        errors: Vec<String>,
    },
}

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

    // --- background work ---
    /// Receiver for the in-flight worker thread's result, if any.
    rx: Option<Receiver<BgResult>>,
    /// Shared progress counters for the in-flight worker.
    progress: Option<Arc<Progress>>,
    /// Label for what the worker is doing ("Scanning", "Renaming", …).
    busy_label: String,
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
            rx: None,
            progress: None,
            busy_label: String::new(),
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

    /// Set up a fresh channel + progress for a worker and return the sender
    /// and a repaint-capable context clone.
    fn start_worker(
        &mut self,
        ctx: &egui::Context,
        label: &str,
    ) -> (std::sync::mpsc::Sender<BgResult>, Arc<Progress>, egui::Context) {
        let (tx, rx) = channel();
        let progress = Arc::new(Progress::default());
        self.rx = Some(rx);
        self.progress = Some(progress.clone());
        self.busy_label = label.to_string();
        (tx, progress, ctx.clone())
    }

    fn do_preview(&mut self, ctx: &egui::Context) {
        let Some(settings) = self.settings() else {
            self.message = "Pick a source folder first.".to_string();
            return;
        };
        let (tx, progress, ctx) = self.start_worker(ctx, "Scanning");
        std::thread::spawn(move || {
            let plan = engine::build_plan_progress(&settings, &progress);
            let _ = tx.send(BgResult::Preview(plan));
            ctx.request_repaint();
        });
    }

    fn do_apply(&mut self, ctx: &egui::Context) {
        let Some(mut plan) = self.plan.clone() else {
            self.message = "Run Preview before Apply.".to_string();
            return;
        };
        let root = self.output_root();
        let (tx, progress, ctx) = self.start_worker(ctx, "Renaming");
        std::thread::spawn(move || {
            let undo = engine::apply_plan_progress(&mut plan, &progress);
            let log_err = root.and_then(|r| engine::write_undo_log(&r, &undo).err().map(|e| e.to_string()));
            let _ = tx.send(BgResult::Apply { plan, undo, log_err });
            ctx.request_repaint();
        });
    }

    fn do_undo(&mut self, ctx: &egui::Context) {
        if self.last_undo.is_empty() {
            self.message = "Nothing to undo.".to_string();
            return;
        }
        let entries = self.last_undo.clone();
        let root = self.output_root();
        let (tx, _progress, ctx) = self.start_worker(ctx, "Undoing");
        std::thread::spawn(move || {
            let (reverted, errors) = engine::undo(&entries);
            if let Some(r) = root {
                engine::clear_undo_log(&r);
            }
            let _ = tx.send(BgResult::Undo { reverted, errors });
            ctx.request_repaint();
        });
    }

    /// Drain a finished worker's result and update state. Returns true if the
    /// worker is still running.
    fn poll_worker(&mut self) -> bool {
        let Some(rx) = self.rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.rx = None;
                self.progress = None;
                self.busy_label.clear();
                self.handle_result(result);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Worker died without sending; clear busy state.
                self.rx = None;
                self.progress = None;
                self.busy_label.clear();
                self.message = "Background task ended unexpectedly.".to_string();
                false
            }
        }
    }

    fn handle_result(&mut self, result: BgResult) {
        match result {
            BgResult::Preview(plan) => {
                self.message = format!(
                    "{} file(s): {} ready, {} problem(s).",
                    plan.rows.len(),
                    plan.ok_count(),
                    plan.problem_count()
                );
                self.plan = Some(plan);
            }
            BgResult::Apply {
                plan,
                undo,
                log_err,
            } => {
                let n = undo.len();
                self.message = match log_err {
                    Some(e) => format!("Renamed {n} file(s), but undo log not saved: {e}"),
                    None => format!("Renamed {n} file(s). Undo available. Re-run Preview to refresh."),
                };
                self.plan = Some(plan);
                self.last_undo = undo;
            }
            BgResult::Undo { reverted, errors } => {
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
    }
}

impl eframe::App for RenamerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Pull in any finished background work, and keep animating while busy.
        let busy = self.poll_worker();
        if busy {
            ctx.request_repaint();
        }

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
                ui.separator();
                ui.small(
                    "EXIF fields (date_taken, camera_*, iso, \
                     width, height) read file contents; on cloud \
                     drives that triggers downloads. Filesystem \
                     fields don't.",
                );
            });

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
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
                        self.do_preview(ctx);
                    }
                    let can_apply = self.plan.as_ref().map(|p| p.ok_count() > 0).unwrap_or(false);
                    if ui
                        .add_enabled(can_apply, egui::Button::new("Apply rename"))
                        .clicked()
                    {
                        self.do_apply(ctx);
                    }
                    let can_undo = !self.last_undo.is_empty();
                    if ui
                        .add_enabled(
                            can_undo,
                            egui::Button::new(format!("Undo ({})", self.last_undo.len())),
                        )
                        .clicked()
                    {
                        self.do_undo(ctx);
                    }
                });
            });

            ui.add_space(2.0);
            if busy {
                let (done, total) = self
                    .progress
                    .as_ref()
                    .map(|p| p.snapshot())
                    .unwrap_or((0, 0));
                let frac = if total > 0 {
                    done as f32 / total as f32
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(format!("{} {done}/{total}", self.busy_label))
                        .animate(true),
                );
            } else {
                ui.label(&self.message);
            }
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
