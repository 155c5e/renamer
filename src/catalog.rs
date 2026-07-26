//! Persistent SQLite catalog of indexed media.
//!
//! The catalog exists so that opening a large library is instant and a rescan
//! only touches files that actually changed. Every expensive thing the indexer
//! computes — EXIF, content hash, perceptual hash — is cached here keyed by
//! `(path, size, mtime)`, so re-indexing an unchanged 80k-photo folder costs a
//! directory walk and nothing else.
//!
//! The database file lives in the platform data directory (see [`crate::paths`]),
//! never inside the library being organized.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

/// Current schema version. Bump when altering the schema and add a migration
/// step in [`Catalog::migrate`].
const SCHEMA_VERSION: i64 = 1;

/// Columns selected by the `present_*` queries, in the order [`read_media_row`]
/// expects. Kept in one place so the two call sites cannot drift apart.
const MEDIA_COLUMNS: &str = "id, path, file_name, ext, size, mtime, date_taken, \
     camera_make, camera_model, lens, iso, width, height, content_hash, phash, missing";

/// A row in the media table: one file, with whatever has been computed for it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaRow {
    pub id: i64,
    pub path: PathBuf,
    pub file_name: String,
    pub ext: String,
    pub size: u64,
    /// Modified time, unix seconds. Part of the cache key.
    pub mtime: i64,
    /// EXIF capture time, unix seconds, when available.
    pub date_taken: Option<i64>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// BLAKE3 of file contents. `None` until the indexer decides it is needed —
    /// only files sharing a size with another file ever get hashed.
    pub content_hash: Option<String>,
    /// 64-bit dHash. Stored as a signed integer because SQLite has no unsigned
    /// type; the round trip is bit-preserving. `None` when the file could not be
    /// decoded (HEIC, most RAW, corrupt files).
    pub phash: Option<u64>,
    /// Set when a previously indexed file is no longer on disk.
    pub missing: bool,
}

/// The minimum needed to decide whether a file must be re-examined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub id: i64,
    pub size: u64,
    pub mtime: i64,
    pub has_content_hash: bool,
    pub has_phash: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatalogStats {
    pub files: usize,
    pub hashed: usize,
    pub phashed: usize,
    pub bytes: u64,
}

/// One quarantined file — enough information to put it back.
#[derive(Debug, Clone, PartialEq)]
pub struct QuarantineEntry {
    pub id: i64,
    pub batch: i64,
    pub original_path: PathBuf,
    pub quarantine_path: PathBuf,
}

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// Open (creating if needed) the catalog at the default location.
    pub fn open_default() -> rusqlite::Result<Catalog> {
        let path = crate::paths::catalog_path();
        if let Some(parent) = path.parent() {
            let _ = crate::paths::ensure_dir(parent);
        }
        Catalog::open(&path)
    }

    pub fn open(path: &Path) -> rusqlite::Result<Catalog> {
        Catalog::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Catalog> {
        Catalog::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Catalog> {
        // WAL keeps the catalog readable from the GUI while a background index
        // writes. `execute_batch` is used rather than `pragma_update` because
        // `PRAGMA journal_mode` *returns a row*, which `pragma_update` rejects.
        // Failure is non-fatal: in-memory databases and some network
        // filesystems do not support WAL, and the default journal still works.
        let _ = conn.execute_batch("PRAGMA journal_mode=WAL;");
        let _ = conn.execute_batch("PRAGMA synchronous=NORMAL;");
        let cat = Catalog { conn };
        cat.migrate()?;
        Ok(cat)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_meta (
                key   TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS media (
                id            INTEGER PRIMARY KEY,
                path          TEXT    NOT NULL UNIQUE,
                file_name     TEXT    NOT NULL,
                ext           TEXT    NOT NULL,
                size          INTEGER NOT NULL,
                mtime         INTEGER NOT NULL,
                date_taken    INTEGER,
                camera_make   TEXT,
                camera_model  TEXT,
                lens          TEXT,
                iso           TEXT,
                width         INTEGER,
                height        INTEGER,
                content_hash  TEXT,
                phash         INTEGER,
                missing       INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS media_size_idx    ON media(size);
            CREATE INDEX IF NOT EXISTS media_hash_idx    ON media(content_hash);
            CREATE INDEX IF NOT EXISTS media_taken_idx   ON media(date_taken);
            CREATE INDEX IF NOT EXISTS media_missing_idx ON media(missing);

            -- Ledger of quarantined files. Nothing is ever unlinked by the app,
            -- so this table plus the quarantine folder is a complete undo record.
            CREATE TABLE IF NOT EXISTS quarantine (
                id              INTEGER PRIMARY KEY,
                batch           INTEGER NOT NULL,
                original_path   TEXT    NOT NULL,
                quarantine_path TEXT    NOT NULL,
                moved_at        INTEGER NOT NULL,
                restored        INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS quarantine_batch_idx ON quarantine(batch);
            "#,
        )?;
        self.conn.execute(
            "INSERT INTO schema_meta(key, value) VALUES('version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    #[allow(dead_code)] // asserted by tests; kept for future migrations
    pub fn schema_version(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'version'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|v| v.unwrap_or(0))
    }

    /// Every known `(path -> stamp)` for files under `root`, so the indexer can
    /// skip unchanged files without issuing a query per file.
    pub fn stamps_under(&self, root: &Path) -> rusqlite::Result<HashMap<PathBuf, Stamp>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, size, mtime, content_hash IS NOT NULL, phash IS NOT NULL
             FROM media WHERE path LIKE ?1 ESCAPE '\\'",
        )?;
        let rows = stmt.query_map(params![like_prefix(root)], |r| {
            Ok((
                PathBuf::from(r.get::<_, String>(1)?),
                Stamp {
                    id: r.get(0)?,
                    size: r.get::<_, i64>(2)? as u64,
                    mtime: r.get(3)?,
                    has_content_hash: r.get(4)?,
                    has_phash: r.get(5)?,
                },
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    /// Insert or update the cheap (walk + EXIF) part of a row.
    ///
    /// Hashes already computed for an *unchanged* file are preserved; a changed
    /// size or mtime means the bytes may differ, so cached hashes are dropped
    /// and the indexer will recompute them.
    pub fn upsert_media(&self, row: &MediaRow) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO media (path, file_name, ext, size, mtime, date_taken,
                                camera_make, camera_model, lens, iso, width, height,
                                content_hash, phash, missing)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,0)
             ON CONFLICT(path) DO UPDATE SET
                file_name = excluded.file_name,
                ext = excluded.ext,
                size = excluded.size,
                mtime = excluded.mtime,
                date_taken = excluded.date_taken,
                camera_make = excluded.camera_make,
                camera_model = excluded.camera_model,
                lens = excluded.lens,
                iso = excluded.iso,
                width = excluded.width,
                height = excluded.height,
                content_hash = CASE
                    WHEN media.size = excluded.size AND media.mtime = excluded.mtime
                    THEN media.content_hash ELSE excluded.content_hash END,
                phash = CASE
                    WHEN media.size = excluded.size AND media.mtime = excluded.mtime
                    THEN media.phash ELSE excluded.phash END,
                missing = 0",
            params![
                path_str(&row.path),
                row.file_name,
                row.ext,
                row.size as i64,
                row.mtime,
                row.date_taken,
                row.camera_make,
                row.camera_model,
                row.lens,
                row.iso,
                row.width.map(|v| v as i64),
                row.height.map(|v| v as i64),
                row.content_hash,
                row.phash.map(|h| h as i64),
            ],
        )?;
        self.conn.query_row(
            "SELECT id FROM media WHERE path = ?1",
            params![path_str(&row.path)],
            |r| r.get(0),
        )
    }

    pub fn set_content_hash(&self, id: i64, hash: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE media SET content_hash = ?2 WHERE id = ?1",
            params![id, hash],
        )?;
        Ok(())
    }

    /// Store a perceptual hash. Passing `None` records SQL NULL, i.e. "no
    /// perceptual hash available", which means the decode is retried on the next
    /// scan — acceptable because undecodable files are a small minority.
    pub fn set_phash(&self, id: i64, phash: Option<u64>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE media SET phash = ?2 WHERE id = ?1",
            params![id, phash.map(|h| h as i64)],
        )?;
        Ok(())
    }

    /// Record true pixel dimensions measured by decoding the image.
    ///
    /// More trustworthy than the EXIF tags, which are absent from PNGs and
    /// screenshots entirely and stale in any file edited by a tool that did not
    /// update them. Dimensions drive the "keep the highest resolution copy"
    /// suggestion, so getting them right matters.
    pub fn set_dimensions(&self, id: i64, width: u32, height: u32) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE media SET width = ?2, height = ?3 WHERE id = ?1",
            params![id, width as i64, height as i64],
        )?;
        Ok(())
    }

    /// Mark files under `root` that the latest walk did not see.
    pub fn mark_missing(&self, root: &Path, seen_ids: &[i64]) -> rusqlite::Result<usize> {
        // A temp table beats a giant `IN (...)` clause, which would blow past
        // SQLite's bound-parameter limit on a large library.
        self.conn
            .execute_batch("CREATE TEMP TABLE IF NOT EXISTS seen(id INTEGER PRIMARY KEY)")?;
        self.conn.execute("DELETE FROM seen", [])?;
        {
            let mut stmt = self
                .conn
                .prepare("INSERT OR IGNORE INTO seen(id) VALUES (?1)")?;
            for id in seen_ids {
                stmt.execute(params![id])?;
            }
        }
        self.conn.execute(
            "UPDATE media SET missing = 1
             WHERE path LIKE ?1 ESCAPE '\\' AND id NOT IN (SELECT id FROM seen)",
            params![like_prefix(root)],
        )
    }

    /// Drop rows for files that are gone.
    #[allow(dead_code)] // asserted by tests; not yet surfaced in the UI
    pub fn purge_missing(&self) -> rusqlite::Result<usize> {
        self.conn.execute("DELETE FROM media WHERE missing = 1", [])
    }

    /// Remove a single row by path (used after a file is quarantined).
    pub fn remove_path(&self, path: &Path) -> rusqlite::Result<usize> {
        self.conn
            .execute("DELETE FROM media WHERE path = ?1", params![path_str(path)])
    }

    /// Sizes shared by more than one present file. Only these files can possibly
    /// be byte-identical, so only these need hashing — on a typical library that
    /// is a small fraction of the whole.
    #[allow(dead_code)] // asserted by tests; the work list below is what runs
    pub fn duplicate_candidate_sizes(&self) -> rusqlite::Result<Vec<u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT size FROM media WHERE missing = 0
             GROUP BY size HAVING COUNT(*) > 1 ORDER BY size",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0).map(|v| v as u64))?;
        rows.collect()
    }

    /// Present rows that still lack a content hash *and* share a size with
    /// another file. This is the work list for the hashing pass.
    pub fn rows_needing_content_hash(&self) -> rusqlite::Result<Vec<(i64, PathBuf)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path FROM media
             WHERE missing = 0 AND content_hash IS NULL
               AND size IN (SELECT size FROM media WHERE missing = 0
                            GROUP BY size HAVING COUNT(*) > 1)
             ORDER BY path",
        )?;
        let rows = stmt.query_map([], read_id_path)?;
        rows.collect()
    }

    /// Present rows with no perceptual hash yet.
    pub fn rows_needing_phash(&self) -> rusqlite::Result<Vec<(i64, PathBuf)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path FROM media
             WHERE missing = 0 AND phash IS NULL ORDER BY path",
        )?;
        let rows = stmt.query_map([], read_id_path)?;
        rows.collect()
    }

    /// Every present row, across all indexed libraries.
    #[allow(dead_code)] // asserted by tests; the UI scans one library at a time
    pub fn all_present(&self) -> rusqlite::Result<Vec<MediaRow>> {
        let sql = format!("SELECT {MEDIA_COLUMNS} FROM media WHERE missing = 0 ORDER BY path");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], read_media_row)?;
        rows.collect()
    }

    /// Present rows under a specific library root.
    pub fn present_under(&self, root: &Path) -> rusqlite::Result<Vec<MediaRow>> {
        let sql = format!(
            "SELECT {MEDIA_COLUMNS} FROM media
             WHERE missing = 0 AND path LIKE ?1 ESCAPE '\\' ORDER BY path"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![like_prefix(root)], read_media_row)?;
        rows.collect()
    }

    /// Counts and total bytes for the present rows under `root`.
    pub fn stats_under(&self, root: &Path) -> rusqlite::Result<CatalogStats> {
        self.conn.query_row(
            "SELECT COUNT(*), COUNT(content_hash), COUNT(phash), COALESCE(SUM(size), 0)
             FROM media WHERE missing = 0 AND path LIKE ?1 ESCAPE '\\'",
            params![like_prefix(root)],
            |r| {
                Ok(CatalogStats {
                    files: r.get::<_, i64>(0)? as usize,
                    hashed: r.get::<_, i64>(1)? as usize,
                    phashed: r.get::<_, i64>(2)? as usize,
                    bytes: r.get::<_, i64>(3)? as u64,
                })
            },
        )
    }

    // ---- quarantine ledger ----

    /// Next batch number. A batch groups one "quarantine these" action so it can
    /// be undone as a unit.
    pub fn next_quarantine_batch(&self) -> rusqlite::Result<i64> {
        let max: Option<i64> = self.conn.query_row(
            "SELECT MAX(batch) FROM quarantine",
            [],
            |r| r.get::<_, Option<i64>>(0),
        )?;
        Ok(max.unwrap_or(0) + 1)
    }

    pub fn record_quarantine(
        &self,
        batch: i64,
        original: &Path,
        quarantined: &Path,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO quarantine (batch, original_path, quarantine_path, moved_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                batch,
                path_str(original),
                path_str(quarantined),
                chrono::Utc::now().timestamp(),
            ],
        )?;
        Ok(())
    }

    /// Un-restored entries for a batch, newest first.
    pub fn quarantine_batch(&self, batch: i64) -> rusqlite::Result<Vec<QuarantineEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, batch, original_path, quarantine_path FROM quarantine
             WHERE batch = ?1 AND restored = 0 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![batch], read_quarantine_entry)?;
        rows.collect()
    }

    /// The most recent batch that still has restorable entries.
    pub fn latest_restorable_batch(&self) -> rusqlite::Result<Option<i64>> {
        self.conn.query_row(
            "SELECT MAX(batch) FROM quarantine WHERE restored = 0",
            [],
            |r| r.get::<_, Option<i64>>(0),
        )
    }

    /// All un-restored entries, newest first — what the quarantine holds now.
    #[allow(dead_code)] // asserted by tests; the UI shows the folder size instead
    pub fn quarantine_pending(&self) -> rusqlite::Result<Vec<QuarantineEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, batch, original_path, quarantine_path FROM quarantine
             WHERE restored = 0 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], read_quarantine_entry)?;
        rows.collect()
    }

    pub fn mark_restored(&self, entry_id: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE quarantine SET restored = 1 WHERE id = ?1",
            params![entry_id],
        )?;
        Ok(())
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// Build a `LIKE` pattern matching everything beneath `root`.
///
/// Two subtleties, both of which bite on real photo libraries:
///
/// * `%` and `_` are `LIKE` wildcards, and `_` is extremely common in camera
///   filenames (`IMG_1234`). They are escaped, and every query using this
///   pattern declares `ESCAPE '\'`.
/// * The pattern is anchored with a trailing separator so that indexing
///   `/photos/lib` does not also match `/photos/library`.
fn like_prefix(root: &Path) -> String {
    let mut s = path_str(root);
    if !s.ends_with(std::path::MAIN_SEPARATOR) {
        s.push(std::path::MAIN_SEPARATOR);
    }
    let mut escaped = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped.push('%');
    escaped
}

fn read_id_path(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, PathBuf)> {
    Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?)))
}

fn read_media_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MediaRow> {
    Ok(MediaRow {
        id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        file_name: r.get(2)?,
        ext: r.get(3)?,
        size: r.get::<_, i64>(4)? as u64,
        mtime: r.get(5)?,
        date_taken: r.get(6)?,
        camera_make: r.get(7)?,
        camera_model: r.get(8)?,
        lens: r.get(9)?,
        iso: r.get(10)?,
        width: r.get::<_, Option<i64>>(11)?.map(|v| v as u32),
        height: r.get::<_, Option<i64>>(12)?.map(|v| v as u32),
        content_hash: r.get(13)?,
        phash: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        missing: r.get::<_, i64>(15)? != 0,
    })
}

fn read_quarantine_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<QuarantineEntry> {
    Ok(QuarantineEntry {
        id: r.get(0)?,
        batch: r.get(1)?,
        original_path: PathBuf::from(r.get::<_, String>(2)?),
        quarantine_path: PathBuf::from(r.get::<_, String>(3)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, size: u64, mtime: i64) -> MediaRow {
        let p = PathBuf::from(path);
        MediaRow {
            file_name: p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            ext: p
                .extension()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: p,
            size,
            mtime,
            ..MediaRow::default()
        }
    }

    fn paths_of(rows: &[MediaRow]) -> Vec<String> {
        rows.iter().map(|r| path_str(&r.path)).collect()
    }

    #[test]
    fn schema_initializes_at_current_version() {
        let cat = Catalog::open_in_memory().unwrap();
        assert_eq!(cat.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn upsert_is_idempotent_by_path() {
        let cat = Catalog::open_in_memory().unwrap();
        let r = row("/lib/a.jpg", 100, 10);
        let id1 = cat.upsert_media(&r).unwrap();
        let id2 = cat.upsert_media(&r).unwrap();
        assert_eq!(id1, id2);
        assert_eq!(cat.all_present().unwrap().len(), 1);
    }

    #[test]
    fn unchanged_file_keeps_cached_hashes_but_changed_file_loses_them() {
        let cat = Catalog::open_in_memory().unwrap();
        let r = row("/lib/a.jpg", 100, 10);
        let id = cat.upsert_media(&r).unwrap();
        cat.set_content_hash(id, "deadbeef").unwrap();
        cat.set_phash(id, Some(0xABCD)).unwrap();

        // Re-walking an unchanged file must not throw away the expensive work —
        // this is the whole point of the catalog.
        cat.upsert_media(&r).unwrap();
        let got = &cat.all_present().unwrap()[0];
        assert_eq!(got.content_hash.as_deref(), Some("deadbeef"));
        assert_eq!(got.phash, Some(0xABCD));

        // A changed mtime means the bytes may differ, so the hashes must go.
        cat.upsert_media(&row("/lib/a.jpg", 100, 11)).unwrap();
        let got = &cat.all_present().unwrap()[0];
        assert_eq!(got.content_hash, None);
        assert_eq!(got.phash, None);
    }

    #[test]
    fn phash_roundtrips_high_bit_values() {
        // dHash uses the full 64-bit range but SQLite integers are signed, so
        // the top bit must survive the round trip.
        let cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/a.jpg", 1, 1)).unwrap();
        for probe in [u64::MAX, u64::MAX - 3, 1 << 63, 0] {
            cat.set_phash(id, Some(probe)).unwrap();
            assert_eq!(cat.all_present().unwrap()[0].phash, Some(probe));
        }
    }

    #[test]
    fn dimensions_roundtrip() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut r = row("/lib/a.jpg", 1, 1);
        r.width = Some(6000);
        r.height = Some(4000);
        cat.upsert_media(&r).unwrap();
        let got = &cat.all_present().unwrap()[0];
        assert_eq!((got.width, got.height), (Some(6000), Some(4000)));
    }

    #[test]
    fn stamps_under_scopes_to_root() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/a.jpg", 1, 1)).unwrap();
        cat.upsert_media(&row("/lib/sub/b.jpg", 2, 2)).unwrap();
        cat.upsert_media(&row("/other/c.jpg", 3, 3)).unwrap();

        let stamps = cat.stamps_under(Path::new("/lib")).unwrap();
        assert_eq!(stamps.len(), 2);
        let a = stamps.get(Path::new("/lib/a.jpg")).unwrap();
        assert_eq!((a.size, a.mtime), (1, 1));
        assert!(!a.has_content_hash && !a.has_phash);
    }

    #[test]
    fn root_prefix_does_not_leak_into_sibling_folders() {
        // "/photos/lib" must not match "/photos/library".
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/photos/lib/a.jpg", 1, 1)).unwrap();
        cat.upsert_media(&row("/photos/library/b.jpg", 2, 2))
            .unwrap();

        let rows = cat.present_under(Path::new("/photos/lib")).unwrap();
        assert_eq!(paths_of(&rows), vec!["/photos/lib/a.jpg"]);
        assert_eq!(cat.stats_under(Path::new("/photos/lib")).unwrap().files, 1);
    }

    #[test]
    fn like_wildcards_in_paths_are_escaped() {
        // `_` and `%` are LIKE wildcards, and `_` is everywhere in camera
        // filenames. An unescaped pattern would pull in unrelated folders.
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib_a/keep.jpg", 1, 1)).unwrap();
        cat.upsert_media(&row("/libXa/other.jpg", 2, 2)).unwrap();
        cat.upsert_media(&row("/100%/pct.jpg", 3, 3)).unwrap();
        cat.upsert_media(&row("/100X/notpct.jpg", 4, 4)).unwrap();

        let rows = cat.present_under(Path::new("/lib_a")).unwrap();
        assert_eq!(paths_of(&rows), vec!["/lib_a/keep.jpg"]);

        let rows = cat.present_under(Path::new("/100%")).unwrap();
        assert_eq!(paths_of(&rows), vec!["/100%/pct.jpg"]);
    }

    #[test]
    fn mark_missing_respects_escaped_root_scope() {
        let cat = Catalog::open_in_memory().unwrap();
        let keep = cat.upsert_media(&row("/lib_a/keep.jpg", 1, 1)).unwrap();
        cat.upsert_media(&row("/lib_a/gone.jpg", 2, 2)).unwrap();
        cat.upsert_media(&row("/libXa/untouched.jpg", 3, 3)).unwrap();

        let flagged = cat.mark_missing(Path::new("/lib_a"), &[keep]).unwrap();
        assert_eq!(flagged, 1, "only gone.jpg is missing");
        assert_eq!(
            paths_of(&cat.all_present().unwrap()),
            vec!["/libXa/untouched.jpg", "/lib_a/keep.jpg"]
        );
    }

    #[test]
    fn only_size_collisions_need_hashing() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.upsert_media(&row("/lib/b.jpg", 100, 2)).unwrap();
        cat.upsert_media(&row("/lib/c.jpg", 999, 3)).unwrap();

        assert_eq!(cat.duplicate_candidate_sizes().unwrap(), vec![100]);
        let names: Vec<String> = cat
            .rows_needing_content_hash()
            .unwrap()
            .iter()
            .map(|(_, p)| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        // c.jpg has a unique size, so it cannot be a byte-duplicate of anything.
        assert_eq!(names, vec!["a.jpg", "b.jpg"]);
    }

    #[test]
    fn hashed_rows_drop_out_of_the_work_list() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.upsert_media(&row("/lib/b.jpg", 100, 2)).unwrap();
        cat.set_content_hash(a, "aa").unwrap();
        assert_eq!(cat.rows_needing_content_hash().unwrap().len(), 1);

        assert_eq!(cat.rows_needing_phash().unwrap().len(), 2);
        cat.set_phash(a, Some(1)).unwrap();
        assert_eq!(cat.rows_needing_phash().unwrap().len(), 1);
    }

    #[test]
    fn mark_missing_then_purge() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.upsert_media(&row("/lib/a.jpg", 1, 1)).unwrap();
        cat.upsert_media(&row("/lib/b.jpg", 2, 2)).unwrap();
        cat.upsert_media(&row("/other/c.jpg", 3, 3)).unwrap();

        // Only a.jpg was seen; b.jpg is gone; c.jpg is out of scope entirely.
        assert_eq!(cat.mark_missing(Path::new("/lib"), &[a]).unwrap(), 1);
        assert_eq!(
            paths_of(&cat.all_present().unwrap()),
            vec!["/lib/a.jpg", "/other/c.jpg"]
        );
        assert_eq!(cat.purge_missing().unwrap(), 1);
        assert_eq!(cat.all_present().unwrap().len(), 2);
    }

    #[test]
    fn reappearing_file_clears_the_missing_flag() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.upsert_media(&row("/lib/a.jpg", 1, 1)).unwrap();
        cat.upsert_media(&row("/lib/b.jpg", 2, 2)).unwrap();
        cat.mark_missing(Path::new("/lib"), &[a]).unwrap();
        assert_eq!(cat.all_present().unwrap().len(), 1);

        // Re-indexing after the file comes back must un-hide it.
        cat.upsert_media(&row("/lib/b.jpg", 2, 2)).unwrap();
        assert_eq!(cat.all_present().unwrap().len(), 2);
    }

    #[test]
    fn stats_are_scoped_and_count_computed_fields() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.upsert_media(&row("/lib/b.jpg", 250, 2)).unwrap();
        cat.upsert_media(&row("/other/c.jpg", 999, 3)).unwrap();
        cat.set_content_hash(a, "aa").unwrap();
        cat.set_phash(a, Some(7)).unwrap();

        assert_eq!(
            cat.stats_under(Path::new("/lib")).unwrap(),
            CatalogStats {
                files: 2,
                hashed: 1,
                phashed: 1,
                bytes: 350,
            }
        );
    }

    #[test]
    fn quarantine_ledger_batches_and_restores() {
        let cat = Catalog::open_in_memory().unwrap();
        assert_eq!(cat.next_quarantine_batch().unwrap(), 1);
        assert_eq!(cat.latest_restorable_batch().unwrap(), None);

        cat.record_quarantine(1, Path::new("/lib/dup1.jpg"), Path::new("/q/dup1.jpg"))
            .unwrap();
        cat.record_quarantine(1, Path::new("/lib/dup2.jpg"), Path::new("/q/dup2.jpg"))
            .unwrap();
        assert_eq!(cat.next_quarantine_batch().unwrap(), 2);
        assert_eq!(cat.latest_restorable_batch().unwrap(), Some(1));

        let batch = cat.quarantine_batch(1).unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(cat.quarantine_pending().unwrap().len(), 2);
        assert_eq!(batch[0].original_path, PathBuf::from("/lib/dup2.jpg"));

        for e in &batch {
            cat.mark_restored(e.id).unwrap();
        }
        assert!(cat.quarantine_batch(1).unwrap().is_empty());
        assert!(cat.quarantine_pending().unwrap().is_empty());
        assert_eq!(cat.latest_restorable_batch().unwrap(), None);
        // Batch numbers keep advancing so a restored batch is never reused.
        assert_eq!(cat.next_quarantine_batch().unwrap(), 2);
    }

    #[test]
    fn remove_path_drops_the_row() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/a.jpg", 1, 1)).unwrap();
        assert_eq!(cat.remove_path(Path::new("/lib/a.jpg")).unwrap(), 1);
        assert!(cat.all_present().unwrap().is_empty());
    }

    #[test]
    fn like_prefix_escapes_and_anchors() {
        assert_eq!(like_prefix(Path::new("/a/b")), "/a/b/%");
        assert_eq!(like_prefix(Path::new("/a/b/")), "/a/b/%");
        assert_eq!(like_prefix(Path::new("/lib_a")), "/lib\\_a/%");
        assert_eq!(like_prefix(Path::new("/100%")), "/100\\%/%");
    }
}
