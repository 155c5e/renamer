//! Dominant colour extraction and hue ordering.
//!
//! Used to sort a library by colour. The dominant colour is found by k-means
//! over a downsampled copy of the image, taking the largest cluster — *not* by
//! averaging pixels, which turns every photo into the same muddy brown (average
//! a red sunset against a blue sea and you get grey).
//!
//! Extraction runs inside the indexer's existing decode pass, so it costs
//! effectively nothing: the image is already decoded for the perceptual hash and
//! thumbnail.

/// Edge length the image is reduced to before clustering. 64x64 = 4096 samples,
/// plenty to find dominant colours and small enough that k-means is trivial.
const SAMPLE_EDGE: u32 = 64;

/// Number of clusters. Enough to separate subject from background without
/// splitting a single subject across several clusters.
const K: usize = 5;

/// Fixed iteration count. k-means converges fast on so few points, and a fixed
/// count keeps the result deterministic — the same photo must always sort to the
/// same place.
const ITERATIONS: usize = 12;

/// Dominant colour of an already-decoded image, packed as `0xRRGGBB`.
pub fn dominant_colour(img: &image::DynamicImage) -> u32 {
    use image::imageops::FilterType;
    let small = img
        .resize_exact(SAMPLE_EDGE, SAMPLE_EDGE, FilterType::Triangle)
        .to_rgb8();
    let pixels: Vec<[f32; 3]> = small
        .pixels()
        .map(|p| [p.0[0] as f32, p.0[1] as f32, p.0[2] as f32])
        .collect();
    let (r, g, b) = kmeans_dominant(&pixels);
    pack(r, g, b)
}

/// k-means over RGB, returning the centroid of the largest cluster.
fn kmeans_dominant(pixels: &[[f32; 3]]) -> (u8, u8, u8) {
    if pixels.is_empty() {
        return (0, 0, 0);
    }

    // Deterministic seeding: spread the initial centroids evenly through the
    // sample. Random seeding would make the same photo sort differently between
    // runs, which looks like a bug to anyone using colour sort.
    let mut centroids: Vec<[f32; 3]> = (0..K)
        .map(|i| pixels[(i * pixels.len()) / K.max(1) % pixels.len()])
        .collect();

    let mut assignment = vec![0usize; pixels.len()];
    for _ in 0..ITERATIONS {
        let mut moved = false;
        for (i, px) in pixels.iter().enumerate() {
            let mut best = 0;
            let mut best_d = f32::MAX;
            for (c, centroid) in centroids.iter().enumerate() {
                let d = sq_dist(px, centroid);
                if d < best_d {
                    best_d = d;
                    best = c;
                }
            }
            if assignment[i] != best {
                assignment[i] = best;
                moved = true;
            }
        }

        let mut sums = vec![[0f32; 3]; K];
        let mut counts = vec![0usize; K];
        for (i, px) in pixels.iter().enumerate() {
            let c = assignment[i];
            sums[c][0] += px[0];
            sums[c][1] += px[1];
            sums[c][2] += px[2];
            counts[c] += 1;
        }
        for c in 0..K {
            if counts[c] > 0 {
                let n = counts[c] as f32;
                centroids[c] = [sums[c][0] / n, sums[c][1] / n, sums[c][2] / n];
            }
        }
        if !moved {
            break; // converged
        }
    }

    // Largest cluster wins. Ties break on the lower index so the result stays
    // deterministic.
    let mut counts = vec![0usize; K];
    for &c in &assignment {
        counts[c] += 1;
    }
    let best = counts
        .iter()
        .enumerate()
        .max_by_key(|(i, n)| (**n, std::cmp::Reverse(*i)))
        .map(|(i, _)| i)
        .unwrap_or(0);

    let c = centroids[best];
    (
        c[0].round().clamp(0.0, 255.0) as u8,
        c[1].round().clamp(0.0, 255.0) as u8,
        c[2].round().clamp(0.0, 255.0) as u8,
    )
}

fn sq_dist(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    let (dr, dg, db) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    dr * dr + dg * dg + db * db
}

pub fn pack(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

pub fn unpack(c: u32) -> (u8, u8, u8) {
    (((c >> 16) & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, (c & 0xFF) as u8)
}

/// Hue (0–360), saturation (0–1), lightness (0–1).
pub fn to_hsl(c: u32) -> (f32, f32, f32) {
    let (r, g, b) = unpack(c);
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let lightness = (max + min) / 2.0;
    if delta <= f32::EPSILON {
        return (0.0, 0.0, lightness); // greyscale has no meaningful hue
    }

    let saturation = if lightness > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };

    let mut hue = if max == r {
        ((g - b) / delta) % 6.0
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } * 60.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    (hue, saturation, lightness)
}

/// Sort key arranging a library into a rainbow.
///
/// Near-greyscale images are pushed to the end rather than being scattered
/// through the spectrum on meaningless hue values — a black-and-white photo has
/// no hue, and interleaving those with saturated ones makes the sort look
/// broken. Within the greys, ordering is by lightness.
pub fn hue_sort_key(colour: Option<u32>) -> (u8, i32, i32) {
    let Some(c) = colour else {
        return (2, 0, 0); // unanalysed: last
    };
    let (h, s, l) = to_hsl(c);
    if s < 0.15 {
        (1, (l * 1000.0) as i32, 0)
    } else {
        (0, (h * 10.0) as i32, (l * 1000.0) as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};

    fn solid(w: u32, h: u32, colour: [u8; 3]) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for (_x, _y, px) in img.enumerate_pixels_mut() {
            *px = Rgb(colour);
        }
        DynamicImage::ImageRgb8(img)
    }

    /// `fraction` of the width painted `a`, the rest `b`.
    fn two_tone(w: u32, h: u32, a: [u8; 3], b: [u8; 3], fraction: f32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        let split = (w as f32 * fraction) as u32;
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = Rgb(if x < split { a } else { b });
        }
        DynamicImage::ImageRgb8(img)
    }

    fn close(a: u32, b: u32, tol: i32) -> bool {
        let (ar, ag, ab) = unpack(a);
        let (br, bg, bb) = unpack(b);
        (ar as i32 - br as i32).abs() <= tol
            && (ag as i32 - bg as i32).abs() <= tol
            && (ab as i32 - bb as i32).abs() <= tol
    }

    #[test]
    fn solid_image_yields_that_colour() {
        let c = dominant_colour(&solid(80, 80, [200, 30, 40]));
        assert!(close(c, pack(200, 30, 40), 4), "got {c:06X}");
    }

    #[test]
    fn majority_colour_wins_over_minority() {
        // 80% red, 20% blue: the dominant colour is red, and critically NOT the
        // purple an averaging implementation would produce.
        let img = two_tone(100, 100, [220, 20, 20], [20, 20, 220], 0.8);
        let c = dominant_colour(&img);
        let (r, g, b) = unpack(c);
        assert!(r > 150, "expected a red-dominant result, got {c:06X}");
        assert!(b < 90, "must not blend toward purple, got {c:06X}");
        assert!(g < 90);
    }

    #[test]
    fn minority_subject_does_not_hijack_the_result() {
        let img = two_tone(100, 100, [20, 200, 20], [10, 10, 10], 0.15);
        let (r, g, b) = unpack(dominant_colour(&img));
        // Mostly near-black, so that should win.
        assert!(r < 80 && g < 80 && b < 80);
    }

    #[test]
    fn extraction_is_deterministic() {
        // Colour sort must be stable across runs, so the same input must always
        // give the same output.
        let img = two_tone(64, 64, [10, 120, 200], [200, 180, 20], 0.55);
        let first = dominant_colour(&img);
        for _ in 0..5 {
            assert_eq!(dominant_colour(&img), first);
        }
    }

    #[test]
    fn pack_unpack_roundtrip() {
        for c in [(0, 0, 0), (255, 255, 255), (18, 52, 86), (200, 30, 40)] {
            assert_eq!(unpack(pack(c.0, c.1, c.2)), c);
        }
        assert_eq!(pack(0x12, 0x34, 0x56), 0x123456);
    }

    #[test]
    fn hsl_matches_known_values() {
        let (h, s, l) = to_hsl(pack(255, 0, 0));
        assert!((h - 0.0).abs() < 0.5 && s > 0.99 && (l - 0.5).abs() < 0.01);

        let (h, _, _) = to_hsl(pack(0, 255, 0));
        assert!((h - 120.0).abs() < 0.5);

        let (h, _, _) = to_hsl(pack(0, 0, 255));
        assert!((h - 240.0).abs() < 0.5);

        // Greys have no hue and zero saturation.
        let (_, s, l) = to_hsl(pack(128, 128, 128));
        assert!(s < 0.01);
        assert!((l - 0.502).abs() < 0.01);
    }

    #[test]
    fn hue_sort_orders_spectrum_then_greys_then_unknown() {
        let mut items = vec![
            ("grey", hue_sort_key(Some(pack(128, 128, 128)))),
            ("unknown", hue_sort_key(None)),
            ("blue", hue_sort_key(Some(pack(0, 0, 255)))),
            ("red", hue_sort_key(Some(pack(255, 0, 0)))),
            ("green", hue_sort_key(Some(pack(0, 255, 0)))),
        ];
        items.sort_by_key(|(_, k)| *k);
        let order: Vec<&str> = items.into_iter().map(|(n, _)| n).collect();
        // Saturated colours in spectrum order, then desaturated, then unanalysed.
        assert_eq!(order, vec!["red", "green", "blue", "grey", "unknown"]);
    }

    #[test]
    fn near_greyscale_is_not_scattered_through_the_spectrum() {
        // A slightly-tinted grey has a hue, but sorting by it would drop a
        // black-and-white photo in the middle of the colours.
        let (bucket, _, _) = hue_sort_key(Some(pack(130, 128, 126)));
        assert_eq!(bucket, 1, "near-grey belongs in the grey bucket");
        let (bucket, _, _) = hue_sort_key(Some(pack(220, 20, 20)));
        assert_eq!(bucket, 0);
    }

    #[test]
    fn tiny_and_degenerate_images_do_not_panic() {
        assert_eq!(dominant_colour(&solid(1, 1, [7, 7, 7])) & 0xFF_FFFF, pack(7, 7, 7));
        let _ = dominant_colour(&solid(3, 1, [0, 0, 0]));
    }
}
