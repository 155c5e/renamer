use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::engine::{self, Plan, Progress, RowStatus, Settings, UndoEntry};
use crate::metadata::FIELDS;
use crate::pcloud::{self, Client, Region, RemotePlan, RemoteUndo};

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode {
    Local,
    PCloud,
}

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
    Login(Result<Client, String>),
    RemotePreview(Result<RemotePlan, String>),
    RemoteApply {
        plan: RemotePlan,
        undo: Vec<RemoteUndo>,
    },
    RemoteUndo {
        reverted: usize,
        errors: Vec<String>,
    },
}

/// Lightweight row for the results table, built from whichever plan is active.
struct RowView {
    name: String,
    target: String,
    status: RowStatus,
}

pub struct RenamerApp {
    mode: Mode,

    // --- local mode ---
    source: Option<PathBuf>,
    output: Option<PathBuf>, // None => same as source
    plan: Option<Plan>,
    last_undo: Vec<UndoEntry>,

    // --- pcloud mode ---
    pc_region: Region,
    pc_email: String,
    pc_password: String,
    pc_path: String,
    pc_client: Option<Client>,
    remote_plan: Option<RemotePlan>,
    remote_undo: Vec<RemoteUndo>,

    // --- shared ---
    template: String,
    recursive: bool,
    append_ext: bool,
    message: String,

    // --- background work ---
    rx: Option<Receiver<BgResult>>,
    progress: Option<Arc<Progress>>,
    busy_label: String,
}

impl Default for RenamerApp {
    fn default() -> Self {
        Self {
            mode: Mode::Local,
            source: None,
            output: None,
            plan: None,
            last_undo: Vec::new(),
            pc_region: Region::Us,
            pc_email: String::new(),
            pc_password: String::new(),
            pc_path: "/".to_string(),
            pc_client: None,
            remote_plan: None,
            remote_undo: Vec::new(),
            template: "{date_modified:%Y}/{date_modified:%m}/{name}".to_string(),
            recursive: false,
            append_ext: true,
            message: "Pick a source folder, or switch to pCloud.".to_string(),
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

    fn start_worker(
        &mut self,
        ctx: &egui::Context,
        label: &str,
    ) -> (Sender<BgResult>, Arc<Progress>, egui::Context) {
        let (tx, rx) = channel();
        let progress = Arc::new(Progress::default());
        self.rx = Some(rx);
        self.progress = Some(progress.clone());
        self.busy_label = label.to_string();
        (tx, progress, ctx.clone())
    }

    // ---- local actions ----

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
            let log_err =
                root.and_then(|r| engine::write_undo_log(&r, &undo).err().map(|e| e.to_string()));
            let _ = tx.send(BgResult::Apply {
                plan,
                undo,
                log_err,
            });
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
        let (tx, _p, ctx) = self.start_worker(ctx, "Undoing");
        std::thread::spawn(move || {
            let (reverted, errors) = engine::undo(&entries);
            if let Some(r) = root {
                engine::clear_undo_log(&r);
            }
            let _ = tx.send(BgResult::Undo { reverted, errors });
            ctx.request_repaint();
        });
    }

    // ---- pcloud actions ----

    fn do_login(&mut self, ctx: &egui::Context) {
        let (region, email, pass) = (self.pc_region, self.pc_email.clone(), self.pc_password.clone());
        let (tx, _p, ctx) = self.start_worker(ctx, "Logging in");
        std::thread::spawn(move || {
            let res = Client::login(region, &email, &pass);
            let _ = tx.send(BgResult::Login(res));
            ctx.request_repaint();
        });
    }

    fn do_remote_preview(&mut self, ctx: &egui::Context) {
        let Some(client) = self.pc_client.clone() else {
            self.message = "Log in to pCloud first.".to_string();
            return;
        };
        let (path, recursive, template, append_ext) = (
            self.pc_path.clone(),
            self.recursive,
            self.template.clone(),
            self.append_ext,
        );
        let (tx, _p, ctx) = self.start_worker(ctx, "Listing pCloud");
        std::thread::spawn(move || {
            let res = client
                .listfolder(&path, recursive)
                .map(|files| pcloud::build_remote_plan(&files, &path, &template, append_ext));
            let _ = tx.send(BgResult::RemotePreview(res));
            ctx.request_repaint();
        });
    }

    fn do_remote_apply(&mut self, ctx: &egui::Context) {
        let (Some(client), Some(mut plan)) = (self.pc_client.clone(), self.remote_plan.clone())
        else {
            self.message = "Preview before Apply.".to_string();
            return;
        };
        let (tx, progress, ctx) = self.start_worker(ctx, "Renaming on pCloud");
        std::thread::spawn(move || {
            let undo = pcloud::apply_remote_plan(&client, &mut plan, &progress);
            let _ = tx.send(BgResult::RemoteApply { plan, undo });
            ctx.request_repaint();
        });
    }

    fn do_remote_undo(&mut self, ctx: &egui::Context) {
        let (Some(client), false) = (self.pc_client.clone(), self.remote_undo.is_empty()) else {
            self.message = "Nothing to undo.".to_string();
            return;
        };
        let entries = self.remote_undo.clone();
        let (tx, _p, ctx) = self.start_worker(ctx, "Undoing on pCloud");
        std::thread::spawn(move || {
            let (reverted, errors) = pcloud::undo_remote(&client, &entries);
            let _ = tx.send(BgResult::RemoteUndo { reverted, errors });
            ctx.request_repaint();
        });
    }

    // ---- worker plumbing ----

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
                self.message = plan_summary(plan.rows.len(), plan.ok_count(), plan.problem_count());
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
                    None => format!("Renamed {n} file(s). Undo available."),
                };
                self.plan = Some(plan);
                self.last_undo = undo;
            }
            BgResult::Undo { reverted, errors } => {
                self.last_undo.clear();
                self.plan = None;
                self.message = undo_summary(reverted, &errors);
            }
            BgResult::Login(Ok(client)) => {
                self.pc_client = Some(client);
                self.pc_password.clear();
                self.message = "Connected to pCloud.".to_string();
            }
            BgResult::Login(Err(e)) => {
                self.message = format!("Login failed: {e}");
            }
            BgResult::RemotePreview(Ok(plan)) => {
                self.message =
                    plan_summary(plan.rows.len(), plan.ok_count(), plan.problem_count());
                self.remote_plan = Some(plan);
            }
            BgResult::RemotePreview(Err(e)) => {
                self.message = format!("pCloud list failed: {e}");
            }
            BgResult::RemoteApply { plan, undo } => {
                self.message = format!("Renamed {} file(s) on pCloud. Undo available.", undo.len());
                self.remote_plan = Some(plan);
                self.remote_undo = undo;
            }
            BgResult::RemoteUndo { reverted, errors } => {
                self.remote_undo.clear();
                self.remote_plan = None;
                self.message = undo_summary(reverted, &errors);
            }
        }
    }

    /// Rows to display in the table for the active mode.
    fn display_rows(&self) -> Vec<RowView> {
        let map_local = |p: &Plan| {
            p.rows
                .iter()
                .map(|r| RowView {
                    name: r
                        .src
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string(),
                    target: r.rel_target.clone(),
                    status: r.status.clone(),
                })
                .collect()
        };
        match self.mode {
            Mode::Local => self.plan.as_ref().map(map_local).unwrap_or_default(),
            Mode::PCloud => self
                .remote_plan
                .as_ref()
                .map(|p| {
                    p.rows
                        .iter()
                        .map(|r| RowView {
                            name: r.display_name.clone(),
                            target: r.rel_target.clone(),
                            status: r.status.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn can_apply(&self) -> bool {
        match self.mode {
            Mode::Local => self.plan.as_ref().map(|p| p.ok_count() > 0).unwrap_or(false),
            Mode::PCloud => self
                .remote_plan
                .as_ref()
                .map(|p| p.ok_count() > 0)
                .unwrap_or(false),
        }
    }

    fn undo_count(&self) -> usize {
        match self.mode {
            Mode::Local => self.last_undo.len(),
            Mode::PCloud => self.remote_undo.len(),
        }
    }
}

fn plan_summary(total: usize, ok: usize, problems: usize) -> String {
    format!("{total} file(s): {ok} ready, {problems} problem(s).")
}

fn undo_summary(reverted: usize, errors: &[String]) -> String {
    if errors.is_empty() {
        format!("Undo complete: {reverted} file(s) restored.")
    } else {
        format!(
            "Undo: {reverted} restored, {} failed (e.g. {}).",
            errors.len(),
            errors.first().cloned().unwrap_or_default()
        )
    }
}

impl eframe::App for RenamerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
                ui.monospace("{n:03}");
                ui.small("sequence counter");
                ui.monospace("{date_modified:%Y-%m}");
                ui.small("strftime date format");
                ui.separator();
                ui.label("Slashes make subfolders.");
                ui.separator();
                if self.mode == Mode::PCloud {
                    ui.small(
                        "pCloud mode: EXIF fields (date_taken, \
                         camera_*, iso) are unavailable via the API. \
                         Use date_modified / date_created.",
                    );
                } else {
                    ui.small(
                        "EXIF fields read file contents; on cloud \
                         drives that triggers downloads. Filesystem \
                         fields don't.",
                    );
                }
            });

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Mode:");
                    ui.selectable_value(&mut self.mode, Mode::Local, "Local files");
                    ui.selectable_value(&mut self.mode, Mode::PCloud, "pCloud");
                });
                ui.separator();
                match self.mode {
                    Mode::Local => self.local_controls(ui),
                    Mode::PCloud => self.pcloud_controls(ui),
                }
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
                        match self.mode {
                            Mode::Local => self.do_preview(ctx),
                            Mode::PCloud => self.do_remote_preview(ctx),
                        }
                    }
                    if ui
                        .add_enabled(self.can_apply(), egui::Button::new("Apply rename"))
                        .clicked()
                    {
                        match self.mode {
                            Mode::Local => self.do_apply(ctx),
                            Mode::PCloud => self.do_remote_apply(ctx),
                        }
                    }
                    let undo_n = self.undo_count();
                    if ui
                        .add_enabled(undo_n > 0, egui::Button::new(format!("Undo ({undo_n})")))
                        .clicked()
                    {
                        match self.mode {
                            Mode::Local => self.do_undo(ctx),
                            Mode::PCloud => self.do_remote_undo(ctx),
                        }
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
            let rows = self.display_rows();
            if rows.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No preview yet. Set a template and click Preview.");
                });
                return;
            }

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
                    for row in &rows {
                        body.row(18.0, |mut r| {
                            r.col(|ui| {
                                ui.monospace(&row.name);
                            });
                            r.col(|ui| {
                                ui.monospace(&row.target);
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

impl RenamerApp {
    fn local_controls(&mut self, ui: &mut egui::Ui) {
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
    }

    fn pcloud_controls(&mut self, ui: &mut egui::Ui) {
        if self.pc_client.is_none() {
            ui.horizontal(|ui| {
                ui.label("Region:");
                ui.selectable_value(&mut self.pc_region, Region::Us, "US");
                ui.selectable_value(&mut self.pc_region, Region::Eu, "EU");
            });
            ui.horizontal(|ui| {
                ui.label("Email:");
                ui.add(egui::TextEdit::singleline(&mut self.pc_email).desired_width(220.0));
            });
            ui.horizontal(|ui| {
                ui.label("Password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.pc_password)
                        .password(true)
                        .desired_width(220.0),
                );
                let ctx = ui.ctx().clone();
                if ui.button("Log in").clicked() {
                    self.do_login(&ctx);
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::from_rgb(120, 200, 120), "● Connected");
                if ui.button("Log out").clicked() {
                    self.pc_client = None;
                    self.remote_plan = None;
                    self.remote_undo.clear();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Remote folder:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.pc_path)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
            });
        }
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
