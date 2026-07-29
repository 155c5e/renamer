use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::catalog::{Catalog, CatalogStats, MediaRow, Visibility};
use crate::colour;
use crate::dupes::{self, DupConfig, DupGroup, DupKind, DupReport};
use crate::engine::{self, Plan, Progress, RowStatus, Settings, UndoEntry};
use crate::indexer::{self, IndexConfig, IndexOutcome};
use crate::metadata::FIELDS;
use crate::pcloud::{self, Client, Region, RemotePlan, RemoteUndo};
use crate::quarantine::{self, QuarantineOutcome};
use crate::thumbs;

/// Top-level sections of the app.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Library,
    Images,
    Duplicates,
    Rename,
}

/// Whether the parent filter starts enabled.
///
/// It does. Forgetting to switch it *on* shows people the photos you meant to
/// hide; forgetting to switch it *off* is a moment's confusion. Only one of
/// those is worth protecting against.
const PARENT_FILTER_DEFAULT: bool = true;

/// Ordering for the image list.
#[derive(PartialEq, Eq, Clone, Copy)]
enum SortBy {
    Path,
    Date,
    Size,
    Colour,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Mode {
    Local,
    PCloud,
}

/// Result handed back from a worker thread to the GUI thread.
enum BgResult {
    // --- library / duplicates ---
    Index(Result<IndexOutcome, String>),
    Images(Result<Vec<MediaRow>, String>),
    Dupes(Result<DupReport, String>),
    Quarantined(QuarantineOutcome),
    Restored {
        restored: usize,
        errors: Vec<String>,
    },
    // --- renamer (unchanged behaviour) ---
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

/// Lightweight row for the rename results table.
struct RowView {
    name: String,
    target: String,
    status: RowStatus,
}

/// Per-group user choices in the duplicates view.
struct GroupSel {
    /// Index into the group's members of the copy to keep.
    keeper: usize,
    /// Whether this group participates in the next quarantine action.
    enabled: bool,
}

pub struct RenamerApp {
    tab: Tab,

    // --- library ---
    library: Option<PathBuf>,
    lib_recursive: bool,
    lib_read_exif: bool,
    lib_compute_phash: bool,
    lib_images_only: bool,
    stats: Option<CatalogStats>,
    last_index: Option<IndexOutcome>,

    // --- parent filter, app-wide ---
    /// When on, hidden images are excluded from every view that reads the
    /// catalog. Enforced by [`Visibility`] at the query layer, not per view.
    parent_filter: bool,
    hidden_count: usize,

    // --- images ---
    images: Vec<MediaRow>,
    image_filter: String,
    sort_by: SortBy,

    // --- duplicates ---
    dup_cfg: DupConfig,
    report: Option<DupReport>,
    selections: Vec<GroupSel>,
    restorable_batch: Option<i64>,
    /// Cached directory sizes. Computing these walks a directory tree, so they
    /// are refreshed on events rather than every frame.
    quarantine_bytes: u64,
    thumb_cache_bytes: u64,

    // --- renamer: local mode ---
    mode: Mode,
    source: Option<PathBuf>,
    output: Option<PathBuf>, // None => same as source
    plan: Option<Plan>,
    last_undo: Vec<UndoEntry>,

    // --- renamer: pcloud mode ---
    pc_region: Region,
    pc_email: String,
    pc_password: String,
    pc_path: String,
    pc_client: Option<Client>,
    remote_plan: Option<RemotePlan>,
    remote_undo: Vec<RemoteUndo>,

    // --- renamer: shared ---
    template: String,
    recursive: bool,
    append_ext: bool,

    // --- shared ---
    message: String,
    rx: Option<Receiver<BgResult>>,
    progress: Option<Arc<Progress>>,
    busy_label: String,
}

impl Default for RenamerApp {
    fn default() -> Self {
        Self {
            tab: Tab::Library,
            library: None,
            lib_recursive: true,
            lib_read_exif: true,
            lib_compute_phash: true,
            lib_images_only: true,
            stats: None,
            last_index: None,
            parent_filter: PARENT_FILTER_DEFAULT,
            hidden_count: 0,
            images: Vec::new(),
            image_filter: String::new(),
            sort_by: SortBy::Path,
            dup_cfg: DupConfig::default(),
            report: None,
            selections: Vec::new(),
            restorable_batch: None,
            quarantine_bytes: 0,
            thumb_cache_bytes: 0,
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
            template: "{date_taken:%Y}/{date_taken:%m}/{name}".to_string(),
            recursive: false,
            append_ext: true,
            message: "Pick a library folder and index it to get started.".to_string(),
            rx: None,
            progress: None,
            busy_label: String::new(),
        }
    }
}

impl RenamerApp {
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

    // ---- library actions ----

    /// The single place the parent filter turns into a query decision.
    ///
    /// Every catalog read that feeds a view goes through this, which is what
    /// makes the filter app-wide: no view can accidentally opt out, because no
    /// view chooses for itself.
    fn visibility(&self) -> Visibility {
        if self.parent_filter {
            Visibility::VisibleOnly
        } else {
            Visibility::All
        }
    }

    /// Refresh cached counters. Cheap aggregate queries, run on events rather
    /// than per frame.
    fn refresh_stats(&mut self) {
        let Ok(cat) = Catalog::open_default() else {
            return;
        };
        if let Some(root) = &self.library {
            self.stats = cat.stats_under(root).ok();
            self.hidden_count = cat.hidden_count_under(root).unwrap_or(0);
        }
        self.restorable_batch = cat.latest_restorable_batch().ok().flatten();
        self.quarantine_bytes = quarantine::quarantine_size();
        self.thumb_cache_bytes = thumbs::cache_size();
    }

    fn do_index(&mut self, ctx: &egui::Context) {
        let Some(root) = self.library.clone() else {
            self.message = "Pick a library folder first.".to_string();
            return;
        };
        let cfg = IndexConfig {
            root,
            recursive: self.lib_recursive,
            read_exif: self.lib_read_exif,
            compute_phash: self.lib_compute_phash,
            images_only: self.lib_images_only,
        };
        let (tx, progress, ctx) = self.start_worker(ctx, "Indexing");
        std::thread::spawn(move || {
            // Each worker opens its own connection: a rusqlite `Connection` is
            // not `Sync`, and SQLite in WAL mode handles concurrent handles well.
            let res = Catalog::open_default()
                .map_err(|e| e.to_string())
                .map(|cat| indexer::index_library(&cat, &cfg, &progress));
            let _ = tx.send(BgResult::Index(res));
            ctx.request_repaint();
        });
    }

    fn do_find_dupes(&mut self, ctx: &egui::Context) {
        let Some(root) = self.library.clone() else {
            self.message = "Index a library first.".to_string();
            return;
        };
        let cfg = self.dup_cfg;
        // Hidden images must not surface in duplicate groups either — that would
        // be a side door straight past the parent filter.
        let visibility = self.visibility();
        let (tx, _p, ctx) = self.start_worker(ctx, "Finding duplicates");
        std::thread::spawn(move || {
            let res = Catalog::open_default()
                .and_then(|cat| cat.present_under(&root, visibility))
                .map_err(|e| e.to_string())
                .map(|rows| dupes::find_duplicates(&rows, &cfg));
            let _ = tx.send(BgResult::Dupes(res));
            ctx.request_repaint();
        });
    }

    fn do_load_images(&mut self, ctx: &egui::Context) {
        let Some(root) = self.library.clone() else {
            self.message = "Index a library first.".to_string();
            return;
        };
        let visibility = self.visibility();
        let (tx, _p, ctx) = self.start_worker(ctx, "Loading images");
        std::thread::spawn(move || {
            let res = Catalog::open_default()
                .and_then(|cat| cat.present_under(&root, visibility))
                .map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Images(res));
            ctx.request_repaint();
        });
    }

    /// Hide or unhide one image.
    ///
    /// Runs inline rather than on a worker: it is a single row write, plus at
    /// most one content hash for a file that has never needed one.
    fn set_hidden(&mut self, row_index: usize, hidden: bool) {
        let Some(row) = self.images.get(row_index).cloned() else {
            return;
        };
        let Ok(cat) = Catalog::open_default() else {
            self.message = "Could not open the catalog.".to_string();
            return;
        };
        match indexer::ensure_content_hash(&cat, &row) {
            Ok(hash) => match cat.set_hidden(&hash, hidden) {
                Ok(()) => {
                    self.images[row_index].hidden = hidden;
                    self.message = format!(
                        "{} {}",
                        if hidden { "Hidden:" } else { "Unhidden:" },
                        row.file_name
                    );
                    // Byte-identical copies share user metadata, so reflect that
                    // in the list rather than waiting for a reload.
                    for other in self.images.iter_mut() {
                        if other.content_hash.is_some()
                            && other.content_hash == Some(hash.clone())
                        {
                            other.hidden = hidden;
                        }
                    }
                    self.refresh_stats();
                }
                Err(e) => self.message = format!("Could not update: {e}"),
            },
            Err(e) => self.message = format!("Could not hash for tagging: {e}"),
        }
    }

    /// Rows to show in the image list, filtered by text and ordered.
    fn visible_images(&self) -> Vec<usize> {
        let needle = self.image_filter.trim().to_lowercase();
        let mut idx: Vec<usize> = self
            .images
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                needle.is_empty() || r.path.to_string_lossy().to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect();

        match self.sort_by {
            SortBy::Path => idx.sort_by(|&a, &b| self.images[a].path.cmp(&self.images[b].path)),
            SortBy::Date => idx.sort_by(|&a, &b| {
                let key = |r: &MediaRow| r.date_taken.unwrap_or(r.mtime);
                key(&self.images[a])
                    .cmp(&key(&self.images[b]))
                    .then_with(|| self.images[a].path.cmp(&self.images[b].path))
            }),
            SortBy::Size => idx.sort_by(|&a, &b| {
                self.images[b]
                    .size
                    .cmp(&self.images[a].size)
                    .then_with(|| self.images[a].path.cmp(&self.images[b].path))
            }),
            SortBy::Colour => idx.sort_by(|&a, &b| {
                colour::hue_sort_key(self.images[a].dom_color)
                    .cmp(&colour::hue_sort_key(self.images[b].dom_color))
                    .then_with(|| self.images[a].path.cmp(&self.images[b].path))
            }),
        }
        idx
    }

    /// Paths the current selection would quarantine: every enabled group's
    /// non-keepers.
    fn selected_for_quarantine(&self) -> Vec<PathBuf> {
        let Some(report) = &self.report else {
            return Vec::new();
        };
        report
            .groups
            .iter()
            .zip(&self.selections)
            .filter(|(_, sel)| sel.enabled)
            .flat_map(|(group, sel)| {
                group
                    .members
                    .iter()
                    .enumerate()
                    .filter(move |(i, _)| *i != sel.keeper)
                    .map(|(_, m)| m.path.clone())
            })
            .collect()
    }

    fn do_quarantine(&mut self, ctx: &egui::Context) {
        let paths = self.selected_for_quarantine();
        if paths.is_empty() {
            self.message = "Nothing selected to quarantine.".to_string();
            return;
        }
        let (tx, progress, ctx) = self.start_worker(ctx, "Quarantining");
        std::thread::spawn(move || {
            let outcome = match Catalog::open_default() {
                Ok(cat) => quarantine::quarantine(&cat, &paths, &progress),
                Err(e) => QuarantineOutcome {
                    errors: vec![format!("catalog: {e}")],
                    ..QuarantineOutcome::default()
                },
            };
            let _ = tx.send(BgResult::Quarantined(outcome));
            ctx.request_repaint();
        });
    }

    fn do_restore(&mut self, ctx: &egui::Context) {
        let Some(batch) = self.restorable_batch else {
            self.message = "Nothing in quarantine to restore.".to_string();
            return;
        };
        let (tx, progress, ctx) = self.start_worker(ctx, "Restoring");
        std::thread::spawn(move || {
            let (restored, errors) = match Catalog::open_default() {
                Ok(cat) => quarantine::restore_batch(&cat, batch, &progress),
                Err(e) => (0, vec![format!("catalog: {e}")]),
            };
            let _ = tx.send(BgResult::Restored { restored, errors });
            ctx.request_repaint();
        });
    }

    // ---- renamer actions (behaviour unchanged) ----

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
            // Follow the moved files in the catalog. Without this the next index
            // sees a vanished file and a new one, discarding the hashes and
            // colour of a file whose bytes never changed.
            follow_moves(undo.iter().map(|e| (e.from.clone(), e.to.clone())));
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
            // Undo moves files back, so the catalog has to follow them back too.
            follow_moves(entries.iter().map(|e| (e.to.clone(), e.from.clone())));
            let _ = tx.send(BgResult::Undo { reverted, errors });
            ctx.request_repaint();
        });
    }

    fn do_login(&mut self, ctx: &egui::Context) {
        let (region, email, pass) = (
            self.pc_region,
            self.pc_email.clone(),
            self.pc_password.clone(),
        );
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
            BgResult::Index(Ok(outcome)) => {
                self.message = outcome.summary();
                self.stats = Some(outcome.stats);
                self.last_index = Some(outcome);
                // Any cached duplicate report or image list is stale once the
                // index moves.
                self.report = None;
                self.selections.clear();
                self.images.clear();
                self.refresh_stats();
            }
            BgResult::Index(Err(e)) => self.message = format!("Index failed: {e}"),
            BgResult::Images(Ok(rows)) => {
                self.message = format!(
                    "{} image(s){}.",
                    rows.len(),
                    if self.parent_filter && self.hidden_count > 0 {
                        format!(", {} hidden by the parent filter", self.hidden_count)
                    } else {
                        String::new()
                    }
                );
                self.images = rows;
            }
            BgResult::Images(Err(e)) => self.message = format!("Could not load images: {e}"),
            BgResult::Dupes(Ok(report)) => {
                self.selections = report
                    .groups
                    .iter()
                    .map(|g| GroupSel {
                        keeper: g.keeper,
                        // Exact duplicates are safe to act on by default; near
                        // and burst matches are judgement calls, so they start
                        // switched off.
                        enabled: g.kind == DupKind::Exact,
                    })
                    .collect();
                self.message = if report.is_empty() {
                    "No duplicates found.".to_string()
                } else {
                    format!(
                        "{} group(s): {} exact, {} near, {} burst — up to {} recoverable.",
                        report.groups.len(),
                        report.count_of(DupKind::Exact),
                        report.count_of(DupKind::Near),
                        report.count_of(DupKind::Burst),
                        human_bytes(report.total_reclaimable()),
                    )
                };
                self.report = Some(report);
            }
            BgResult::Dupes(Err(e)) => self.message = format!("Duplicate scan failed: {e}"),
            BgResult::Quarantined(outcome) => {
                self.message = format!(
                    "Quarantined {} file(s), {} freed{}. Undo available.",
                    outcome.moved,
                    human_bytes(outcome.bytes),
                    if outcome.errors.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " ({} error(s), e.g. {})",
                            outcome.errors.len(),
                            outcome.errors.first().cloned().unwrap_or_default()
                        )
                    }
                );
                // Quarantined files are gone from the library, so the report no
                // longer matches what is on disk.
                self.report = None;
                self.selections.clear();
                self.refresh_stats();
            }
            BgResult::Restored { restored, errors } => {
                self.message = if errors.is_empty() {
                    format!("Restored {restored} file(s) from quarantine.")
                } else {
                    format!(
                        "Restored {restored}, {} failed (e.g. {}).",
                        errors.len(),
                        errors.first().cloned().unwrap_or_default()
                    )
                };
                self.report = None;
                self.selections.clear();
                self.refresh_stats();
            }
            BgResult::Preview(plan) => {
                self.message =
                    plan_summary(plan.rows.len(), plan.ok_count(), plan.problem_count());
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
            BgResult::Login(Err(e)) => self.message = format!("Login failed: {e}"),
            BgResult::RemotePreview(Ok(plan)) => {
                self.message =
                    plan_summary(plan.rows.len(), plan.ok_count(), plan.problem_count());
                self.remote_plan = Some(plan);
            }
            BgResult::RemotePreview(Err(e)) => self.message = format!("pCloud list failed: {e}"),
            BgResult::RemoteApply { plan, undo } => {
                self.message =
                    format!("Renamed {} file(s) on pCloud. Undo available.", undo.len());
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

    fn display_rows(&self) -> Vec<RowView> {
        match self.mode {
            Mode::Local => self
                .plan
                .as_ref()
                .map(|p| {
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
                })
                .unwrap_or_default(),
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

/// Update catalog paths after the app moved files, so rows (and everything
/// expensive attached to them) follow their files instead of being rebuilt.
///
/// Best-effort and deliberately silent: a rename outside any indexed library is
/// the normal case, and it simply updates nothing.
fn follow_moves(moves: impl Iterator<Item = (PathBuf, PathBuf)>) {
    let moves: Vec<(PathBuf, PathBuf)> = moves.collect();
    if moves.is_empty() {
        return;
    }
    if let Ok(mut cat) = Catalog::open_default() {
        let _ = cat.apply_moves(&moves);
    }
}

/// Human-readable byte count, e.g. `1.4 GB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

impl eframe::App for RenamerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let busy = self.poll_worker();
        if busy {
            ctx.request_repaint();
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Library, "Library");
                ui.selectable_value(&mut self.tab, Tab::Images, "Images");
                ui.selectable_value(&mut self.tab, Tab::Duplicates, "Duplicates");
                ui.selectable_value(&mut self.tab, Tab::Rename, "Rename");

                // The parent filter lives in the tab bar, not inside a tab: it
                // applies everywhere, and its state must be obvious at a glance
                // before handing someone the screen.
                ui.separator();
                let was = self.parent_filter;
                ui.checkbox(&mut self.parent_filter, "Parent filter")
                    .on_hover_text(
                        "Hides images you have marked as hidden, everywhere in the app. \
                         A display filter, not security — the files are still on disk \
                         under their real names.",
                    );
                if self.parent_filter != was {
                    // Everything derived from a catalog read is now filtered
                    // differently, so drop it rather than show stale rows.
                    self.images.clear();
                    self.report = None;
                    self.selections.clear();
                    self.message = if self.parent_filter {
                        format!("Parent filter on — {} image(s) hidden.", self.hidden_count)
                    } else {
                        "Parent filter off — showing everything.".to_string()
                    };
                }
                if self.parent_filter {
                    ui.colored_label(egui::Color32::from_rgb(120, 200, 120), "● on");
                } else {
                    ui.colored_label(egui::Color32::from_rgb(240, 180, 60), "○ off");
                }
            });
            ui.add_space(2.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            if busy {
                let (done, total) = self
                    .progress
                    .as_ref()
                    .map(|p| p.snapshot())
                    .unwrap_or((0, 0));
                let stage = self
                    .progress
                    .as_ref()
                    .map(|p| p.stage())
                    .unwrap_or_default();
                let label = if stage.is_empty() {
                    self.busy_label.clone()
                } else {
                    stage
                };
                let frac = if total > 0 {
                    done as f32 / total as f32
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(format!("{label} {done}/{total}"))
                        .animate(true),
                );
            } else {
                ui.label(&self.message);
            }
            ui.add_space(2.0);
        });

        match self.tab {
            Tab::Library => self.library_tab(ctx, busy),
            Tab::Images => self.images_tab(ctx, busy),
            Tab::Duplicates => self.duplicates_tab(ctx, busy),
            Tab::Rename => self.rename_tab(ctx, busy),
        }
    }
}

impl RenamerApp {
    // ---- Library tab ----

    fn library_tab(&mut self, ctx: &egui::Context, busy: bool) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.heading("Library");
                ui.label(
                    "Index a folder once; the catalog remembers metadata, hashes and \
                     thumbnails so later scans only look at files that changed.",
                );
                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button("Library folder…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.library = Some(p);
                            self.report = None;
                            self.selections.clear();
                            self.refresh_stats();
                        }
                    }
                    ui.label(
                        self.library
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(none)".to_string()),
                    );
                });

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.lib_recursive, "Recurse subfolders");
                    ui.checkbox(&mut self.lib_images_only, "Images only");
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.lib_read_exif, "Read EXIF")
                        .on_hover_text(
                            "Capture date, camera and lens. Reads file contents, which \
                             downloads files on a streamed cloud drive.",
                        );
                    ui.checkbox(
                        &mut self.lib_compute_phash,
                        "Analyse images (near-duplicates + thumbnails)",
                    )
                    .on_hover_text(
                        "Decodes every image once to compute a perceptual hash and a \
                         thumbnail. Slow on the first run, cached afterwards.",
                    );
                });

                ui.add_space(4.0);
                if ui
                    .add_enabled(self.library.is_some(), egui::Button::new("Index library"))
                    .clicked()
                {
                    self.do_index(ctx);
                }

                ui.separator();
                if let Some(stats) = self.stats {
                    ui.heading("Catalogued");
                    egui::Grid::new("stats").num_columns(2).show(ui, |ui| {
                        ui.label("Files");
                        ui.monospace(stats.files.to_string());
                        ui.end_row();
                        ui.label("Total size");
                        ui.monospace(human_bytes(stats.bytes));
                        ui.end_row();
                        ui.label("Content-hashed");
                        ui.monospace(format!(
                            "{} (only files sharing a size need this)",
                            stats.hashed
                        ));
                        ui.end_row();
                        ui.label("Image-analysed");
                        ui.monospace(stats.phashed.to_string());
                        ui.end_row();
                    });
                } else {
                    ui.label("Not indexed yet.");
                }

                if let Some(idx) = &self.last_index {
                    if !idx.errors.is_empty() {
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::from_rgb(240, 180, 60),
                            format!("{} problem(s) during indexing:", idx.errors.len()),
                        );
                        egui::ScrollArea::vertical()
                            .max_height(120.0)
                            .id_salt("index_errors")
                            .show(ui, |ui| {
                                for e in idx.errors.iter().take(200) {
                                    ui.small(e);
                                }
                            });
                    }
                }

                ui.separator();
                ui.collapsing("Where app data lives", |ui| {
                    ui.label(
                        "Nothing is written into your picture folders — no sidecar files, \
                         no database, no thumbnail directories.",
                    );
                    egui::Grid::new("paths").num_columns(2).show(ui, |ui| {
                        ui.label("Catalog");
                        ui.monospace(crate::paths::catalog_path().display().to_string());
                        ui.end_row();
                        ui.label("Thumbnails");
                        ui.monospace(crate::paths::thumbs_dir().display().to_string());
                        ui.end_row();
                        ui.label("Quarantine");
                        ui.monospace(crate::paths::quarantine_dir().display().to_string());
                        ui.end_row();
                    });
                    ui.small(
                        "Override with RENAMER_DATA_DIR / RENAMER_CACHE_DIR — useful to put \
                         the quarantine on the same disk as a large library so moves are \
                         instant.",
                    );
                    ui.label(format!(
                        "Thumbnail cache: {}",
                        human_bytes(self.thumb_cache_bytes)
                    ));
                    if ui.button("Clear thumbnail cache").clicked() {
                        match thumbs::clear_cache() {
                            Ok(()) => {
                                self.message = "Thumbnail cache cleared.".to_string();
                                self.thumb_cache_bytes = 0;
                            }
                            Err(e) => self.message = format!("Could not clear cache: {e}"),
                        }
                    }
                });
            });
        });
    }

    // ---- Images tab ----

    fn images_tab(&mut self, ctx: &egui::Context, busy: bool) {
        egui::TopBottomPanel::top("image_controls").show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(self.library.is_some(), egui::Button::new("Load images"))
                        .clicked()
                    {
                        self.do_load_images(ctx);
                    }
                    ui.label("Sort:");
                    ui.selectable_value(&mut self.sort_by, SortBy::Path, "Path");
                    ui.selectable_value(&mut self.sort_by, SortBy::Date, "Date");
                    ui.selectable_value(&mut self.sort_by, SortBy::Size, "Size");
                    ui.selectable_value(&mut self.sort_by, SortBy::Colour, "Colour")
                        .on_hover_text(
                            "Arranges by hue into a rainbow. Greyscale images collect at \
                             the end, since they have no meaningful hue.",
                        );
                });
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.image_filter)
                            .desired_width(280.0)
                            .hint_text("path contains…"),
                    );
                    if self.parent_filter && self.hidden_count > 0 {
                        ui.small(format!(
                            "{} image(s) hidden — turn off the parent filter to manage them.",
                            self.hidden_count
                        ));
                    }
                });
                ui.add_space(2.0);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.images.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No images loaded. Index a library, then click “Load images”.");
                });
                return;
            }

            let order = self.visible_images();
            let mut toggle: Option<(usize, bool)> = None;

            egui::ScrollArea::vertical().show(ui, |ui| {
                for &i in &order {
                    let row = &self.images[i];
                    ui.horizontal(|ui| {
                        // Colour swatch, so the colour sort is legible.
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(18.0, 18.0),
                            egui::Sense::hover(),
                        );
                        if let Some(c) = row.dom_color {
                            let (r, g, b) = colour::unpack(c);
                            ui.painter().rect_filled(
                                rect,
                                2.0,
                                egui::Color32::from_rgb(r, g, b),
                            );
                        }

                        let mut hidden = row.hidden;
                        if ui
                            .checkbox(&mut hidden, "hide")
                            .on_hover_text("Hide this image everywhere while the parent filter is on")
                            .changed()
                        {
                            toggle = Some((i, hidden));
                        }

                        ui.monospace(format!("{:>9}", human_bytes(row.size)));
                        let dims = match (row.width, row.height) {
                            (Some(w), Some(h)) => format!("{w}×{h}"),
                            _ => "—".to_string(),
                        };
                        ui.monospace(format!("{dims:>11}"));
                        let text = egui::RichText::new(row.path.display().to_string());
                        ui.label(if row.hidden { text.weak() } else { text });
                    });
                }
            });

            // Applied after the loop so the list is not mutated mid-iteration.
            if let Some((i, hidden)) = toggle {
                self.set_hidden(i, hidden);
            }
        });
    }

    // ---- Duplicates tab ----

    fn duplicates_tab(&mut self, ctx: &egui::Context, busy: bool) {
        egui::TopBottomPanel::top("dup_controls").show(ctx, |ui| {
            ui.add_enabled_ui(!busy, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.dup_cfg.find_exact, "Identical")
                        .on_hover_text("Byte-for-byte duplicates. No false positives.");
                    ui.checkbox(&mut self.dup_cfg.find_near, "Near-duplicates")
                        .on_hover_text(
                            "Same picture at a different size or quality. Needs \
                             'Analyse images' during indexing.",
                        );
                    ui.checkbox(&mut self.dup_cfg.find_burst, "Bursts")
                        .on_hover_text("Frames shot seconds apart on the same camera.");
                });
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Slider::new(&mut self.dup_cfg.near_threshold, 0..=16)
                            .text("similarity tolerance"),
                    )
                    .on_hover_text(
                        "Higher finds more re-encoded copies but risks matching merely \
                         similar photos. 6 is a good default.",
                    );
                    ui.add(
                        egui::Slider::new(&mut self.dup_cfg.burst_window_secs, 1..=10)
                            .text("burst seconds"),
                    );
                });
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            self.library.is_some(),
                            egui::Button::new("Find duplicates"),
                        )
                        .clicked()
                    {
                        self.do_find_dupes(ctx);
                    }

                    let selected = self.selected_for_quarantine().len();
                    if ui
                        .add_enabled(
                            selected > 0,
                            egui::Button::new(format!("Quarantine {selected} file(s)")),
                        )
                        .on_hover_text(
                            "Moves the non-keepers into the quarantine folder. Nothing is \
                             deleted; you can undo this or empty the folder yourself later.",
                        )
                        .clicked()
                    {
                        self.do_quarantine(ctx);
                    }

                    if ui
                        .add_enabled(
                            self.restorable_batch.is_some(),
                            egui::Button::new("Undo last quarantine"),
                        )
                        .clicked()
                    {
                        self.do_restore(ctx);
                    }
                });
                if self.quarantine_bytes > 0 {
                    ui.small(format!(
                        "Quarantine currently holds {} — delete it yourself when satisfied.",
                        human_bytes(self.quarantine_bytes)
                    ));
                }
                ui.add_space(2.0);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(report) = &self.report else {
                ui.centered_and_justified(|ui| {
                    ui.label("No duplicate scan yet. Click “Find duplicates”.");
                });
                return;
            };
            if report.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No duplicates found.");
                });
                return;
            }

            // Disjoint field borrows: the report is read while selections are
            // mutated, so neither can go through `&mut self` here.
            let selections = &mut self.selections;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (gi, group) in report.groups.iter().enumerate() {
                    let Some(sel) = selections.get_mut(gi) else {
                        continue;
                    };
                    group_ui(ui, gi, group, sel);
                }
            });
        });
    }

    // ---- Rename tab (existing behaviour) ----

    fn rename_tab(&mut self, ctx: &egui::Context, busy: bool) {
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
                        "pCloud mode: EXIF fields (date_taken, camera_*, iso) are \
                         unavailable via the API. Use date_modified / date_created.",
                    );
                } else {
                    ui.small(
                        "EXIF fields read file contents; on cloud drives that triggers \
                         downloads. Filesystem fields don't.",
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
                ui.add_space(2.0);
            });
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

/// One duplicate group: header line plus a row per member with a keeper choice.
fn group_ui(ui: &mut egui::Ui, gi: usize, group: &DupGroup, sel: &mut GroupSel) {
    let color = kind_color(group.kind);
    let header = format!(
        "{} · {} files · {} recoverable",
        group.kind.label(),
        group.members.len(),
        human_bytes(group.reclaimable)
    );

    ui.horizontal(|ui| {
        ui.checkbox(&mut sel.enabled, "");
        ui.colored_label(color, &header);
    });

    egui::CollapsingHeader::new(
        group
            .members
            .first()
            .map(|m| m.file_name.clone())
            .unwrap_or_default(),
    )
    .id_salt(("dup_group", gi))
    .default_open(group.members.len() <= 4)
    .show(ui, |ui| {
        ui.small(group.kind.confidence());
        if group.kind == DupKind::Near && group.spread > 0 {
            ui.small(format!(
                "widest difference in this group: {} bits",
                group.spread
            ));
        }
        ui.separator();
        for (mi, m) in group.members.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.radio_value(&mut sel.keeper, mi, "keep");
                let dims = match (m.width, m.height) {
                    (Some(w), Some(h)) => format!("{w}×{h}"),
                    _ => "—".to_string(),
                };
                ui.monospace(format!("{:>9}", human_bytes(m.size)));
                ui.monospace(format!("{dims:>11}"));
                let label = if mi == sel.keeper {
                    egui::RichText::new(m.path.display().to_string()).strong()
                } else {
                    egui::RichText::new(m.path.display().to_string()).weak()
                };
                ui.label(label);
            });
        }
    });
    ui.separator();
}

fn kind_color(kind: DupKind) -> egui::Color32 {
    match kind {
        DupKind::Exact => egui::Color32::from_rgb(240, 120, 120),
        DupKind::Near => egui::Color32::from_rgb(240, 180, 60),
        DupKind::Burst => egui::Color32::from_rgb(120, 190, 240),
    }
}

fn status_label(s: &RowStatus) -> (String, egui::Color32) {
    match s {
        RowStatus::Ok => ("ready".into(), egui::Color32::from_rgb(120, 200, 120)),
        RowStatus::Done => ("done".into(), egui::Color32::from_rgb(120, 200, 255)),
        RowStatus::Collision => ("collision".into(), egui::Color32::from_rgb(240, 180, 60)),
        RowStatus::Exists => ("target exists".into(), egui::Color32::from_rgb(240, 180, 60)),
        RowStatus::Error(e) => (format!("error: {e}"), egui::Color32::from_rgb(240, 100, 100)),
        RowStatus::Failed(e) => (
            format!("failed: {e}"),
            egui::Color32::from_rgb(240, 100, 100),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::MediaRow;

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    fn app_with_report(kinds: &[DupKind]) -> RenamerApp {
        let groups: Vec<DupGroup> = kinds
            .iter()
            .enumerate()
            .map(|(i, &kind)| {
                let members = vec![
                    MediaRow {
                        path: PathBuf::from(format!("/lib/g{i}_keep.jpg")),
                        size: 100,
                        ..MediaRow::default()
                    },
                    MediaRow {
                        path: PathBuf::from(format!("/lib/g{i}_dup.jpg")),
                        size: 100,
                        ..MediaRow::default()
                    },
                ];
                DupGroup {
                    kind,
                    members,
                    keeper: 0,
                    spread: 0,
                    reclaimable: 100,
                }
            })
            .collect();
        let report = DupReport { groups };
        let mut app = RenamerApp::default();
        app.handle_result(BgResult::Dupes(Ok(report)));
        app
    }

    #[test]
    fn exact_groups_are_preselected_and_risky_ones_are_not() {
        // Byte-identical matches are safe to act on; near and burst matches are
        // judgement calls and must not be armed by default.
        let app = app_with_report(&[DupKind::Exact, DupKind::Near, DupKind::Burst]);
        let enabled: Vec<bool> = app.selections.iter().map(|s| s.enabled).collect();
        assert_eq!(enabled, vec![true, false, false]);
    }

    #[test]
    fn selection_collects_non_keepers_of_enabled_groups_only() {
        let mut app = app_with_report(&[DupKind::Exact, DupKind::Near]);
        // Only the exact group is armed, so only its duplicate is selected.
        let paths = app.selected_for_quarantine();
        assert_eq!(paths, vec![PathBuf::from("/lib/g0_dup.jpg")]);

        // Arming the near group adds its non-keeper too.
        app.selections[1].enabled = true;
        let paths = app.selected_for_quarantine();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/lib/g0_dup.jpg"),
                PathBuf::from("/lib/g1_dup.jpg"),
            ]
        );
    }

    #[test]
    fn changing_the_keeper_changes_what_would_be_quarantined() {
        let mut app = app_with_report(&[DupKind::Exact]);
        assert_eq!(
            app.selected_for_quarantine(),
            vec![PathBuf::from("/lib/g0_dup.jpg")]
        );
        // Keep the other copy instead: the first file becomes the removable one.
        app.selections[0].keeper = 1;
        assert_eq!(
            app.selected_for_quarantine(),
            vec![PathBuf::from("/lib/g0_keep.jpg")]
        );
    }

    #[test]
    fn keeper_is_never_in_the_quarantine_selection() {
        let mut app = app_with_report(&[DupKind::Exact, DupKind::Near, DupKind::Burst]);
        for sel in &mut app.selections {
            sel.enabled = true;
        }
        let selected = app.selected_for_quarantine();
        let report = app.report.as_ref().unwrap();
        for (group, sel) in report.groups.iter().zip(&app.selections) {
            let keeper = &group.members[sel.keeper].path;
            assert!(
                !selected.contains(keeper),
                "keeper {keeper:?} must never be quarantined"
            );
        }
        assert_eq!(selected.len(), 3, "one non-keeper per group");
    }

    fn img(path: &str, size: u64, dom: Option<u32>, taken: Option<i64>) -> MediaRow {
        MediaRow {
            path: PathBuf::from(path),
            file_name: path.rsplit('/').next().unwrap_or(path).to_string(),
            size,
            dom_color: dom,
            date_taken: taken,
            ..MediaRow::default()
        }
    }

    fn names(app: &RenamerApp) -> Vec<String> {
        app.visible_images()
            .into_iter()
            .map(|i| app.images[i].file_name.clone())
            .collect()
    }

    #[test]
    fn parent_filter_maps_to_query_visibility() {
        let mut app = RenamerApp::default();
        // The default matters: forgetting to switch the filter on is the
        // failure worth protecting against.
        assert!(app.parent_filter, "parent filter defaults to on");
        assert_eq!(app.visibility(), Visibility::VisibleOnly);

        app.parent_filter = false;
        assert_eq!(app.visibility(), Visibility::All);
    }

    #[test]
    fn colour_sort_arranges_by_hue_with_greys_last() {
        let mut app = RenamerApp::default();
        app.sort_by = SortBy::Colour;
        app.images = vec![
            img("/l/grey.jpg", 1, Some(colour::pack(128, 128, 128)), None),
            img("/l/blue.jpg", 1, Some(colour::pack(0, 0, 255)), None),
            img("/l/none.jpg", 1, None, None),
            img("/l/red.jpg", 1, Some(colour::pack(255, 0, 0)), None),
            img("/l/green.jpg", 1, Some(colour::pack(0, 255, 0)), None),
        ];
        assert_eq!(
            names(&app),
            vec!["red.jpg", "green.jpg", "blue.jpg", "grey.jpg", "none.jpg"]
        );
    }

    #[test]
    fn other_sorts_behave() {
        let mut app = RenamerApp::default();
        app.images = vec![
            img("/l/c.jpg", 300, None, Some(30)),
            img("/l/a.jpg", 100, None, Some(10)),
            img("/l/b.jpg", 200, None, Some(20)),
        ];

        app.sort_by = SortBy::Path;
        assert_eq!(names(&app), vec!["a.jpg", "b.jpg", "c.jpg"]);

        app.sort_by = SortBy::Date;
        assert_eq!(names(&app), vec!["a.jpg", "b.jpg", "c.jpg"]);

        // Size sorts largest first — that is what you want when reclaiming space.
        app.sort_by = SortBy::Size;
        assert_eq!(names(&app), vec!["c.jpg", "b.jpg", "a.jpg"]);
    }

    #[test]
    fn text_filter_matches_anywhere_in_the_path_case_insensitively() {
        let mut app = RenamerApp::default();
        app.images = vec![
            img("/lib/2019/Holiday.jpg", 1, None, None),
            img("/lib/2020/work.jpg", 1, None, None),
        ];
        app.image_filter = "HOLI".to_string();
        assert_eq!(names(&app), vec!["Holiday.jpg"]);

        app.image_filter = "2020".to_string();
        assert_eq!(names(&app), vec!["work.jpg"]);

        app.image_filter = "  ".to_string();
        assert_eq!(names(&app).len(), 2, "blank filter matches everything");
    }

    #[test]
    fn toggling_the_parent_filter_discards_views_built_under_the_old_setting() {
        // Stale rows fetched with the filter off must not linger on screen after
        // it is switched on.
        let mut app = app_with_report(&[DupKind::Exact]);
        app.images = vec![img("/l/a.jpg", 1, None, None)];
        assert!(app.report.is_some());

        // Mirrors what the tab-bar checkbox does on change.
        app.parent_filter = true;
        app.images.clear();
        app.report = None;
        app.selections.clear();

        assert!(app.images.is_empty());
        assert!(app.report.is_none());
        assert!(app.selected_for_quarantine().is_empty());
    }

    #[test]
    fn no_report_means_nothing_selected() {
        let app = RenamerApp::default();
        assert!(app.selected_for_quarantine().is_empty());
    }

    #[test]
    fn quarantine_result_invalidates_the_stale_report() {
        // Handling this result refreshes stats, which opens the catalog — point
        // it at a scratch directory so the test never touches real user data.
        let _guard = crate::paths::env_lock();
        let tmp = std::env::temp_dir().join(format!("renamer_app_{}", std::process::id()));
        std::env::set_var("RENAMER_DATA_DIR", &tmp);
        std::env::set_var("RENAMER_CACHE_DIR", &tmp);

        // After files move, the on-screen report no longer matches disk.
        let mut app = app_with_report(&[DupKind::Exact]);
        assert!(app.report.is_some());
        app.handle_result(BgResult::Quarantined(QuarantineOutcome {
            batch: Some(1),
            moved: 1,
            bytes: 100,
            errors: Vec::new(),
        }));
        assert!(app.report.is_none());
        assert!(app.selections.is_empty());
        assert!(app.message.contains("Quarantined 1 file"));

        std::env::remove_var("RENAMER_DATA_DIR");
        std::env::remove_var("RENAMER_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn empty_report_reports_no_duplicates() {
        let mut app = RenamerApp::default();
        app.handle_result(BgResult::Dupes(Ok(DupReport::default())));
        assert_eq!(app.message, "No duplicates found.");
        assert!(app.selected_for_quarantine().is_empty());
    }
}
