//! Hashes used for duplicate detection.
//!
//! Two independent signals, because they answer different questions:
//!
//! * [`content_hash`] — BLAKE3 over the file bytes. Answers "are these the same
//!   file?". Zero false positives.
//! * [`perceptual_hash_from_image`] — a 64-bit dHash over a decoded, downscaled
//!   greyscale image. Answers "are these the same *picture*?", so it survives
//!   resizing, recompression and format changes (the original alongside its
//!   WhatsApp-shrunk copy). Compared by Hamming distance, so it needs a radius
//!   search rather than a hash-map lookup — hence [`BkTree`].

use std::io::Read;
use std::path::Path;

/// Hash the full contents of a file. Streamed in chunks so a 100 MB raw file
/// never lands in memory all at once.
pub fn content_hash(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Cheap stable key derived from a path (not its contents). Used to name
/// thumbnail cache files.
pub fn key_hash(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for p in parts {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f"); // separator, so ("a","bc") != ("ab","c")
    }
    hasher.finalize().to_hex()[..32].to_string()
}

/// Width/height of the greyscale grid the dHash is computed over. A 9x8 grid
/// yields 8 horizontal comparisons per row across 8 rows = 64 bits.
const DHASH_W: u32 = 9;
const DHASH_H: u32 = 8;

/// Compute a 64-bit difference hash from an already-decoded image.
///
/// dHash encodes the *gradient* between horizontally adjacent pixels rather
/// than absolute brightness, which makes it robust to overall exposure and
/// gamma shifts while staying sensitive to composition.
pub fn perceptual_hash_from_image(img: &image::DynamicImage) -> u64 {
    use image::imageops::FilterType;
    let small = img
        .resize_exact(DHASH_W, DHASH_H, FilterType::Triangle)
        .to_luma8();
    let mut hash: u64 = 0;
    let mut bit = 0;
    for y in 0..DHASH_H {
        for x in 0..(DHASH_W - 1) {
            let left = small.get_pixel(x, y).0[0];
            let right = small.get_pixel(x + 1, y).0[0];
            if left > right {
                hash |= 1 << bit;
            }
            bit += 1;
        }
    }
    hash
}

/// Number of differing bits between two perceptual hashes. 0 means the
/// downscaled images are identical; in practice <= 6 is "same picture" and
/// >= 16 is unrelated.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Burkhard-Keller tree over 64-bit hashes under the Hamming metric.
///
/// Near-duplicate search is a radius query, not an equality lookup, so a
/// hash-map is useless and brute force is O(n^2) — 100k photos would be 5
/// billion comparisons per scan. Hamming distance is a true metric, so a BK-tree
/// prunes by triangle inequality and returns *exact* results (unlike banded
/// LSH, which silently loses recall at larger radii).
#[derive(Debug, Default)]
pub struct BkTree {
    nodes: Vec<Node>,
    root: Option<usize>,
}

#[derive(Debug)]
struct Node {
    hash: u64,
    /// Payload index into the caller's own collection.
    id: usize,
    /// (distance from this node, child node index)
    children: Vec<(u32, usize)>,
}

impl BkTree {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)] // asserted by tests
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    #[allow(dead_code)] // asserted by tests
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Insert `hash` carrying an opaque `id`. Duplicate hashes are kept as
    /// distinct nodes at distance 0 so every member is findable.
    pub fn insert(&mut self, hash: u64, id: usize) {
        let new_idx = self.nodes.len();
        self.nodes.push(Node {
            hash,
            id,
            children: Vec::new(),
        });
        let Some(root) = self.root else {
            self.root = Some(new_idx);
            return;
        };

        let mut cur = root;
        loop {
            let d = hamming(self.nodes[cur].hash, hash);
            match self.nodes[cur].children.iter().find(|(cd, _)| *cd == d) {
                Some(&(_, child)) => cur = child,
                None => {
                    self.nodes[cur].children.push((d, new_idx));
                    return;
                }
            }
        }
    }

    /// All ids whose hash is within `radius` bits of `hash`.
    pub fn within(&self, hash: u64, radius: u32) -> Vec<usize> {
        let mut out = Vec::new();
        let Some(root) = self.root else {
            return out;
        };
        let mut stack = vec![root];
        while let Some(idx) = stack.pop() {
            let node = &self.nodes[idx];
            let d = hamming(node.hash, hash);
            if d <= radius {
                out.push(node.id);
            }
            // Triangle inequality: only children whose stored distance falls in
            // [d - radius, d + radius] can hold a match.
            let lo = d.saturating_sub(radius);
            let hi = d + radius;
            for &(cd, child) in &node.children {
                if cd >= lo && cd <= hi {
                    stack.push(child);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    #[test]
    fn content_hash_matches_for_identical_bytes() {
        let dir = std::env::temp_dir().join(format!("renamer_hash_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        let c = dir.join("c.bin");
        std::fs::write(&a, b"hello world").unwrap();
        std::fs::write(&b, b"hello world").unwrap();
        std::fs::write(&c, b"hello worlds").unwrap();

        let ha = content_hash(&a).unwrap();
        assert_eq!(ha, content_hash(&b).unwrap());
        assert_ne!(ha, content_hash(&c).unwrap());
        assert_eq!(ha.len(), 64); // blake3 hex

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn content_hash_handles_multi_chunk_files() {
        let dir = std::env::temp_dir().join(format!("renamer_hash_big_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.bin");
        // Larger than the 64 KiB read buffer, to exercise the streaming loop.
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&big, &data).unwrap();
        let expected = blake3::hash(&data).to_hex().to_string();
        assert_eq!(content_hash(&big).unwrap(), expected);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Vertical gradient bars, so the dHash has real horizontal structure.
    fn gradient_image(w: u32, h: u32, shift: u8) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            let v = ((x * 37 + shift as u32 * 11) % 256) as u8;
            *px = Rgb([v, v, v]);
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn perceptual_hash_survives_rescaling() {
        let big = gradient_image(400, 300, 0);
        let small = big.resize_exact(200, 150, image::imageops::FilterType::Triangle);
        let hb = perceptual_hash_from_image(&big);
        let hs = perceptual_hash_from_image(&small);
        // A pure downscale should be near-identical perceptually.
        assert!(
            hamming(hb, hs) <= 4,
            "rescaled copy should stay close, got distance {}",
            hamming(hb, hs)
        );
    }

    #[test]
    fn perceptual_hash_separates_different_images() {
        let a = perceptual_hash_from_image(&gradient_image(400, 300, 0));
        let b = perceptual_hash_from_image(&gradient_image(400, 300, 7));
        assert!(
            hamming(a, b) > 4,
            "different images should be far apart, got {}",
            hamming(a, b)
        );
    }

    #[test]
    fn hamming_counts_bit_differences() {
        assert_eq!(hamming(0, 0), 0);
        assert_eq!(hamming(0b1011, 0b1001), 1);
        assert_eq!(hamming(u64::MAX, 0), 64);
    }

    #[test]
    fn bktree_radius_search_is_exact() {
        // Brute force is the oracle: the tree must agree with it exactly.
        let hashes: Vec<u64> = (0..500u64)
            .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .collect();
        let mut tree = BkTree::new();
        for (i, &h) in hashes.iter().enumerate() {
            tree.insert(h, i);
        }
        assert_eq!(tree.len(), 500);

        for radius in [0u32, 3, 8, 16] {
            for &probe in hashes.iter().take(25) {
                let mut got = tree.within(probe, radius);
                got.sort_unstable();
                let mut want: Vec<usize> = hashes
                    .iter()
                    .enumerate()
                    .filter(|(_, &h)| hamming(h, probe) <= radius)
                    .map(|(i, _)| i)
                    .collect();
                want.sort_unstable();
                assert_eq!(got, want, "radius {radius} mismatch");
            }
        }
    }

    #[test]
    fn bktree_keeps_duplicate_hashes_distinct() {
        let mut tree = BkTree::new();
        tree.insert(42, 0);
        tree.insert(42, 1);
        tree.insert(42, 2);
        let mut got = tree.within(42, 0);
        got.sort_unstable();
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn empty_bktree_returns_nothing() {
        let tree = BkTree::new();
        assert!(tree.is_empty());
        assert!(tree.within(1, 64).is_empty());
    }

    #[test]
    fn key_hash_is_stable_and_separated() {
        assert_eq!(key_hash(&["a", "b"]), key_hash(&["a", "b"]));
        // The separator prevents component-boundary collisions.
        assert_ne!(key_hash(&["a", "bc"]), key_hash(&["ab", "c"]));
        assert_eq!(key_hash(&["x"]).len(), 32);
    }
}
