//! The indexing pipeline: turn a folder of photos into catalog rows.
//!
//! Indexing runs in stages, cheapest first, so that the expensive work is only
//! done on files that actually need it:
//!
//! 1. **Walk** — stat every file. Anything whose `(size, mtime)` matches the
//!    catalog is skipped entirely: no EXIF read, no hash, no decode. This is what
//!    makes a rescan of an unchanged library nearly free.
//! 2. **Reconcile** — flag catalog rows whose files have disappeared.
//! 3. **Content hashes** — only for files that share a size with another file.
//!    Files with a unique size cannot be byte-duplicates of anything, so on a
//!    typical library the great majority are never read at all.
//! 4. **Decode** — perceptual hash, dominant colour and thumbnail, all from a
//!    single decode per file, across all cores. Optional, because it is the only
//!    stage that must read every image in full.
//!
//! Anything else needing per-pixel access belongs in stage 4 alongside the rest.
//! Adding a separate pass later means re-reading and re-decoding the whole
//! library; adding it here is close to free.
//!
//! Stage 4 is the reason indexing is worth caching: everything it produces is
//! stored in the catalog and the thumbnail cache, so it happens once per file per
//! edit, not once per launch.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use walkdir::WalkDir;

use crate::catalog::{Catalog, CatalogStats, MediaRow};
use crate::engine::{Progress, UNDO_FILE};
use crate::hashing;
use crate::metadata::{self, FileMeta};
use crate::thumbs;

/// What to index and how thoroughly.
#[derive(Debug, Clone)]
pub struct IndexConfig {
    pub root: PathBuf,
    pub recursive: bool,
    /// Read EXIF for images. Opens file contents, which on a streamed cloud
    /// filesystem triggers downloads — but without it there is no capture date,
    /// camera, or burst detection.
    pub read_exif: bool,
    /// Run the decode pass: perceptual hashes (near-duplicate detection) and
    /// thumbnails. The slowest stage by far on a first run.
    pub compute_phash: bool,
    /// Ignore files that are not recognizable images.
    pub images_only: bool,
}

impl IndexConfig {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            recursive: true,
            read_exif: true,
            compute_phash: true,
            images_only: true,
        }
    }
}

/// Summary of an indexing run.
#[derive(Debug, Clone, Default)]
pub struct IndexOutcome {
    /// Files seen by the walk.
    pub scanned: usize,
    /// Files that were new or had changed since the last index.
    pub updated: usize,
    /// Files skipped because the catalog was already current for them.
    pub unchanged: usize,
    pub hashed: usize,
    pub phashed: usize,
    pub thumbs: usize,
    /// Catalog rows whose files are no longer on disk.
    pub missing: usize,
    pub errors: Vec<String>,
    pub stats: CatalogStats,
}

impl IndexOutcome {
    /// One-line summary for the status bar.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} file(s): {} updated, {} unchanged",
            self.scanned, self.updated, self.unchanged
        );
        if self.hashed > 0 {
            s.push_str(&format!(", {} hashed", self.hashed));
        }
        if self.phashed > 0 {
            s.push_str(&format!(", {} analysed", self.phashed));
        }
        if self.thumbs > 0 {
            s.push_str(&format!(", {} thumbnails", self.thumbs));
        }
        if self.missing > 0 {
            s.push_str(&format!(", {} missing", self.missing));
        }
        if !self.errors.is_empty() {
            s.push_str(&format!(", {} error(s)", self.errors.len()));
        }
        s
    }
}

/// Extensions the bundled decoders can actually read.
///
/// Deliberately narrower than [`metadata::is_image_ext`], which also lists HEIC
/// and camera RAW: those carry EXIF worth reading but cannot be decoded here, so
/// attempting a decode would waste a full file read per scan. They still take
/// part in exact-duplicate detection.
pub fn is_decodable_ext(ext: &str) -> bool {
    matches!(
        ext.to_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp"
    )
}

/// Index `cfg.root` into `catalog`, reporting staged progress.
pub fn index_library(
    catalog: &Catalog,
    cfg: &IndexConfig,
    progress: &Progress,
) -> IndexOutcome {
    let mut out = IndexOutcome::default();

    // ---- stage 1: walk + stat, reusing everything already cached ----
    progress.start_stage("Scanning", 0);
    let files = collect_files(cfg);
    progress.start_stage("Scanning", files.len());

    let stamps = match catalog.stamps_under(&cfg.root) {
        Ok(s) => s,
        Err(e) => {
            out.errors.push(format!("catalog: {e}"));
            return out;
        }
    };

    let mut seen_ids: Vec<i64> = Vec::with_capacity(files.len());
    for path in &files {
        progress.inc();
        out.scanned += 1;

        let Ok(fsmeta) = std::fs::metadata(path) else {
            out.errors.push(format!("{}: cannot stat", path.display()));
            continue;
        };
        let size = fsmeta.len();
        let mtime = mtime_secs(&fsmeta);

        // The fast path that makes rescans cheap. A row stamped with an older
        // metadata generation is re-examined even though the file has not
        // changed, so a newly-extracted field (GPS, most recently) reaches
        // files that were catalogued before it existed.
        if let Some(stamp) = stamps.get(path) {
            if stamp.size == size
                && stamp.mtime == mtime
                && stamp.meta_version == crate::catalog::META_VERSION
            {
                seen_ids.push(stamp.id);
                out.unchanged += 1;
                continue;
            }
        }

        let want_exif = cfg.read_exif && metadata::is_image_ext(&extension_of(path));
        let meta = FileMeta::read_with(path, want_exif);
        match catalog.upsert_media(&to_row(path, &meta, size, mtime)) {
            Ok(id) => {
                seen_ids.push(id);
                out.updated += 1;
            }
            Err(e) => out.errors.push(format!("{}: {e}", path.display())),
        }
    }

    // ---- stage 2: reconcile files that vanished ----
    match catalog.mark_missing(&cfg.root, &seen_ids) {
        Ok(n) => out.missing = n,
        Err(e) => out.errors.push(format!("reconcile: {e}")),
    }

    // ---- stage 3: content hashes, only where a size collision makes it useful ----
    match catalog.rows_needing_content_hash() {
        Ok(work) => {
            progress.start_stage("Hashing", work.len());
            for (id, path) in work {
                progress.inc();
                match hashing::content_hash(&path) {
                    Ok(h) => match catalog.set_content_hash(id, &h) {
                        Ok(()) => out.hashed += 1,
                        Err(e) => out.errors.push(format!("{}: {e}", path.display())),
                    },
                    Err(e) => out.errors.push(format!("{}: {e}", path.display())),
                }
            }
        }
        Err(e) => out.errors.push(format!("hash query: {e}")),
    }

    // ---- stage 4: decode once for perceptual hash, colour and thumbnail ----
    if cfg.compute_phash {
        match catalog.rows_needing_analysis() {
            Ok(work) => {
                let work: Vec<(i64, PathBuf)> = work
                    .into_iter()
                    .filter(|(_, p)| is_decodable_ext(&extension_of(p)))
                    .collect();
                progress.start_stage("Analysing images", work.len());
                run_decode_pass(catalog, &work, progress, &mut out);
            }
            Err(e) => out.errors.push(format!("analysis query: {e}")),
        }
    }

    if let Ok(stats) = catalog.stats_under(&cfg.root) {
        out.stats = stats;
    }
    progress.start_stage("", 0);
    out
}

/// Everything one decode produces.
struct Decoded {
    id: i64,
    phash: Option<u64>,
    /// Dominant colour, packed `0xRRGGBB`.
    dom_color: Option<u32>,
    /// True pixel dimensions, measured rather than taken from EXIF.
    dims: Option<(u32, u32)>,
    thumb_written: bool,
    error: Option<String>,
}

/// Decode `work` across all available cores.
///
/// Decoding is CPU-bound and dominates a first index, so it is parallelised.
/// Catalog writes stay on this thread: one `Connection` is not `Sync`, and
/// serializing the writes avoids SQLite lock contention entirely.
fn run_decode_pass(
    catalog: &Catalog,
    work: &[(i64, PathBuf)],
    progress: &Progress,
    out: &mut IndexOutcome,
) {
    if work.is_empty() {
        return;
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(work.len());

    let cursor = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Decoded>();

    std::thread::scope(|scope| {
        for _ in 0..threads {
            let tx = tx.clone();
            let cursor = &cursor;
            scope.spawn(move || {
                loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= work.len() {
                        break;
                    }
                    let (id, path) = &work[i];
                    if tx.send(decode_one(*id, path)).is_err() {
                        break; // receiver gone
                    }
                }
            });
        }
        // Drop the original sender so the loop below ends once workers finish.
        drop(tx);

        for done in rx {
            progress.inc();
            if let Some(e) = done.error {
                out.errors.push(e);
            }
            if done.thumb_written {
                out.thumbs += 1;
            }
            if done.phash.is_some() {
                out.phashed += 1;
            }
            if let Err(e) = catalog.set_analysis(done.id, done.phash, done.dom_color) {
                out.errors.push(format!("analysis write: {e}"));
            }
            if let Some((w, h)) = done.dims {
                if let Err(e) = catalog.set_dimensions(done.id, w, h) {
                    out.errors.push(format!("dimensions write: {e}"));
                }
            }
        }
    });
}

fn decode_one(id: i64, path: &Path) -> Decoded {
    let (size, mtime) = match std::fs::metadata(path) {
        Ok(m) => (m.len(), mtime_secs(&m)),
        Err(e) => {
            return Decoded {
                id,
                phash: None,
                dom_color: None,
                dims: None,
                thumb_written: false,
                error: Some(format!("{}: {e}", path.display())),
            }
        }
    };

    let img = match image::open(path) {
        Ok(i) => i,
        Err(_) => {
            // Not a failure worth reporting: plenty of files in a photo folder
            // are simply not decodable images.
            return Decoded {
                id,
                phash: None,
                dom_color: None,
                dims: None,
                thumb_written: false,
                error: None,
            };
        }
    };

    // One decode, four derived products. Adding any of these as a separate pass
    // later would mean re-reading and re-decoding the entire library.
    let phash = Some(hashing::perceptual_hash_from_image(&img));
    let dom_color = Some(crate::colour::dominant_colour(&img));
    let dims = Some((img.width(), img.height()));
    // Same decode feeds the thumbnail, so a first index pays for one decode.
    let mut thumb_written = false;
    let mut error = None;
    if !thumbs::is_cached(path, size, mtime) {
        match thumbs::write_thumb(&img, path, size, mtime) {
            Ok(_) => thumb_written = true,
            Err(e) => error = Some(format!("{}: thumbnail: {e}", path.display())),
        }
    }

    Decoded {
        id,
        phash,
        dom_color,
        dims,
        thumb_written,
        error,
    }
}

/// Content hash for `row`, computing and storing it if absent.
///
/// User-assigned metadata is keyed on content hash so it survives renames, but
/// the indexer only hashes files that share a size with another. Tagging a file
/// is therefore the moment its hash has to exist, so it is computed on demand —
/// one file read, rather than hashing the whole library up front.
pub fn ensure_content_hash(catalog: &Catalog, row: &MediaRow) -> Result<String, String> {
    if let Some(h) = &row.content_hash {
        return Ok(h.clone());
    }
    let hash =
        hashing::content_hash(&row.path).map_err(|e| format!("{}: {e}", row.path.display()))?;
    catalog
        .set_content_hash(row.id, &hash)
        .map_err(|e| format!("catalog: {e}"))?;
    Ok(hash)
}

/// Files to consider, sorted for deterministic ordering.
fn collect_files(cfg: &IndexConfig) -> Vec<PathBuf> {
    let walker = WalkDir::new(&cfg.root).min_depth(1);
    let walker = if cfg.recursive {
        walker
    } else {
        walker.max_depth(1)
    };
    let mut files: Vec<PathBuf> = walker
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        // The renamer's own undo log is bookkeeping, not media.
        .filter(|e| e.file_name().to_str() != Some(UNDO_FILE))
        .map(|e| e.into_path())
        .filter(|p| !cfg.images_only || metadata::is_image_ext(&extension_of(p)))
        .collect();
    files.sort();
    files
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Modified time as unix seconds, 0 when unavailable.
fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn to_row(path: &Path, meta: &FileMeta, size: u64, mtime: i64) -> MediaRow {
    MediaRow {
        id: 0,
        path: path.to_path_buf(),
        file_name: path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        ext: meta.ext.clone(),
        size,
        mtime,
        date_taken: meta.date_taken.map(|d| d.timestamp()),
        camera_make: meta.camera_make.clone(),
        camera_model: meta.camera_model.clone(),
        lens: meta.lens.clone(),
        iso: meta.iso.clone(),
        width: meta.width.as_deref().and_then(|s| s.parse().ok()),
        height: meta.height.as_deref().and_then(|s| s.parse().ok()),
        lat: meta.lat,
        lon: meta.lon,
        // Filled in by later stages.
        content_hash: None,
        phash: None,
        dom_color: None,
        missing: false,
        // Owned by `user_meta` and joined in at read time. `upsert_media`
        // ignores these, so the values here are inert — indexing can never
        // clobber a rating or a hidden flag.
        hidden: false,
        rating: 0,
        kind_override: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Visibility;
    use image::{Rgb, RgbImage};

    struct Fixture {
        tmp: PathBuf,
        library: PathBuf,
        catalog: Catalog,
    }

    fn fixture(name: &str) -> (Fixture, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::paths::env_lock();
        let tmp = std::env::temp_dir().join(format!("renamer_idx_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let library = tmp.join("library");
        std::fs::create_dir_all(&library).unwrap();
        std::env::set_var("RENAMER_CACHE_DIR", tmp.join("cache"));
        std::env::set_var("RENAMER_DATA_DIR", tmp.join("data"));
        let catalog = Catalog::open_in_memory().unwrap();
        (
            Fixture {
                tmp,
                library,
                catalog,
            },
            guard,
        )
    }

    fn cleanup(f: &Fixture) {
        std::env::remove_var("RENAMER_CACHE_DIR");
        std::env::remove_var("RENAMER_DATA_DIR");
        let _ = std::fs::remove_dir_all(&f.tmp);
    }

    impl Fixture {
        /// Write a distinct PNG. `shift` changes the pixels, so two images with
        /// the same shift are byte-identical and different shifts are not.
        fn image(&self, rel: &str, w: u32, h: u32, shift: u8) -> PathBuf {
            let p = self.library.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            let mut img = RgbImage::new(w, h);
            for (x, y, px) in img.enumerate_pixels_mut() {
                let v = ((x * 7 + y * 13 + shift as u32 * 61) % 256) as u8;
                *px = Rgb([v, v.wrapping_add(40), v.wrapping_add(80)]);
            }
            img.save(&p).unwrap();
            p
        }

        fn cfg(&self) -> IndexConfig {
            IndexConfig::new(self.library.clone())
        }
    }

    #[test]
    fn indexes_images_and_computes_hashes_and_thumbnails() {
        let (f, _g) = fixture("basic");
        f.image("a.png", 40, 30, 1);
        f.image("nested/b.png", 40, 30, 2);

        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
        assert_eq!(out.scanned, 2);
        assert_eq!(out.updated, 2);
        assert_eq!(out.unchanged, 0);
        assert_eq!(out.phashed, 2, "both images analysed");
        assert_eq!(out.thumbs, 2, "thumbnails generated from the same decode");

        let rows = f.catalog.all_present(Visibility::All).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.phash.is_some()));
        assert!(rows.iter().all(|r| r.width == Some(40)));
        assert!(rows.iter().all(|r| r.height == Some(30)));
        assert!(thumbs::cache_size() > 0);

        cleanup(&f);
    }

    #[test]
    fn rescan_of_an_unchanged_library_does_no_work() {
        let (f, _g) = fixture("rescan");
        f.image("a.png", 32, 32, 1);
        f.image("b.png", 32, 32, 2);

        let first = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(first.updated, 2);

        // This is the payoff of the catalog: nothing is re-read.
        let second = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(second.scanned, 2);
        assert_eq!(second.unchanged, 2);
        assert_eq!(second.updated, 0);
        assert_eq!(second.hashed, 0);
        assert_eq!(second.phashed, 0, "no image re-decoded");
        assert_eq!(second.thumbs, 0);

        cleanup(&f);
    }

    #[test]
    fn identical_files_get_content_hashes_and_group_as_exact_duplicates() {
        let (f, _g) = fixture("exact");
        let a = f.image("a.png", 32, 32, 5);
        // A byte-for-byte copy, which is the common "imported twice" case.
        let b = f.library.join("copies/a-copy.png");
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::copy(&a, &b).unwrap();
        // And a file with unique contents, which must not be hashed at all.
        f.image("unique.png", 64, 48, 9);

        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
        assert_eq!(out.scanned, 3);
        assert_eq!(
            out.hashed, 2,
            "only the size-colliding pair is read for hashing"
        );

        let rows = f.catalog.all_present(Visibility::All).unwrap();
        let hashed: Vec<_> = rows.iter().filter(|r| r.content_hash.is_some()).collect();
        assert_eq!(hashed.len(), 2);
        assert_eq!(hashed[0].content_hash, hashed[1].content_hash);

        let rep = crate::dupes::find_duplicates(&rows, &crate::dupes::DupConfig::default());
        assert_eq!(rep.count_of(crate::dupes::DupKind::Exact), 1);

        cleanup(&f);
    }

    #[test]
    fn resized_copy_is_found_as_a_near_duplicate() {
        // End-to-end proof of the near-duplicate path: same picture, different
        // bytes and dimensions, so only perceptual hashing can connect them.
        let (f, _g) = fixture("near");
        let orig = f.image("orig.png", 200, 200, 3);
        let img = image::open(&orig).unwrap();
        let small = img.resize_exact(100, 100, image::imageops::FilterType::Triangle);
        small.save(f.library.join("shrunk.png")).unwrap();

        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);

        let rows = f.catalog.all_present(Visibility::All).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.phash.is_some()));

        let rep = crate::dupes::find_duplicates(&rows, &crate::dupes::DupConfig::default());
        assert_eq!(
            rep.count_of(crate::dupes::DupKind::Near),
            1,
            "resized copy should be flagged as a near-duplicate"
        );
        // The full-size original is the suggested keeper.
        let g = &rep.groups[0];
        assert_eq!(g.keeper_row().width, Some(200));

        cleanup(&f);
    }

    #[test]
    fn changed_file_is_reindexed_and_stale_hashes_dropped() {
        let (f, _g) = fixture("changed");
        let a = f.image("a.png", 32, 32, 1);
        index_library(&f.catalog, &f.cfg(), &Progress::default());
        let before = f.catalog.all_present(Visibility::All).unwrap()[0].phash;

        // Rewrite with different content and a newer mtime.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let mut img = RgbImage::new(32, 32);
        for (_x, _y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([255, 0, 0]);
        }
        img.save(&a).unwrap();

        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(out.updated, 1, "changed file re-examined");
        let after = f.catalog.all_present(Visibility::All).unwrap()[0].phash;
        assert!(after.is_some());
        assert_ne!(before, after, "perceptual hash recomputed for new content");

        cleanup(&f);
    }

    #[test]
    fn deleted_file_is_marked_missing() {
        let (f, _g) = fixture("missing");
        let a = f.image("a.png", 32, 32, 1);
        f.image("b.png", 40, 40, 2);
        index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(f.catalog.all_present(Visibility::All).unwrap().len(), 2);

        std::fs::remove_file(&a).unwrap();
        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(out.missing, 1);
        assert_eq!(out.scanned, 1);
        assert_eq!(f.catalog.all_present(Visibility::All).unwrap().len(), 1);

        cleanup(&f);
    }

    #[test]
    fn non_images_are_skipped_when_images_only() {
        let (f, _g) = fixture("filter");
        f.image("a.png", 32, 32, 1);
        std::fs::write(f.library.join("notes.txt"), b"not a photo").unwrap();

        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(out.scanned, 1, "text file ignored");

        let mut cfg = f.cfg();
        cfg.images_only = false;
        let out = index_library(&f.catalog, &cfg, &Progress::default());
        assert_eq!(out.scanned, 2, "text file included when asked");

        cleanup(&f);
    }

    #[test]
    fn non_recursive_index_stays_at_the_top_level() {
        let (f, _g) = fixture("shallow");
        f.image("top.png", 32, 32, 1);
        f.image("sub/deep.png", 32, 32, 2);

        let mut cfg = f.cfg();
        cfg.recursive = false;
        let out = index_library(&f.catalog, &cfg, &Progress::default());
        assert_eq!(out.scanned, 1);

        cleanup(&f);
    }

    #[test]
    fn phash_pass_can_be_disabled() {
        let (f, _g) = fixture("nophash");
        f.image("a.png", 32, 32, 1);
        let mut cfg = f.cfg();
        cfg.compute_phash = false;

        let out = index_library(&f.catalog, &cfg, &Progress::default());
        assert_eq!(out.phashed, 0);
        assert_eq!(out.thumbs, 0);
        assert!(f.catalog.all_present(Visibility::All).unwrap()[0].phash.is_none());
        assert_eq!(thumbs::cache_size(), 0, "no thumbnails written");

        cleanup(&f);
    }

    #[test]
    fn progress_reaches_completion_and_clears() {
        let (f, _g) = fixture("progress");
        f.image("a.png", 32, 32, 1);
        f.image("b.png", 40, 40, 2);

        let p = Progress::default();
        index_library(&f.catalog, &f.cfg(), &p);
        let (done, total) = p.snapshot();
        assert_eq!(done, total, "final stage completed");
        assert_eq!(p.stage(), "", "stage cleared when finished");

        cleanup(&f);
    }

    #[test]
    fn indexing_writes_nothing_into_the_library() {
        // The promise from the design: no sidecar files, no database, no
        // thumbnail folder inside the photo library.
        let (f, _g) = fixture("noleak");
        f.image("a.png", 40, 30, 1);
        f.image("sub/b.png", 40, 30, 2);

        let before = tree_snapshot(&f.library);
        index_library(&f.catalog, &f.cfg(), &Progress::default());
        let after = tree_snapshot(&f.library);
        assert_eq!(before, after, "library must be untouched by indexing");

        cleanup(&f);
    }

    fn tree_snapshot(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.path().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn empty_library_indexes_cleanly() {
        let (f, _g) = fixture("emptylib");
        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(out.scanned, 0);
        assert!(out.errors.is_empty());
        assert_eq!(out.stats, CatalogStats::default());
        cleanup(&f);
    }

    #[test]
    fn dominant_colour_comes_out_of_the_same_decode_pass() {
        let (f, _g) = fixture("colour");
        // A strongly red image, written as a solid fill.
        let p = f.library.join("red.png");
        let mut img = RgbImage::new(48, 48);
        for (_x, _y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([210, 25, 30]);
        }
        img.save(&p).unwrap();

        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);

        let rows = f.catalog.all_present(Visibility::All).unwrap();
        let dom = rows[0].dom_color.expect("colour extracted");
        let (r, g, b) = crate::colour::unpack(dom);
        assert!(r > 180 && g < 70 && b < 70, "expected red, got {dom:06X}");
        // And it arrived alongside the perceptual hash from one decode.
        assert!(rows[0].phash.is_some());

        cleanup(&f);
    }

    #[test]
    fn colour_is_backfilled_without_a_full_reindex() {
        // An image analysed before colour extraction existed: the next index
        // must fill it in without re-walking or re-hashing everything.
        let (f, _g) = fixture("backfill");
        f.image("a.png", 32, 32, 4);
        index_library(&f.catalog, &f.cfg(), &Progress::default());

        let id = f.catalog.all_present(Visibility::All).unwrap()[0].id;
        f.catalog.set_analysis(id, Some(9), None).unwrap(); // simulate old catalog

        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(out.unchanged, 1, "file itself was not re-read");
        assert!(f.catalog.all_present(Visibility::All).unwrap()[0]
            .dom_color
            .is_some());

        cleanup(&f);
    }

    #[test]
    fn ensure_content_hash_computes_on_demand_and_caches() {
        // Tagging a file is the moment its hash has to exist, since indexing
        // only hashes size-colliding files.
        let (f, _g) = fixture("ondemand");
        f.image("solo.png", 32, 32, 1);
        index_library(&f.catalog, &f.cfg(), &Progress::default());

        let row = f.catalog.all_present(Visibility::All).unwrap()[0].clone();
        assert!(row.content_hash.is_none(), "unique size, so never hashed");

        let hash = ensure_content_hash(&f.catalog, &row).unwrap();
        assert_eq!(hash.len(), 64);
        // Persisted, so a second call is free and returns the same value.
        let stored = f.catalog.all_present(Visibility::All).unwrap()[0].clone();
        assert_eq!(stored.content_hash.as_deref(), Some(hash.as_str()));
        assert_eq!(ensure_content_hash(&f.catalog, &stored).unwrap(), hash);

        cleanup(&f);
    }

    #[test]
    fn reindexing_never_clobbers_user_metadata() {
        // `to_row` builds a MediaRow with inert user-metadata fields, and
        // `upsert_media` ignores them. If either ever changed, re-indexing would
        // silently wipe ratings and hidden flags.
        use crate::kind::MediaKind;
        let (f, _g) = fixture("noclobber");
        f.image("a.png", 32, 32, 1);
        index_library(&f.catalog, &f.cfg(), &Progress::default());

        let row = f.catalog.all_present(Visibility::All).unwrap()[0].clone();
        let hash = ensure_content_hash(&f.catalog, &row).unwrap();
        f.catalog.set_rating(&hash, 5).unwrap();
        f.catalog.set_hidden(&hash, true).unwrap();
        f.catalog.set_kind(&hash, Some(MediaKind::Saved)).unwrap();

        // Re-index twice, to catch both the "skipped, unchanged" fast path and
        // anything that might rewrite the row.
        index_library(&f.catalog, &f.cfg(), &Progress::default());
        index_library(&f.catalog, &f.cfg(), &Progress::default());

        let after = &f.catalog.all_present(Visibility::All).unwrap()[0];
        assert_eq!(after.rating, 5, "rating survived re-indexing");
        assert!(after.hidden, "hidden flag survived re-indexing");
        assert_eq!(after.kind_override, Some(MediaKind::Saved));

        cleanup(&f);
    }

    #[test]
    fn replacing_a_file_with_different_bytes_does_not_inherit_its_metadata() {
        // A direct consequence of keying user metadata on content hash: metadata
        // follows the *bytes*, not the path. Overwrite a photo with a different
        // image and the new content starts unrated, because as far as the
        // catalog is concerned it is a different picture that happens to live at
        // the same path.
        //
        // Pinned deliberately. It is the right behaviour for "this path now
        // holds something else", and arguably the wrong one for "I cropped this
        // photo and saved over it" — so it should not change by accident.
        use crate::kind::MediaKind;
        let (f, _g) = fixture("rebytes");
        f.image("a.png", 32, 32, 1);
        index_library(&f.catalog, &f.cfg(), &Progress::default());

        let row = f.catalog.all_present(Visibility::All).unwrap()[0].clone();
        let old_hash = ensure_content_hash(&f.catalog, &row).unwrap();
        f.catalog.set_rating(&old_hash, 5).unwrap();
        f.catalog.set_kind(&old_hash, Some(MediaKind::Saved)).unwrap();

        // mtime has one-second resolution, so wait before rewriting.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        f.image("a.png", 48, 48, 9); // same path, different content

        index_library(&f.catalog, &f.cfg(), &Progress::default());
        let after = &f.catalog.all_present(Visibility::All).unwrap()[0];
        assert_eq!(after.rating, 0, "new content is unrated");
        assert_eq!(after.kind_override, None);

        // The old rating is not lost, just detached: it still applies to those
        // bytes should they reappear.
        assert_eq!(f.catalog.rating_of(&old_hash).unwrap(), 5);

        cleanup(&f);
    }

    #[test]
    fn a_stale_metadata_stamp_forces_a_re_read_of_an_unchanged_file() {
        // The backfill mechanism. Without it, adding a new extracted field
        // (GPS, most recently) would only ever apply to new and changed files,
        // leaving an existing library permanently missing it.
        let (f, _g) = fixture("metaversion");
        f.image("a.png", 32, 32, 1);
        index_library(&f.catalog, &f.cfg(), &Progress::default());

        // Unchanged: skipped entirely.
        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(out.unchanged, 1);
        assert_eq!(out.updated, 0);

        // Simulate a row written by an older build, without touching the file.
        f.catalog.mark_meta_stale_for_tests();

        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(
            out.updated, 1,
            "stale stamp must re-read the file even though it is unchanged"
        );
        assert_eq!(out.unchanged, 0);

        // And the stamp is brought up to date, so it settles after one pass.
        let out = index_library(&f.catalog, &f.cfg(), &Progress::default());
        assert_eq!(out.unchanged, 1);
        assert_eq!(out.updated, 0);

        cleanup(&f);
    }

    #[test]
    fn decodable_extension_list_excludes_raw_and_heic() {
        // RAW/HEIC carry EXIF worth reading but cannot be decoded here, so the
        // decode pass must not waste a full read attempting them.
        assert!(is_decodable_ext("jpg"));
        assert!(is_decodable_ext("JPEG"));
        assert!(is_decodable_ext("png"));
        assert!(!is_decodable_ext("cr2"));
        assert!(!is_decodable_ext("heic"));
        assert!(!is_decodable_ext("txt"));
        // ...but they are still recognized as images worth cataloguing.
        assert!(metadata::is_image_ext("cr2"));
        assert!(metadata::is_image_ext("heic"));
    }

    #[test]
    fn summary_mentions_the_interesting_counts() {
        let out = IndexOutcome {
            scanned: 10,
            updated: 3,
            unchanged: 7,
            hashed: 2,
            missing: 1,
            errors: vec!["boom".into()],
            ..IndexOutcome::default()
        };
        let s = out.summary();
        assert!(s.contains("10 file(s)"));
        assert!(s.contains("3 updated"));
        assert!(s.contains("7 unchanged"));
        assert!(s.contains("2 hashed"));
        assert!(s.contains("1 missing"));
        assert!(s.contains("1 error(s)"));
    }
}
