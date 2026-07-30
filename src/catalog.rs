//! Persistent SQLite catalog of indexed media.
//!
//! The catalog exists so that opening a large library is instant and a rescan
//! only touches files that actually changed. Every expensive thing the indexer
//! computes — EXIF, content hash, perceptual hash, dominant colour — is cached
//! here keyed by `(path, size, mtime)`, so re-indexing an unchanged 80k-photo
//! folder costs a directory walk and nothing else.
//!
//! Two tables, and the split between them matters:
//!
//! * `media` is **derived** state. Every column can be recomputed from the file,
//!   so losing a row costs time, not information. Keyed on path.
//! * `user_meta` is **irreplaceable** state — things only the user can supply,
//!   like whether an image is hidden. Keyed on *content hash*, because a renamed
//!   or moved file gets a new `media` row but keeps its bytes. Nothing the user
//!   typed may depend on a path staying put.
//!
//! The database file lives in the platform data directory (see [`crate::paths`]),
//! never inside the library being organized.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

/// Current schema version. Bump when altering the schema and add a migration
/// step in [`Catalog::migrate`].
const SCHEMA_VERSION: i64 = 4;

/// Version of the metadata the walk pass extracts.
///
/// Bump this whenever the indexer starts reading a *new* field from a file's own
/// metadata. Rows stamped with an older version are re-examined on the next
/// index even when their size and mtime are unchanged, which is the only way a
/// newly-added field ever reaches files that were catalogued before it existed.
///
/// Without it, adding GPS extraction would have silently applied to new and
/// changed files only, leaving an already-indexed library permanently without
/// coordinates and no obvious reason why.
///
/// * 1 — original: name, size, dates, camera, dimensions
/// * 2 — added GPS latitude/longitude
pub const META_VERSION: i64 = 2;

/// Columns selected by the `present_*` queries, in the order [`read_media_row`]
/// expects. Kept in one place so the call sites cannot drift apart.
const MEDIA_COLUMNS: &str = "m.id, m.path, m.file_name, m.ext, m.size, m.mtime, m.date_taken, \
     m.camera_make, m.camera_model, m.lens, m.iso, m.width, m.height, m.content_hash, \
     m.phash, m.missing, m.dom_color, COALESCE(u.hidden, 0), COALESCE(u.rating, 0), u.kind, \
     m.lat, m.lon";

/// Whether a query should include images the user has hidden.
///
/// Every query that returns media rows takes one of these, so there is no way to
/// read rows without deciding. That is deliberate: if each view filtered for
/// itself, one of them would eventually forget, and the failure mode of the
/// parent filter is showing the exact thing you meant to hide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Everything, including hidden images. For indexing and for managing which
    /// images are hidden in the first place.
    All,
    /// Hidden images excluded. What every browsing, slideshow and duplicate view
    /// uses while the parent filter is on.
    VisibleOnly,
}

impl Visibility {
    /// Bound as a SQL parameter: the queries read `(?n = 0 OR hidden = 0)`.
    fn as_param(self) -> i64 {
        match self {
            Visibility::All => 0,
            Visibility::VisibleOnly => 1,
        }
    }
}

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
    /// Dominant colour as packed `0xRRGGBB`, from the decode pass. `None` when
    /// the file could not be decoded.
    pub dom_color: Option<u32>,
    /// Hidden by the user (the parent filter). Read-only here: it lives in
    /// `user_meta`, keyed on content hash, and is joined in.
    pub hidden: bool,
    /// 0–5 stars; 0 means unrated. Joined from `user_meta`.
    pub rating: u8,
    /// User's override of the guessed media kind, if any. `None` means "trust
    /// the guess" — see [`crate::kind::effective`].
    pub kind_override: Option<crate::kind::MediaKind>,
    /// Geotag in decimal degrees, south and west negative. Set as a pair.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

impl MediaRow {
    /// Coordinates, when the photo has a complete geotag.
    pub fn coords(&self) -> Option<(f64, f64)> {
        self.lat.zip(self.lon)
    }
}

/// The minimum needed to decide whether a file must be re-examined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub id: i64,
    pub size: u64,
    pub mtime: i64,
    pub has_content_hash: bool,
    pub has_phash: bool,
    /// Which generation of metadata extraction produced this row. A row behind
    /// [`META_VERSION`] is re-examined even if the file is unchanged.
    pub meta_version: i64,
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

    /// Pretend every row was written by an older build, so the indexer's
    /// backfill path can be exercised without hand-rolling a stale database.
    #[cfg(test)]
    pub fn mark_meta_stale_for_tests(&self) {
        self.conn
            .execute("UPDATE media SET meta_version = 0", [])
            .unwrap();
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

            -- Metadata the user supplies, which cannot be recomputed from the
            -- file and so must outlive renames, moves and re-imports.
            --
            -- Keyed on content hash rather than path or row id precisely for
            -- that reason: the media row for a renamed file is dropped and
            -- recreated by the next index, but the bytes are the same, so the
            -- hash still finds it. Two byte-identical files therefore share a
            -- row here, which is the desired behaviour — they are the same
            -- picture.
            CREATE TABLE IF NOT EXISTS user_meta (
                content_hash TEXT PRIMARY KEY,
                hidden       INTEGER NOT NULL DEFAULT 0,
                rating       INTEGER NOT NULL DEFAULT 0,
                kind         TEXT,
                updated_at   INTEGER NOT NULL
            );
            "#,
        )?;

        // v1 -> v2: dominant colour, computed in the decode pass. Added in place
        // so an existing catalog keeps its hashes and thumbnails instead of
        // forcing a full re-index.
        self.add_column_if_missing("media", "dom_color", "INTEGER")?;

        // v2 -> v3: star rating and media-kind override. Both are user-assigned,
        // so they join `hidden` in `user_meta` keyed on content hash.
        self.add_column_if_missing("user_meta", "rating", "INTEGER NOT NULL DEFAULT 0")?;
        self.add_column_if_missing("user_meta", "kind", "TEXT")?;

        // v3 -> v4: geotag, plus the metadata-generation stamp that lets a
        // newly-extracted field reach already-indexed files. Existing rows
        // default to version 0, so they are all below META_VERSION and get
        // re-examined once.
        self.add_column_if_missing("media", "lat", "REAL")?;
        self.add_column_if_missing("media", "lon", "REAL")?;
        self.add_column_if_missing("media", "meta_version", "INTEGER NOT NULL DEFAULT 0")?;
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS media_geo_idx ON media(lat, lon);",
        )?;

        self.conn.execute(
            "INSERT INTO schema_meta(key, value) VALUES('version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    /// `ALTER TABLE ... ADD COLUMN`, but idempotent. SQLite has no
    /// `ADD COLUMN IF NOT EXISTS`, so the column list is checked first.
    fn add_column_if_missing(&self, table: &str, column: &str, ty: &str) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        if !existing.iter().any(|c| c == column) {
            self.conn
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
        }
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
            "SELECT id, path, size, mtime, content_hash IS NOT NULL, phash IS NOT NULL,
                    meta_version
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
                    meta_version: r.get(6)?,
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
                                content_hash, phash, dom_color, lat, lon,
                                meta_version, missing)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,0)
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
                dom_color = CASE
                    WHEN media.size = excluded.size AND media.mtime = excluded.mtime
                    THEN media.dom_color ELSE excluded.dom_color END,
                -- Geotag comes from the same EXIF read as the rest of this
                -- statement's values, so it is always current.
                lat = excluded.lat,
                lon = excluded.lon,
                meta_version = excluded.meta_version,
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
                row.dom_color.map(|c| c as i64),
                row.lat,
                row.lon,
                META_VERSION,
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

    /// Store everything one decode produced: perceptual hash and dominant
    /// colour. `None` records SQL NULL, i.e. "not available", which means the
    /// decode is retried on the next scan — acceptable because undecodable files
    /// are a small minority.
    pub fn set_analysis(
        &self,
        id: i64,
        phash: Option<u64>,
        dom_color: Option<u32>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE media SET phash = ?2, dom_color = ?3 WHERE id = ?1",
            params![id, phash.map(|h| h as i64), dom_color.map(|c| c as i64)],
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

    /// Follow files the app itself moved, keeping their catalog rows intact.
    ///
    /// Without this a rename looks like "one file vanished, another appeared":
    /// the next index drops the old row and inserts a new one, throwing away the
    /// content hash, perceptual hash and dominant colour, and forcing a re-decode
    /// of a file whose bytes never changed.
    ///
    /// User-assigned metadata survives regardless, since `user_meta` is keyed on
    /// content hash — but only files that have been hashed have one, so keeping
    /// the row is still the first line of defence.
    ///
    /// Runs as one transaction: a half-applied remap would leave the catalog
    /// describing a directory layout that never existed.
    pub fn apply_moves(&mut self, moves: &[(PathBuf, PathBuf)]) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut updated = 0;
        for (from, to) in moves {
            let file_name = to
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let ext = to
                .extension()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            // The destination may already have a stale row (e.g. a file that was
            // there before an earlier scan). `path` is UNIQUE, so clear it first
            // or the update below fails.
            tx.execute(
                "DELETE FROM media WHERE path = ?1 AND path <> ?2",
                params![path_str(to), path_str(from)],
            )?;
            updated += tx.execute(
                "UPDATE media SET path = ?2, file_name = ?3, ext = ?4, missing = 0
                 WHERE path = ?1",
                params![path_str(from), path_str(to), file_name, ext],
            )?;
        }
        tx.commit()?;
        Ok(updated)
    }

    // ---- user-assigned metadata ----

    /// Set one user-assigned field, leaving the others alone.
    ///
    /// `column` is only ever passed an internal constant, never anything from
    /// the user, so interpolating it into the statement is safe. The values
    /// themselves stay bound parameters.
    fn set_user_meta<T: rusqlite::ToSql>(
        &self,
        content_hash: &str,
        column: &str,
        value: T,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO user_meta (content_hash, {column}, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(content_hash) DO UPDATE SET {column} = ?2, updated_at = ?3"
            ),
            params![content_hash, value, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    }

    /// Hide or unhide the image with this content hash.
    pub fn set_hidden(&self, content_hash: &str, hidden: bool) -> rusqlite::Result<()> {
        self.set_user_meta(content_hash, "hidden", hidden as i64)
    }

    /// Set a 0–5 star rating; 0 clears it. Values outside the range are clamped
    /// rather than rejected, so a caller bug cannot poison the database.
    pub fn set_rating(&self, content_hash: &str, rating: u8) -> rusqlite::Result<()> {
        self.set_user_meta(content_hash, "rating", rating.min(5) as i64)
    }

    /// Override the guessed media kind, or pass `None` to go back to the guess.
    pub fn set_kind(
        &self,
        content_hash: &str,
        kind: Option<crate::kind::MediaKind>,
    ) -> rusqlite::Result<()> {
        self.set_user_meta(content_hash, "kind", kind.map(|k| k.as_str()))
    }

    /// Rating stored against this content hash, regardless of whether any file
    /// currently has those bytes.
    #[allow(dead_code)] // asserted by tests; views read the joined column
    pub fn rating_of(&self, content_hash: &str) -> rusqlite::Result<u8> {
        Ok(self
            .conn
            .query_row(
                "SELECT rating FROM user_meta WHERE content_hash = ?1",
                params![content_hash],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            .clamp(0, 5) as u8)
    }

    /// Whether this content hash is hidden.
    #[allow(dead_code)] // asserted by tests; views read the joined column
    pub fn is_hidden(&self, content_hash: &str) -> rusqlite::Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT hidden FROM user_meta WHERE content_hash = ?1",
                params![content_hash],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0)
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

    /// Present rows still missing anything the decode pass produces.
    ///
    /// Covers the perceptual hash *or* the dominant colour, so a catalog built
    /// before colour extraction existed picks up the new field on its next index
    /// without needing a full rebuild.
    pub fn rows_needing_analysis(&self) -> rusqlite::Result<Vec<(i64, PathBuf)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path FROM media
             WHERE missing = 0 AND (phash IS NULL OR dom_color IS NULL) ORDER BY path",
        )?;
        let rows = stmt.query_map([], read_id_path)?;
        rows.collect()
    }

    /// Every present row, across all indexed libraries.
    #[allow(dead_code)] // asserted by tests; the UI scans one library at a time
    pub fn all_present(&self, visibility: Visibility) -> rusqlite::Result<Vec<MediaRow>> {
        let sql = format!(
            "SELECT {MEDIA_COLUMNS} FROM media m
             LEFT JOIN user_meta u ON u.content_hash = m.content_hash
             WHERE m.missing = 0 AND (?1 = 0 OR COALESCE(u.hidden, 0) = 0)
             ORDER BY m.path"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![visibility.as_param()], read_media_row)?;
        rows.collect()
    }

    /// Present rows under a specific library root.
    ///
    /// This is the single gate every browsing view goes through, so honouring
    /// `visibility` here is what makes the parent filter app-wide rather than a
    /// property each view has to remember.
    pub fn present_under(
        &self,
        root: &Path,
        visibility: Visibility,
    ) -> rusqlite::Result<Vec<MediaRow>> {
        let sql = format!(
            "SELECT {MEDIA_COLUMNS} FROM media m
             LEFT JOIN user_meta u ON u.content_hash = m.content_hash
             WHERE m.missing = 0 AND m.path LIKE ?1 ESCAPE '\\'
               AND (?2 = 0 OR COALESCE(u.hidden, 0) = 0)
             ORDER BY m.path"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![like_prefix(root), visibility.as_param()],
            read_media_row,
        )?;
        rows.collect()
    }

    /// Number of hidden images under `root`, so the UI can say how many the
    /// filter is holding back.
    pub fn hidden_count_under(&self, root: &Path) -> rusqlite::Result<usize> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM media m
             JOIN user_meta u ON u.content_hash = m.content_hash
             WHERE m.missing = 0 AND m.path LIKE ?1 ESCAPE '\\' AND u.hidden = 1",
            params![like_prefix(root)],
            |r| r.get::<_, i64>(0).map(|n| n as usize),
        )
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
        dom_color: r.get::<_, Option<i64>>(16)?.map(|v| v as u32),
        hidden: r.get::<_, i64>(17)? != 0,
        rating: r.get::<_, i64>(18)?.clamp(0, 5) as u8,
        kind_override: r
            .get::<_, Option<String>>(19)?
            .as_deref()
            .and_then(crate::kind::MediaKind::parse),
        lat: r.get(20)?,
        lon: r.get(21)?,
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
        assert_eq!(cat.all_present(Visibility::All).unwrap().len(), 1);
    }

    #[test]
    fn unchanged_file_keeps_cached_hashes_but_changed_file_loses_them() {
        let cat = Catalog::open_in_memory().unwrap();
        let r = row("/lib/a.jpg", 100, 10);
        let id = cat.upsert_media(&r).unwrap();
        cat.set_content_hash(id, "deadbeef").unwrap();
        cat.set_analysis(id, Some(0xABCD), None).unwrap();

        // Re-walking an unchanged file must not throw away the expensive work —
        // this is the whole point of the catalog.
        cat.upsert_media(&r).unwrap();
        let got = &cat.all_present(Visibility::All).unwrap()[0];
        assert_eq!(got.content_hash.as_deref(), Some("deadbeef"));
        assert_eq!(got.phash, Some(0xABCD));

        // A changed mtime means the bytes may differ, so the hashes must go.
        cat.upsert_media(&row("/lib/a.jpg", 100, 11)).unwrap();
        let got = &cat.all_present(Visibility::All).unwrap()[0];
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
            cat.set_analysis(id, Some(probe), None).unwrap();
            assert_eq!(cat.all_present(Visibility::All).unwrap()[0].phash, Some(probe));
        }
    }

    #[test]
    fn dimensions_roundtrip() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut r = row("/lib/a.jpg", 1, 1);
        r.width = Some(6000);
        r.height = Some(4000);
        cat.upsert_media(&r).unwrap();
        let got = &cat.all_present(Visibility::All).unwrap()[0];
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

        let rows = cat.present_under(Path::new("/photos/lib"), Visibility::All).unwrap();
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

        let rows = cat.present_under(Path::new("/lib_a"), Visibility::All).unwrap();
        assert_eq!(paths_of(&rows), vec!["/lib_a/keep.jpg"]);

        let rows = cat.present_under(Path::new("/100%"), Visibility::All).unwrap();
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
            paths_of(&cat.all_present(Visibility::All).unwrap()),
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

        assert_eq!(cat.rows_needing_analysis().unwrap().len(), 2);
        // A row only leaves the analysis work list once the decode pass has
        // produced *everything*: a perceptual hash alone still leaves the
        // dominant colour outstanding.
        cat.set_analysis(a, Some(1), None).unwrap();
        assert_eq!(
            cat.rows_needing_analysis().unwrap().len(),
            2,
            "still missing a colour"
        );
        cat.set_analysis(a, Some(1), Some(0x102030)).unwrap();
        assert_eq!(cat.rows_needing_analysis().unwrap().len(), 1);
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
            paths_of(&cat.all_present(Visibility::All).unwrap()),
            vec!["/lib/a.jpg", "/other/c.jpg"]
        );
        assert_eq!(cat.purge_missing().unwrap(), 1);
        assert_eq!(cat.all_present(Visibility::All).unwrap().len(), 2);
    }

    #[test]
    fn reappearing_file_clears_the_missing_flag() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.upsert_media(&row("/lib/a.jpg", 1, 1)).unwrap();
        cat.upsert_media(&row("/lib/b.jpg", 2, 2)).unwrap();
        cat.mark_missing(Path::new("/lib"), &[a]).unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap().len(), 1);

        // Re-indexing after the file comes back must un-hide it.
        cat.upsert_media(&row("/lib/b.jpg", 2, 2)).unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap().len(), 2);
    }

    #[test]
    fn stats_are_scoped_and_count_computed_fields() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.upsert_media(&row("/lib/b.jpg", 250, 2)).unwrap();
        cat.upsert_media(&row("/other/c.jpg", 999, 3)).unwrap();
        cat.set_content_hash(a, "aa").unwrap();
        cat.set_analysis(a, Some(7), None).unwrap();

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
        assert!(cat.all_present(Visibility::All).unwrap().is_empty());
    }

    #[test]
    fn moving_a_file_keeps_its_row_and_everything_expensive_on_it() {
        // The whole point of apply_moves: a rename must not look like a delete
        // plus an insert, or the next index throws away work already done.
        let mut cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/IMG_1.jpg", 100, 5)).unwrap();
        cat.set_content_hash(id, "abc123").unwrap();
        cat.set_analysis(id, Some(0xFEED), Some(0x00FF00)).unwrap();

        let moved = cat
            .apply_moves(&[(
                PathBuf::from("/lib/IMG_1.jpg"),
                PathBuf::from("/lib/2019/holiday.jpg"),
            )])
            .unwrap();
        assert_eq!(moved, 1);

        let rows = cat.all_present(Visibility::All).unwrap();
        assert_eq!(rows.len(), 1, "one row, not two");
        assert_eq!(rows[0].id, id, "same row, not a replacement");
        assert_eq!(rows[0].path, PathBuf::from("/lib/2019/holiday.jpg"));
        assert_eq!(rows[0].file_name, "holiday.jpg");
        assert_eq!(rows[0].ext, "jpg");
        assert_eq!(rows[0].content_hash.as_deref(), Some("abc123"));
        assert_eq!(rows[0].phash, Some(0xFEED));
        assert_eq!(rows[0].dom_color, Some(0x00FF00));
    }

    #[test]
    fn hidden_flag_survives_a_rename() {
        // The durability guarantee that matters: user-assigned metadata is keyed
        // on content hash, so it follows the bytes even if the row were rebuilt.
        let mut cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/private.jpg", 100, 5)).unwrap();
        cat.set_content_hash(id, "hash-of-bytes").unwrap();
        cat.set_hidden("hash-of-bytes", true).unwrap();
        assert!(cat.is_hidden("hash-of-bytes").unwrap());

        cat.apply_moves(&[(
            PathBuf::from("/lib/private.jpg"),
            PathBuf::from("/lib/sorted/2019_001.jpg"),
        )])
        .unwrap();

        let visible = cat.all_present(Visibility::VisibleOnly).unwrap();
        assert!(visible.is_empty(), "still hidden after the rename");
        let all = cat.all_present(Visibility::All).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].hidden);
    }

    #[test]
    fn hidden_flag_survives_even_a_full_row_rebuild() {
        // Simulates a move made outside the app: the row is dropped and
        // recreated by a later index. Keying on content hash is what saves it.
        let cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/a.jpg", 100, 5)).unwrap();
        cat.set_content_hash(id, "same-bytes").unwrap();
        cat.set_hidden("same-bytes", true).unwrap();

        // A second row keeps SQLite's rowid counter above `id`. Without it,
        // deleting the highest rowid frees it for reuse and the "new" row comes
        // back with the same id, which would make the assertion below vacuous.
        cat.upsert_media(&row("/lib/zz-other.jpg", 7, 7)).unwrap();

        cat.remove_path(Path::new("/lib/a.jpg")).unwrap();
        let new_id = cat
            .upsert_media(&MediaRow {
                content_hash: Some("same-bytes".into()),
                ..row("/elsewhere/renamed.jpg", 100, 9)
            })
            .unwrap();
        assert_ne!(new_id, id, "genuinely a new row, not an update");

        // The rebuilt row is still hidden: the flag followed the bytes, not the
        // path or the row id.
        let visible = cat.all_present(Visibility::VisibleOnly).unwrap();
        assert_eq!(paths_of(&visible), vec!["/lib/zz-other.jpg"]);
        let all = cat.all_present(Visibility::All).unwrap();
        let rebuilt = all.iter().find(|r| r.id == new_id).unwrap();
        assert!(rebuilt.hidden);
    }

    #[test]
    fn apply_moves_clears_a_stale_row_at_the_destination() {
        // `path` is UNIQUE, so a leftover row where the file is going would
        // otherwise make the update fail.
        let mut cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.upsert_media(&row("/lib/b.jpg", 200, 2)).unwrap();

        let moved = cat
            .apply_moves(&[(PathBuf::from("/lib/a.jpg"), PathBuf::from("/lib/b.jpg"))])
            .unwrap();
        assert_eq!(moved, 1);
        let rows = cat.all_present(Visibility::All).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, PathBuf::from("/lib/b.jpg"));
        assert_eq!(rows[0].size, 100, "the moved file's row won, not the stale one");
    }

    #[test]
    fn apply_moves_ignores_paths_it_does_not_know() {
        let mut cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        // A rename in a folder that was never indexed is the normal case.
        let moved = cat
            .apply_moves(&[(
                PathBuf::from("/somewhere/else.jpg"),
                PathBuf::from("/somewhere/new.jpg"),
            )])
            .unwrap();
        assert_eq!(moved, 0);
        assert_eq!(cat.all_present(Visibility::All).unwrap().len(), 1);
    }

    #[test]
    fn visibility_gates_every_row_query() {
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.upsert_media(&row("/lib/keep.jpg", 100, 1)).unwrap();
        let b = cat.upsert_media(&row("/lib/private.jpg", 200, 2)).unwrap();
        cat.set_content_hash(a, "h-keep").unwrap();
        cat.set_content_hash(b, "h-private").unwrap();
        cat.set_hidden("h-private", true).unwrap();

        for rows in [
            cat.present_under(Path::new("/lib"), Visibility::VisibleOnly)
                .unwrap(),
            cat.all_present(Visibility::VisibleOnly).unwrap(),
        ] {
            assert_eq!(paths_of(&rows), vec!["/lib/keep.jpg"]);
        }
        for rows in [
            cat.present_under(Path::new("/lib"), Visibility::All).unwrap(),
            cat.all_present(Visibility::All).unwrap(),
        ] {
            assert_eq!(rows.len(), 2);
        }
        assert_eq!(cat.hidden_count_under(Path::new("/lib")).unwrap(), 1);
    }

    #[test]
    fn unhiding_restores_visibility() {
        let cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.set_content_hash(id, "h").unwrap();
        cat.set_hidden("h", true).unwrap();
        assert!(cat.all_present(Visibility::VisibleOnly).unwrap().is_empty());

        cat.set_hidden("h", false).unwrap();
        assert_eq!(cat.all_present(Visibility::VisibleOnly).unwrap().len(), 1);
        assert!(!cat.is_hidden("h").unwrap());
        assert_eq!(cat.hidden_count_under(Path::new("/lib")).unwrap(), 0);
    }

    #[test]
    fn identical_files_share_user_metadata() {
        // Byte-identical files are the same picture, so hiding one hides both.
        let cat = Catalog::open_in_memory().unwrap();
        let a = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        let b = cat.upsert_media(&row("/lib/copy/a.jpg", 100, 2)).unwrap();
        cat.set_content_hash(a, "identical").unwrap();
        cat.set_content_hash(b, "identical").unwrap();
        cat.set_hidden("identical", true).unwrap();

        assert!(cat.all_present(Visibility::VisibleOnly).unwrap().is_empty());
        assert_eq!(cat.hidden_count_under(Path::new("/lib")).unwrap(), 2);
    }

    #[test]
    fn unhashed_rows_are_never_hidden_by_accident() {
        // Rows with no content hash must not join against a NULL and vanish.
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/unhashed.jpg", 100, 1)).unwrap();
        let b = cat.upsert_media(&row("/lib/hidden.jpg", 200, 2)).unwrap();
        cat.set_content_hash(b, "h").unwrap();
        cat.set_hidden("h", true).unwrap();

        let rows = cat.all_present(Visibility::VisibleOnly).unwrap();
        assert_eq!(paths_of(&rows), vec!["/lib/unhashed.jpg"]);
    }

    #[test]
    fn dominant_colour_roundtrips_and_survives_unchanged_reindex() {
        let cat = Catalog::open_in_memory().unwrap();
        let r = row("/lib/a.jpg", 100, 5);
        let id = cat.upsert_media(&r).unwrap();
        cat.set_analysis(id, Some(1), Some(0xC0FFEE)).unwrap();
        assert_eq!(
            cat.all_present(Visibility::All).unwrap()[0].dom_color,
            Some(0xC0FFEE)
        );

        // Unchanged file: keep it. Changed file: recompute.
        cat.upsert_media(&r).unwrap();
        assert_eq!(
            cat.all_present(Visibility::All).unwrap()[0].dom_color,
            Some(0xC0FFEE)
        );
        cat.upsert_media(&row("/lib/a.jpg", 100, 6)).unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap()[0].dom_color, None);
    }

    #[test]
    fn rows_needing_analysis_covers_colour_added_to_an_older_catalog() {
        // A row hashed before colour extraction existed must be picked up again,
        // rather than needing a full rebuild.
        let cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.set_analysis(id, Some(7), None).unwrap();
        assert_eq!(cat.rows_needing_analysis().unwrap().len(), 1);

        cat.set_analysis(id, Some(7), Some(0x112233)).unwrap();
        assert!(cat.rows_needing_analysis().unwrap().is_empty());
    }

    #[test]
    fn migration_adds_new_columns_to_an_existing_catalog() {
        // Open an old-shaped database and confirm migrating it in place keeps
        // the data instead of forcing a re-index.
        let dir = std::env::temp_dir().join(format!("renamer_migrate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("old.sqlite");

        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE media (
                    id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                    file_name TEXT NOT NULL, ext TEXT NOT NULL,
                    size INTEGER NOT NULL, mtime INTEGER NOT NULL,
                    date_taken INTEGER, camera_make TEXT, camera_model TEXT,
                    lens TEXT, iso TEXT, width INTEGER, height INTEGER,
                    content_hash TEXT, phash INTEGER,
                    missing INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO media (path, file_name, ext, size, mtime, content_hash)
                 VALUES ('/lib/old.jpg', 'old.jpg', 'jpg', 42, 7, 'kept');",
            )
            .unwrap();
        }

        let cat = Catalog::open(&db).unwrap();
        assert_eq!(cat.schema_version().unwrap(), SCHEMA_VERSION);
        let rows = cat.all_present(Visibility::All).unwrap();
        assert_eq!(rows.len(), 1, "pre-existing row preserved");
        assert_eq!(rows[0].content_hash.as_deref(), Some("kept"));
        assert_eq!(rows[0].dom_color, None, "new column reads as NULL");
        assert!(!rows[0].hidden);
        // And the new column is writable.
        cat.set_analysis(rows[0].id, Some(3), Some(0xAABBCC)).unwrap();
        assert_eq!(
            cat.all_present(Visibility::All).unwrap()[0].dom_color,
            Some(0xAABBCC)
        );

        // Idempotent: opening again must not try to re-add the column.
        drop(cat);
        let cat = Catalog::open(&db).unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rating_roundtrips_and_clamps() {
        let cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.set_content_hash(id, "h").unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap()[0].rating, 0);

        for stars in 0..=5u8 {
            cat.set_rating("h", stars).unwrap();
            assert_eq!(cat.all_present(Visibility::All).unwrap()[0].rating, stars);
        }
        // A caller bug must not poison the database with an impossible rating.
        cat.set_rating("h", 99).unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap()[0].rating, 5);
    }

    #[test]
    fn kind_override_roundtrips_and_clears() {
        use crate::kind::MediaKind;
        let cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.set_content_hash(id, "h").unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap()[0].kind_override, None);

        for k in MediaKind::ALL {
            cat.set_kind("h", Some(k)).unwrap();
            assert_eq!(
                cat.all_present(Visibility::All).unwrap()[0].kind_override,
                Some(k)
            );
        }
        cat.set_kind("h", None).unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap()[0].kind_override, None);
    }

    #[test]
    fn user_meta_fields_are_independent() {
        // Each setter writes one column; the others must survive untouched.
        use crate::kind::MediaKind;
        let cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.set_content_hash(id, "h").unwrap();

        cat.set_rating("h", 4).unwrap();
        cat.set_hidden("h", true).unwrap();
        cat.set_kind("h", Some(MediaKind::Saved)).unwrap();

        let got = &cat.all_present(Visibility::All).unwrap()[0];
        assert_eq!(got.rating, 4);
        assert!(got.hidden);
        assert_eq!(got.kind_override, Some(MediaKind::Saved));

        // Changing one still leaves the rest alone.
        cat.set_hidden("h", false).unwrap();
        let got = &cat.all_present(Visibility::All).unwrap()[0];
        assert_eq!(got.rating, 4);
        assert!(!got.hidden);
        assert_eq!(got.kind_override, Some(MediaKind::Saved));
    }

    #[test]
    fn ratings_and_kinds_survive_a_rename() {
        // Same durability guarantee as the hidden flag: keyed on bytes, so the
        // rename engine cannot lose them.
        use crate::kind::MediaKind;
        let mut cat = Catalog::open_in_memory().unwrap();
        let id = cat.upsert_media(&row("/lib/IMG_1.jpg", 100, 5)).unwrap();
        cat.set_content_hash(id, "bytes").unwrap();
        cat.set_rating("bytes", 5).unwrap();
        cat.set_kind("bytes", Some(MediaKind::Photo)).unwrap();

        cat.apply_moves(&[(
            PathBuf::from("/lib/IMG_1.jpg"),
            PathBuf::from("/lib/2019/best.jpg"),
        )])
        .unwrap();

        let got = &cat.all_present(Visibility::All).unwrap()[0];
        assert_eq!(got.path, PathBuf::from("/lib/2019/best.jpg"));
        assert_eq!(got.rating, 5);
        assert_eq!(got.kind_override, Some(MediaKind::Photo));
    }

    #[test]
    fn migrating_a_v2_catalog_adds_the_user_meta_columns() {
        let dir = std::env::temp_dir().join(format!("renamer_migrate_v3_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v2.sqlite");

        {
            // A v2-shaped user_meta: hidden but no rating or kind.
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE user_meta (
                    content_hash TEXT PRIMARY KEY,
                    hidden INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL);
                 INSERT INTO user_meta (content_hash, hidden, updated_at)
                 VALUES ('kept', 1, 0);",
            )
            .unwrap();
        }

        let cat = Catalog::open(&db).unwrap();
        assert_eq!(cat.schema_version().unwrap(), SCHEMA_VERSION);
        // The existing hidden flag survives, and the new columns default sanely.
        assert!(cat.is_hidden("kept").unwrap());
        let id = cat.upsert_media(&row("/lib/a.jpg", 1, 1)).unwrap();
        cat.set_content_hash(id, "kept").unwrap();
        let got = &cat.all_present(Visibility::All).unwrap()[0];
        assert!(got.hidden);
        assert_eq!(got.rating, 0);
        assert_eq!(got.kind_override, None);

        cat.set_rating("kept", 3).unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap()[0].rating, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn geotag_roundtrips_including_negative_hemispheres() {
        let cat = Catalog::open_in_memory().unwrap();
        let mut r = row("/lib/a.jpg", 100, 1);
        r.lat = Some(-33.8688);
        r.lon = Some(151.2093);
        cat.upsert_media(&r).unwrap();

        let got = &cat.all_present(Visibility::All).unwrap()[0];
        assert_eq!(got.lat, Some(-33.8688));
        assert_eq!(got.lon, Some(151.2093));
        assert_eq!(got.coords(), Some((-33.8688, 151.2093)));
    }

    #[test]
    fn a_photo_with_no_geotag_has_no_coords() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        assert_eq!(cat.all_present(Visibility::All).unwrap()[0].coords(), None);
    }

    #[test]
    fn half_a_geotag_yields_no_coords() {
        // One axis alone cannot place a photo.
        let cat = Catalog::open_in_memory().unwrap();
        let mut r = row("/lib/a.jpg", 100, 1);
        r.lat = Some(51.5);
        cat.upsert_media(&r).unwrap();
        let got = &cat.all_present(Visibility::All).unwrap()[0];
        assert_eq!(got.lat, Some(51.5));
        assert_eq!(got.coords(), None);
    }

    #[test]
    fn upsert_stamps_the_current_metadata_version() {
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        let stamps = cat.stamps_under(Path::new("/lib")).unwrap();
        assert_eq!(
            stamps.get(Path::new("/lib/a.jpg")).unwrap().meta_version,
            META_VERSION
        );
    }

    #[test]
    fn rows_from_an_older_catalog_report_an_older_metadata_version() {
        // This is what makes a newly-extracted field reach files indexed before
        // it existed: the stamp is behind, so the indexer re-examines them.
        let cat = Catalog::open_in_memory().unwrap();
        cat.upsert_media(&row("/lib/a.jpg", 100, 1)).unwrap();
        cat.mark_meta_stale_for_tests();

        let stamps = cat.stamps_under(Path::new("/lib")).unwrap();
        let stamp = stamps.get(Path::new("/lib/a.jpg")).unwrap();
        assert!(
            stamp.meta_version < META_VERSION,
            "stale row must be detectable"
        );
        // Size and mtime still match, so version is the only thing that would
        // trigger a re-read.
        assert_eq!((stamp.size, stamp.mtime), (100, 1));
    }

    #[test]
    fn migrating_a_v3_catalog_adds_geotag_columns_and_a_zero_stamp() {
        let dir = std::env::temp_dir().join(format!("renamer_migrate_v4_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v3.sqlite");

        {
            // v3 media: everything up to dom_color, no lat/lon/meta_version.
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE media (
                    id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                    file_name TEXT NOT NULL, ext TEXT NOT NULL,
                    size INTEGER NOT NULL, mtime INTEGER NOT NULL,
                    date_taken INTEGER, camera_make TEXT, camera_model TEXT,
                    lens TEXT, iso TEXT, width INTEGER, height INTEGER,
                    content_hash TEXT, phash INTEGER,
                    missing INTEGER NOT NULL DEFAULT 0, dom_color INTEGER);
                 INSERT INTO media (path, file_name, ext, size, mtime, content_hash, dom_color)
                 VALUES ('/lib/old.jpg', 'old.jpg', 'jpg', 42, 7, 'kept', 255);",
            )
            .unwrap();
        }

        let cat = Catalog::open(&db).unwrap();
        assert_eq!(cat.schema_version().unwrap(), SCHEMA_VERSION);

        let rows = cat.all_present(Visibility::All).unwrap();
        assert_eq!(rows.len(), 1, "existing row preserved");
        assert_eq!(rows[0].content_hash.as_deref(), Some("kept"));
        assert_eq!(rows[0].dom_color, Some(255), "earlier work kept");
        assert_eq!(rows[0].coords(), None, "new columns read as NULL");

        // And the stamp defaults to 0, so this row is queued for a re-read
        // rather than being stuck without coordinates forever.
        let stamps = cat.stamps_under(Path::new("/lib")).unwrap();
        assert_eq!(stamps.get(Path::new("/lib/old.jpg")).unwrap().meta_version, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn like_prefix_escapes_and_anchors() {
        assert_eq!(like_prefix(Path::new("/a/b")), "/a/b/%");
        assert_eq!(like_prefix(Path::new("/a/b/")), "/a/b/%");
        assert_eq!(like_prefix(Path::new("/lib_a")), "/lib\\_a/%");
        assert_eq!(like_prefix(Path::new("/100%")), "/100\\%/%");
    }
}
