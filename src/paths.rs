//! Where the app keeps its own state.
//!
//! Nothing the catalog produces is written into the folders being organized —
//! no sidecar files, no `.thumbnails` directories, no stray databases in the
//! middle of a photo library. Everything lives under the platform's standard
//! per-user data/cache locations:
//!
//! | | Linux | macOS |
//! |---|---|---|
//! | catalog | `~/.local/share/renamer/catalog.sqlite` | `~/Library/Application Support/renamer/catalog.sqlite` |
//! | thumbnails | `~/.cache/renamer/thumbs/` | `~/Library/Caches/renamer/thumbs/` |
//! | quarantine | `~/.local/share/renamer/quarantine/` | `~/Library/Application Support/renamer/quarantine/` |
//!
//! `RENAMER_DATA_DIR` / `RENAMER_CACHE_DIR` override the data and cache roots
//! respectively. The overrides exist for the tests, and are genuinely useful for
//! putting the quarantine on the same volume as a large library so that
//! quarantining is an instant rename rather than a copy.

use std::path::{Path, PathBuf};

const APP: &str = "renamer";

/// `$HOME`, or the current directory as a last resort.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Root for durable state: the catalog and the quarantine. Losing this loses
/// quarantined files and the index, so it counts as data, not cache.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RENAMER_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support").join(APP)
    } else {
        // XDG: $XDG_DATA_HOME, else ~/.local/share
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".local/share"))
            .join(APP)
    }
}

/// Root for regenerable state: thumbnails. Safe to delete at any time.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RENAMER_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        home().join("Library/Caches").join(APP)
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".cache"))
            .join(APP)
    }
}

/// SQLite catalog file. One catalog covers every indexed library.
pub fn catalog_path() -> PathBuf {
    data_dir().join("catalog.sqlite")
}

/// Thumbnail cache root.
pub fn thumbs_dir() -> PathBuf {
    cache_dir().join("thumbs")
}

/// Quarantine root: where "deleted" duplicates actually go.
pub fn quarantine_dir() -> PathBuf {
    data_dir().join("quarantine")
}

/// Where face-recognition ONNX models are looked for.
///
/// Not vendored with the app: the usable pretrained weights are licensed for
/// non-commercial research only, so redistributing them here would relicense
/// someone else's work by accident. See `docs/face-models.md`.
#[allow(dead_code)] // referenced by docs; the inference pass is not wired up yet
pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

/// Create `dir` (and parents) if missing.
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Serializes tests that mutate process-global environment variables.
///
/// Environment is per-process, not per-test, and Rust runs tests in parallel
/// threads — without this lock a test that sets `RENAMER_CACHE_DIR` can be
/// observed by an unrelated test mid-assertion.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_are_honored() {
        let _guard = env_lock();
        std::env::set_var("RENAMER_DATA_DIR", "/tmp/renamer-data-test");
        std::env::set_var("RENAMER_CACHE_DIR", "/tmp/renamer-cache-test");

        assert_eq!(data_dir(), PathBuf::from("/tmp/renamer-data-test"));
        assert_eq!(cache_dir(), PathBuf::from("/tmp/renamer-cache-test"));
        assert_eq!(
            catalog_path(),
            PathBuf::from("/tmp/renamer-data-test/catalog.sqlite")
        );
        assert_eq!(
            quarantine_dir(),
            PathBuf::from("/tmp/renamer-data-test/quarantine")
        );
        assert_eq!(
            thumbs_dir(),
            PathBuf::from("/tmp/renamer-cache-test/thumbs")
        );
        assert_eq!(
            models_dir(),
            PathBuf::from("/tmp/renamer-data-test/models")
        );

        std::env::remove_var("RENAMER_DATA_DIR");
        std::env::remove_var("RENAMER_CACHE_DIR");
    }

    #[test]
    fn defaults_are_absolute_and_outside_the_cwd() {
        let _guard = env_lock();
        std::env::remove_var("RENAMER_DATA_DIR");
        std::env::remove_var("RENAMER_CACHE_DIR");
        std::env::set_var("HOME", "/home/tester");
        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var("XDG_CACHE_HOME");

        // The critical property: app state never resolves relative to the
        // current directory, which could itself be a photo folder.
        assert!(data_dir().is_absolute());
        assert!(cache_dir().is_absolute());
        assert!(data_dir().starts_with("/home/tester"));
        assert!(cache_dir().starts_with("/home/tester"));
        assert!(data_dir().ends_with("renamer"));
    }

    #[test]
    fn xdg_vars_win_over_home_on_linux() {
        let _guard = env_lock();
        std::env::remove_var("RENAMER_DATA_DIR");
        std::env::remove_var("RENAMER_CACHE_DIR");
        std::env::set_var("HOME", "/home/tester");
        std::env::set_var("XDG_DATA_HOME", "/xdg/data");
        std::env::set_var("XDG_CACHE_HOME", "/xdg/cache");

        if cfg!(target_os = "macos") {
            // macOS ignores XDG by design.
            assert!(data_dir().starts_with("/home/tester/Library"));
        } else {
            assert_eq!(data_dir(), PathBuf::from("/xdg/data/renamer"));
            assert_eq!(cache_dir(), PathBuf::from("/xdg/cache/renamer"));
        }

        // A relative XDG path is invalid per spec and must be ignored.
        std::env::set_var("XDG_DATA_HOME", "relative/path");
        if !cfg!(target_os = "macos") {
            assert_eq!(data_dir(), PathBuf::from("/home/tester/.local/share/renamer"));
        }

        std::env::remove_var("XDG_DATA_HOME");
        std::env::remove_var("XDG_CACHE_HOME");
    }
}
