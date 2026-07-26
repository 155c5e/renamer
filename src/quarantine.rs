//! Quarantine: reversible removal of duplicate files.
//!
//! Nothing in this module ever deletes a photo. "Removing" a duplicate moves it
//! into a quarantine folder under the app data directory and records the move in
//! the catalog's ledger, so [`restore_batch`] can put every file back exactly
//! where it came from. Emptying the quarantine for real is left to the user, in
//! their file manager, once they are satisfied — which means a false-positive
//! near-duplicate match can never cost data.
//!
//! The quarantine mirrors the original directory tree under a per-batch folder:
//!
//! ```text
//! ~/.local/share/renamer/quarantine/batch-0007/home/me/Pictures/2019/IMG_1234.jpg
//! ```
//!
//! so a quarantined file is identifiable by eye without consulting the database.

use std::path::{Path, PathBuf};

use crate::catalog::Catalog;
use crate::engine::Progress;

/// Result of quarantining a selection.
#[derive(Debug, Clone, Default)]
pub struct QuarantineOutcome {
    /// Batch number, for undo. `None` when nothing was moved.
    pub batch: Option<i64>,
    pub moved: usize,
    /// Bytes moved out of the library.
    pub bytes: u64,
    pub errors: Vec<String>,
}

/// Move `paths` into a fresh quarantine batch, recording each move so it can be
/// undone. Files that fail to move are reported and left untouched.
pub fn quarantine(
    catalog: &Catalog,
    paths: &[PathBuf],
    progress: &Progress,
) -> QuarantineOutcome {
    let mut out = QuarantineOutcome::default();
    if paths.is_empty() {
        return out;
    }
    progress.set_total(paths.len());

    let batch = match catalog.next_quarantine_batch() {
        Ok(b) => b,
        Err(e) => {
            out.errors.push(format!("catalog: {e}"));
            return out;
        }
    };
    let root = crate::paths::quarantine_dir().join(format!("batch-{batch:04}"));

    for src in paths {
        progress.inc();

        // Size is read before the move so the caller can report bytes freed.
        let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        let dest = match reserve_dest(&root, src) {
            Ok(d) => d,
            Err(e) => {
                out.errors.push(format!("{}: {e}", src.display()));
                continue;
            }
        };

        match move_file(src, &dest) {
            Ok(()) => {
                if let Err(e) = catalog.record_quarantine(batch, src, &dest) {
                    // The file has moved but the ledger write failed. Put it
                    // back rather than leave an unrecorded, unrestorable move.
                    let _ = move_file(&dest, src);
                    out.errors
                        .push(format!("{}: ledger write failed: {e}", src.display()));
                    continue;
                }
                // The catalog row is stale now; the file is no longer in the
                // library. A later rescan would drop it anyway.
                let _ = catalog.remove_path(src);
                out.moved += 1;
                out.bytes += size;
            }
            Err(e) => out.errors.push(format!("{}: {e}", src.display())),
        }
    }

    if out.moved > 0 {
        out.batch = Some(batch);
    }
    out
}

/// Move every file of `batch` back to its original location.
///
/// Returns `(restored, errors)`. A file whose original path is occupied again is
/// reported as an error and left in quarantine rather than overwriting whatever
/// now lives there.
pub fn restore_batch(
    catalog: &Catalog,
    batch: i64,
    progress: &Progress,
) -> (usize, Vec<String>) {
    let entries = match catalog.quarantine_batch(batch) {
        Ok(e) => e,
        Err(e) => return (0, vec![format!("catalog: {e}")]),
    };
    progress.set_total(entries.len());

    let mut restored = 0;
    let mut errors = Vec::new();
    for entry in &entries {
        progress.inc();

        if entry.original_path.exists() {
            errors.push(format!(
                "{}: something is already there, left in quarantine",
                entry.original_path.display()
            ));
            continue;
        }
        if let Some(parent) = entry.original_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                errors.push(format!("{}: mkdir: {e}", entry.original_path.display()));
                continue;
            }
        }
        match move_file(&entry.quarantine_path, &entry.original_path) {
            Ok(()) => {
                if let Err(e) = catalog.mark_restored(entry.id) {
                    errors.push(format!("{}: ledger: {e}", entry.original_path.display()));
                }
                restored += 1;
            }
            Err(e) => errors.push(format!("{}: {e}", entry.quarantine_path.display())),
        }
    }

    // Tidy up now-empty batch folders, deepest first. `remove_dir` only succeeds
    // on an empty directory, so this can never discard a file.
    let mut dirs: Vec<PathBuf> = entries
        .iter()
        .filter_map(|e| e.quarantine_path.parent().map(|p| p.to_path_buf()))
        .collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    dirs.dedup();
    for d in dirs {
        let _ = std::fs::remove_dir(&d);
    }

    (restored, errors)
}

/// Bytes currently held in quarantine.
pub fn quarantine_size() -> u64 {
    walkdir::WalkDir::new(crate::paths::quarantine_dir())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Pick a destination inside `root` mirroring `src`'s absolute path, creating
/// parent directories. Never returns a path that already exists.
fn reserve_dest(root: &Path, src: &Path) -> std::io::Result<PathBuf> {
    // Strip the root prefix so `join` does not discard `root` on absolute paths.
    let relative: PathBuf = src
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .collect();
    let mut dest = root.join(&relative);

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Two libraries can hold the same relative path; never clobber.
    if dest.exists() {
        let stem = dest
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let ext = dest.extension().map(|s| s.to_string_lossy().to_string());
        for n in 2.. {
            let name = match &ext {
                Some(e) => format!("{stem}-{n}.{e}"),
                None => format!("{stem}-{n}"),
            };
            let candidate = dest.with_file_name(name);
            if !candidate.exists() {
                dest = candidate;
                break;
            }
        }
    }
    Ok(dest)
}

/// Move a file, falling back to copy-then-remove across filesystem boundaries.
///
/// `rename` fails with `EXDEV` when source and destination are on different
/// volumes, which is the normal case when the quarantine lives in the home
/// directory and the library on an external disk. The original is removed only
/// after the copy is verified, so an interrupted move can never lose the file.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let copied = std::fs::copy(from, to)?;
    let expected = std::fs::metadata(from)?.len();
    if copied != expected {
        // Leave the original alone; the partial copy is the thing to discard.
        let _ = std::fs::remove_file(to);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("short copy: {copied} of {expected} bytes"),
        ));
    }
    std::fs::remove_file(from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::MediaRow;

    struct Fixture {
        _tmp: PathBuf,
        library: PathBuf,
        catalog: Catalog,
    }

    /// Builds a scratch library plus an in-memory catalog, with the quarantine
    /// pointed at a scratch data dir.
    fn fixture(name: &str) -> (Fixture, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::paths::env_lock();
        let tmp = std::env::temp_dir().join(format!("renamer_q_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let library = tmp.join("library");
        std::fs::create_dir_all(&library).unwrap();
        std::env::set_var("RENAMER_DATA_DIR", tmp.join("data"));

        let catalog = Catalog::open_in_memory().unwrap();
        (
            Fixture {
                _tmp: tmp,
                library,
                catalog,
            },
            guard,
        )
    }

    impl Fixture {
        fn write(&self, name: &str, content: &[u8]) -> PathBuf {
            let p = self.library.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
            self.catalog
                .upsert_media(&MediaRow {
                    path: p.clone(),
                    file_name: name.to_string(),
                    size: content.len() as u64,
                    mtime: 1,
                    ..MediaRow::default()
                })
                .unwrap();
            p
        }
    }

    fn cleanup(f: &Fixture) {
        std::env::remove_var("RENAMER_DATA_DIR");
        let _ = std::fs::remove_dir_all(&f._tmp);
    }

    #[test]
    fn quarantine_moves_files_out_and_restore_puts_them_back() {
        let (f, _guard) = fixture("roundtrip");
        let keep = f.write("keep.jpg", b"original");
        let dup = f.write("copies/dup.jpg", b"original");
        let p = Progress::default();

        let out = quarantine(&f.catalog, &[dup.clone()], &p);
        assert!(out.errors.is_empty(), "errors: {:?}", out.errors);
        assert_eq!(out.moved, 1);
        assert_eq!(out.bytes, 8);
        let batch = out.batch.unwrap();

        // Gone from the library, still on disk in quarantine, keeper untouched.
        assert!(!dup.exists(), "duplicate left the library");
        assert!(keep.exists(), "keeper must never be touched");
        assert!(quarantine_size() >= 8, "file is preserved, not deleted");
        // And dropped from the catalog, since it is no longer in the library.
        assert_eq!(f.catalog.all_present().unwrap().len(), 1);

        let (restored, errors) = restore_batch(&f.catalog, batch, &Progress::default());
        assert_eq!(restored, 1);
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert!(dup.exists(), "file came back to its original path");
        assert_eq!(std::fs::read(&dup).unwrap(), b"original");
        // Ledger is settled, so a second undo is a no-op.
        assert!(f.catalog.quarantine_batch(batch).unwrap().is_empty());

        cleanup(&f);
    }

    #[test]
    fn quarantine_mirrors_the_original_tree_under_a_batch_folder() {
        let (f, _guard) = fixture("layout");
        let dup = f.write("2019/holiday/IMG_1.jpg", b"x");

        let out = quarantine(&f.catalog, &[dup], &Progress::default());
        assert_eq!(out.moved, 1);

        let entries = f.catalog.quarantine_batch(out.batch.unwrap()).unwrap();
        let qp = &entries[0].quarantine_path;
        let text = qp.to_string_lossy();
        assert!(text.contains("batch-0001"), "batch folder in {text}");
        assert!(
            text.ends_with("2019/holiday/IMG_1.jpg"),
            "original tree mirrored: {text}"
        );
        assert!(qp.is_file());

        cleanup(&f);
    }

    #[test]
    fn restore_refuses_to_overwrite_a_reoccupied_path() {
        let (f, _guard) = fixture("occupied");
        let dup = f.write("dup.jpg", b"quarantined");

        let out = quarantine(&f.catalog, &[dup.clone()], &Progress::default());
        let batch = out.batch.unwrap();

        // Something new appears at the original path before the undo.
        std::fs::write(&dup, b"newer file").unwrap();

        let (restored, errors) = restore_batch(&f.catalog, batch, &Progress::default());
        assert_eq!(restored, 0);
        assert_eq!(errors.len(), 1);
        // The newer file survives untouched...
        assert_eq!(std::fs::read(&dup).unwrap(), b"newer file");
        // ...and the quarantined copy is still recoverable by hand.
        let entries = f.catalog.quarantine_batch(batch).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].quarantine_path.is_file());

        cleanup(&f);
    }

    #[test]
    fn colliding_names_from_different_folders_do_not_clobber() {
        let (f, _guard) = fixture("collide");
        // Same basename in two directories. The mirrored tree is what keeps them
        // apart; this pins that behaviour so a future flattening of the layout
        // cannot silently overwrite one with the other.
        let a = f.write("a/IMG.jpg", b"first");
        let b = f.write("b/IMG.jpg", b"second");

        let out = quarantine(&f.catalog, &[a, b], &Progress::default());
        assert_eq!(out.moved, 2, "errors: {:?}", out.errors);

        let entries = f.catalog.quarantine_batch(out.batch.unwrap()).unwrap();
        assert_eq!(entries.len(), 2);
        let mut contents: Vec<Vec<u8>> = entries
            .iter()
            .map(|e| std::fs::read(&e.quarantine_path).unwrap())
            .collect();
        contents.sort();
        // Both survived distinctly: nothing was overwritten.
        assert_eq!(contents, vec![b"first".to_vec(), b"second".to_vec()]);

        cleanup(&f);
    }

    #[test]
    fn reserve_dest_uniquifies_an_existing_target() {
        let (f, _guard) = fixture("reserve");
        let root = f._tmp.join("qroot");
        let src = f.library.join("x.jpg");

        let first = reserve_dest(&root, &src).unwrap();
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::write(&first, b"a").unwrap();

        let second = reserve_dest(&root, &src).unwrap();
        assert_ne!(first, second);
        assert!(second
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("x-2"));

        cleanup(&f);
    }

    #[test]
    fn missing_source_is_reported_not_panicked() {
        let (f, _guard) = fixture("missing");
        let ghost = f.library.join("not-there.jpg");
        let out = quarantine(&f.catalog, &[ghost], &Progress::default());
        assert_eq!(out.moved, 0);
        assert_eq!(out.errors.len(), 1);
        assert!(out.batch.is_none());
        cleanup(&f);
    }

    #[test]
    fn empty_selection_is_a_no_op() {
        let (f, _guard) = fixture("empty");
        let out = quarantine(&f.catalog, &[], &Progress::default());
        assert_eq!(out.moved, 0);
        assert!(out.errors.is_empty());
        assert!(out.batch.is_none());
        // No batch number was consumed.
        assert_eq!(f.catalog.next_quarantine_batch().unwrap(), 1);
        cleanup(&f);
    }

    #[test]
    fn progress_is_reported() {
        let (f, _guard) = fixture("progress");
        let a = f.write("a.jpg", b"1");
        let b = f.write("b.jpg", b"22");
        let p = Progress::default();
        let out = quarantine(&f.catalog, &[a, b], &p);
        assert_eq!(out.moved, 2);
        assert_eq!(p.snapshot(), (2, 2));
        assert_eq!(out.bytes, 3);
        cleanup(&f);
    }

    #[test]
    fn separate_batches_are_independently_restorable() {
        let (f, _guard) = fixture("batches");
        let a = f.write("a.jpg", b"1");
        let b = f.write("b.jpg", b"2");

        let first = quarantine(&f.catalog, &[a.clone()], &Progress::default());
        let second = quarantine(&f.catalog, &[b.clone()], &Progress::default());
        assert_ne!(first.batch, second.batch);
        assert_eq!(f.catalog.latest_restorable_batch().unwrap(), second.batch);

        // Undo only the most recent batch.
        restore_batch(&f.catalog, second.batch.unwrap(), &Progress::default());
        assert!(b.exists());
        assert!(!a.exists(), "the earlier batch stays quarantined");
        assert_eq!(f.catalog.latest_restorable_batch().unwrap(), first.batch);

        cleanup(&f);
    }
}
