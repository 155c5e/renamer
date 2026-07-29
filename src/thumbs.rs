//! Thumbnail cache.
//!
//! Thumbnails go to the platform *cache* directory, never next to the originals
//! — organizing a photo library should not litter it with sidecar files, and the
//! whole cache can be deleted at any time without losing data.
//!
//! Cache keys fold in size and mtime, so an edited file naturally misses the
//! cache instead of showing a stale thumbnail; no invalidation pass is needed.

use std::path::{Path, PathBuf};

use crate::hashing;

/// Longest edge of a generated thumbnail, in pixels.
pub const THUMB_MAX_EDGE: u32 = 256;

/// Cache location for a file's thumbnail.
///
/// Sharded by the first two hex characters of the key so no single directory
/// ends up holding hundreds of thousands of entries, which makes several
/// filesystems crawl.
pub fn thumb_path(path: &Path, size: u64, mtime: i64) -> PathBuf {
    let key = hashing::key_hash(&[
        &path.to_string_lossy(),
        &size.to_string(),
        &mtime.to_string(),
    ]);
    crate::paths::thumbs_dir()
        .join(&key[..2])
        .join(format!("{key}.jpg"))
}

/// True if a usable thumbnail already exists.
pub fn is_cached(path: &Path, size: u64, mtime: i64) -> bool {
    thumb_path(path, size, mtime).is_file()
}

/// Write a thumbnail for an already-decoded image. Returns the cache path.
///
/// Takes a decoded image rather than a path because the indexer's decode pass
/// computes the perceptual hash from the same decode. Decoding dominates the
/// cost of indexing, so it must happen exactly once per file.
pub fn write_thumb(
    img: &image::DynamicImage,
    path: &Path,
    size: u64,
    mtime: i64,
) -> std::io::Result<PathBuf> {
    let dest = thumb_path(path, size, mtime);
    if let Some(parent) = dest.parent() {
        crate::paths::ensure_dir(parent)?;
    }
    // `thumbnail` preserves aspect ratio within the box. Converting to RGB8 is
    // required because the JPEG encoder rejects an alpha channel.
    let thumb = img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE);
    image::DynamicImage::ImageRgb8(thumb.to_rgb8())
        .save_with_format(&dest, image::ImageFormat::Jpeg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    Ok(dest)
}

/// Delete the whole thumbnail cache. Purely regenerable, so always safe.
pub fn clear_cache() -> std::io::Result<()> {
    let dir = crate::paths::thumbs_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// Total bytes currently held by the thumbnail cache.
pub fn cache_size() -> u64 {
    walkdir::WalkDir::new(crate::paths::thumbs_dir())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    fn test_image(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
        }
        DynamicImage::ImageRgb8(img)
    }

    /// Point the cache at a scratch directory for the duration of a test.
    fn with_cache_dir<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let _guard = crate::paths::env_lock();
        let tmp = std::env::temp_dir().join(format!("renamer_thumbs_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("RENAMER_CACHE_DIR", &tmp);
        let out = f(&tmp);
        std::env::remove_var("RENAMER_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
        out
    }

    #[test]
    fn thumb_paths_are_sharded_and_keyed_on_size_and_mtime() {
        with_cache_dir("keys", |_| {
            let p = Path::new("/lib/a.jpg");
            let t1 = thumb_path(p, 100, 10);

            let shard = t1.parent().unwrap().file_name().unwrap().to_string_lossy();
            assert_eq!(shard.len(), 2, "one level of sharding");
            assert!(t1.to_string_lossy().ends_with(".jpg"));

            // Stable for identical inputs...
            assert_eq!(t1, thumb_path(p, 100, 10));
            // ...and distinct when anything in the key changes, so an edited
            // file can never display a stale thumbnail.
            assert_ne!(t1, thumb_path(p, 101, 10));
            assert_ne!(t1, thumb_path(p, 100, 11));
            assert_ne!(t1, thumb_path(Path::new("/lib/b.jpg"), 100, 10));
        });
    }

    #[test]
    fn thumbnails_land_outside_the_source_folder() {
        // The core promise: indexing a library never writes into it.
        with_cache_dir("isolation", |tmp| {
            let library = Path::new("/home/someone/Pictures/Holiday");
            let t = thumb_path(&library.join("IMG_1.jpg"), 10, 1);
            assert!(t.starts_with(tmp), "{t:?} must live under the cache dir");
            assert!(
                !t.starts_with(library),
                "{t:?} must not be written into the library"
            );
        });
    }

    #[test]
    fn write_thumb_creates_a_downscaled_jpeg_and_clear_removes_it() {
        with_cache_dir("write", |_| {
            let src = Path::new("/lib/big.jpg");
            assert!(!is_cached(src, 1, 1));

            let dest = write_thumb(&test_image(1000, 500), src, 1, 1).unwrap();
            assert!(dest.is_file());
            assert!(is_cached(src, 1, 1));

            let decoded = image::open(&dest).unwrap();
            // Fits the box, and the 2:1 aspect ratio is preserved.
            assert_eq!(decoded.width(), THUMB_MAX_EDGE);
            assert_eq!(decoded.height(), THUMB_MAX_EDGE / 2);
            assert!(cache_size() > 0);

            clear_cache().unwrap();
            assert!(!is_cached(src, 1, 1));
            assert_eq!(cache_size(), 0);
        });
    }

    #[test]
    fn write_thumb_handles_rgba_input() {
        // PNGs with alpha must not fail the JPEG encode.
        with_cache_dir("rgba", |_| {
            let mut img = image::RgbaImage::new(64, 64);
            for (_x, _y, px) in img.enumerate_pixels_mut() {
                *px = image::Rgba([10, 20, 30, 128]);
            }
            let rgba = DynamicImage::ImageRgba8(img);
            let dest = write_thumb(&rgba, Path::new("/lib/alpha.png"), 5, 5).unwrap();
            assert!(dest.is_file());
            assert!(image::open(&dest).is_ok());
        });
    }

    #[test]
    fn clear_cache_on_missing_dir_is_not_an_error() {
        with_cache_dir("empty", |_| {
            assert!(clear_cache().is_ok());
            assert_eq!(cache_size(), 0);
        });
    }
}
