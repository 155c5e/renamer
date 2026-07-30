//! Duplicate grouping.
//!
//! Three independent detectors, because "duplicate" means three different things
//! in a real photo library:
//!
//! * **Exact** — byte-identical files, grouped by content hash. The same photo
//!   copied twice off a card, or an import that ran twice. Zero false positives.
//! * **Near** — visually the same picture, grouped by perceptual-hash distance.
//!   Catches the original alongside its resized, recompressed, or
//!   format-converted copy, which exact hashing cannot see.
//! * **Burst** — different frames shot within seconds on the same camera. Not
//!   duplicates in any technical sense, but usually a run of near-identical
//!   frames worth culling down to one keeper.
//!
//! Every group carries a *suggested* keeper. Nothing is ever acted on
//! automatically: the caller decides, and removal goes through
//! [`crate::quarantine`], which never unlinks a file.

use std::collections::HashMap;

use crate::catalog::MediaRow;
use crate::hashing::{self, BkTree};
use crate::union_find::UnionFind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DupKind {
    Exact,
    Near,
    Burst,
}

impl DupKind {
    pub fn label(self) -> &'static str {
        match self {
            DupKind::Exact => "identical",
            DupKind::Near => "near-duplicate",
            DupKind::Burst => "burst",
        }
    }

    /// How much the grouping should be trusted. Exact matches are provable;
    /// burst grouping is a heuristic about intent.
    pub fn confidence(self) -> &'static str {
        match self {
            DupKind::Exact => "byte-identical",
            DupKind::Near => "same image, different encoding or size",
            DupKind::Burst => "shot seconds apart — review before removing",
        }
    }
}

/// Tuning for a duplicate scan.
#[derive(Debug, Clone, Copy)]
pub struct DupConfig {
    /// Maximum Hamming distance between perceptual hashes to call two images the
    /// same picture. 0 is identical after downscaling; ~6 tolerates heavy
    /// recompression; beyond ~12 unrelated photos start matching.
    pub near_threshold: u32,
    /// Seconds between consecutive shots that still count as one burst.
    pub burst_window_secs: i64,
    pub find_exact: bool,
    pub find_near: bool,
    pub find_burst: bool,
    /// Only group images of the same kind together.
    ///
    /// A saved reference image that happens to match a photograph you took is
    /// not a duplicate in any useful sense — you want to keep both — so mixing
    /// the two just adds noise to the review.
    pub same_kind_only: bool,
}

impl Default for DupConfig {
    fn default() -> Self {
        Self {
            near_threshold: 6,
            burst_window_secs: 2,
            find_exact: true,
            find_near: true,
            find_burst: false, // opt-in: a burst is intent, not redundancy
            same_kind_only: true,
        }
    }
}

/// A set of files considered duplicates of one another.
#[derive(Debug, Clone)]
pub struct DupGroup {
    pub kind: DupKind,
    /// Members, sorted by path for stable display.
    pub members: Vec<MediaRow>,
    /// Index into `members` of the suggested keeper.
    pub keeper: usize,
    /// Largest pairwise perceptual distance in the group (`Near` groups only).
    pub spread: u32,
    /// Bytes freed if every non-keeper were quarantined.
    pub reclaimable: u64,
}

impl DupGroup {
    #[allow(dead_code)] // asserted by tests; the UI indexes members directly
    pub fn keeper_row(&self) -> &MediaRow {
        &self.members[self.keeper]
    }

    /// Members other than the suggested keeper.
    #[allow(dead_code)] // asserted by tests
    pub fn others(&self) -> impl Iterator<Item = &MediaRow> {
        let keeper = self.keeper;
        self.members
            .iter()
            .enumerate()
            .filter(move |(i, _)| *i != keeper)
            .map(|(_, r)| r)
    }
}

/// Aggregate result of a duplicate scan.
#[derive(Debug, Clone, Default)]
pub struct DupReport {
    pub groups: Vec<DupGroup>,
}

impl DupReport {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn count_of(&self, kind: DupKind) -> usize {
        self.groups.iter().filter(|g| g.kind == kind).count()
    }

    /// Total bytes recoverable across every group, if all suggested non-keepers
    /// were quarantined.
    pub fn total_reclaimable(&self) -> u64 {
        self.groups.iter().map(|g| g.reclaimable).sum()
    }

    /// Number of files that accepting every suggestion would quarantine.
    #[allow(dead_code)] // asserted by tests; the UI counts the live selection
    pub fn total_removable(&self) -> usize {
        self.groups.iter().map(|g| g.members.len() - 1).sum()
    }
}

/// Find duplicate groups among `rows`.
pub fn find_duplicates(rows: &[MediaRow], cfg: &DupConfig) -> DupReport {
    if cfg.same_kind_only {
        // Partition first, then detect within each kind. Running the detectors
        // per partition is what keeps a saved image from ever pairing with a
        // photograph, in every detector at once.
        let mut buckets: HashMap<crate::kind::MediaKind, Vec<MediaRow>> = HashMap::new();
        for r in rows {
            buckets
                .entry(crate::kind::effective(r))
                .or_default()
                .push(r.clone());
        }
        let mut kinds: Vec<_> = buckets.into_iter().collect();
        kinds.sort_by_key(|(k, _)| *k); // deterministic despite HashMap order

        let per_kind = DupConfig {
            same_kind_only: false,
            ..*cfg
        };
        let mut groups = Vec::new();
        for (_kind, bucket) in kinds {
            groups.extend(find_duplicates(&bucket, &per_kind).groups);
        }
        return finish(groups);
    }

    let mut groups = Vec::new();

    if cfg.find_exact {
        groups.extend(exact_groups(rows));
    }
    if cfg.find_near {
        groups.extend(near_groups(rows, cfg.near_threshold));
    }
    if cfg.find_burst {
        groups.extend(burst_groups(rows, cfg.burst_window_secs));
    }

    finish(groups)
}

/// Order the collected groups and wrap them up.
///
/// Shared by the partitioned and unpartitioned paths so both produce the same
/// ordering: strongest evidence first, then biggest win.
fn finish(mut groups: Vec<DupGroup>) -> DupReport {
    groups.sort_by(|a, b| {
        kind_rank(a.kind)
            .cmp(&kind_rank(b.kind))
            .then(b.reclaimable.cmp(&a.reclaimable))
            .then(a.members[0].path.cmp(&b.members[0].path))
    });
    DupReport { groups }
}

fn kind_rank(k: DupKind) -> u8 {
    match k {
        DupKind::Exact => 0,
        DupKind::Near => 1,
        DupKind::Burst => 2,
    }
}

/// Group byte-identical files by content hash.
fn exact_groups(rows: &[MediaRow]) -> Vec<DupGroup> {
    let mut by_hash: HashMap<&str, Vec<&MediaRow>> = HashMap::new();
    for r in rows {
        if let Some(h) = r.content_hash.as_deref() {
            by_hash.entry(h).or_default().push(r);
        }
    }
    by_hash
        .into_values()
        .filter(|v| v.len() > 1)
        .map(|members| build_group(DupKind::Exact, members, 0))
        .collect()
}

/// Group visually-identical images by perceptual-hash proximity.
///
/// A BK-tree radius query per image plus union-find, rather than the O(n^2)
/// all-pairs comparison a naive implementation needs — at 100k photos that is
/// the difference between seconds and hours.
///
/// This takes the *transitive* closure: if A matches B and B matches C, all
/// three land in one group even when A and C are slightly beyond the threshold.
/// `spread` reports the widest pair so a stretched group is visible.
fn near_groups(rows: &[MediaRow], threshold: u32) -> Vec<DupGroup> {
    let indexed: Vec<(usize, u64)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.phash.map(|h| (i, h)))
        .collect();
    if indexed.len() < 2 {
        return Vec::new();
    }

    let mut tree = BkTree::new();
    for (slot, &(_, hash)) in indexed.iter().enumerate() {
        tree.insert(hash, slot);
    }

    let mut uf = UnionFind::new(indexed.len());
    for (slot, &(_, hash)) in indexed.iter().enumerate() {
        for other in tree.within(hash, threshold) {
            if other != slot {
                uf.union(slot, other);
            }
        }
    }

    let mut clusters: HashMap<usize, Vec<&MediaRow>> = HashMap::new();
    for (slot, &(row_idx, _)) in indexed.iter().enumerate() {
        let root = uf.find(slot);
        clusters.entry(root).or_default().push(&rows[row_idx]);
    }

    clusters
        .into_values()
        .filter(|members| members.len() > 1)
        // A cluster whose members are all byte-identical is already reported as
        // an exact group; repeating it as "near" would be pure noise.
        .filter(|members| !all_same_content_hash(members))
        .map(|members| {
            let spread = max_pairwise_distance(&members);
            build_group(DupKind::Near, members, spread)
        })
        .collect()
}

/// Group frames shot within `window` seconds of each other on the same camera.
fn burst_groups(rows: &[MediaRow], window: i64) -> Vec<DupGroup> {
    // Bucket by camera first: two cameras shooting at the same moment are not a
    // burst, and an unknown camera must not merge with a known one.
    let mut by_camera: HashMap<&str, Vec<&MediaRow>> = HashMap::new();
    for r in rows.iter().filter(|r| r.date_taken.is_some()) {
        let cam = r.camera_model.as_deref().unwrap_or("");
        by_camera.entry(cam).or_default().push(r);
    }

    // Deterministic output despite HashMap iteration order.
    let mut cameras: Vec<_> = by_camera.into_iter().collect();
    cameras.sort_by_key(|(cam, _)| *cam);

    let mut out = Vec::new();
    for (_cam, mut shots) in cameras {
        shots.sort_by(|a, b| {
            a.date_taken
                .cmp(&b.date_taken)
                .then_with(|| a.path.cmp(&b.path))
        });

        let mut run: Vec<&MediaRow> = Vec::new();
        for r in shots {
            let extends_run = match (run.last().and_then(|p| p.date_taken), r.date_taken) {
                (Some(prev), Some(cur)) => cur - prev <= window,
                _ => false,
            };
            if !extends_run {
                flush_burst(&mut run, &mut out);
            }
            run.push(r);
        }
        flush_burst(&mut run, &mut out);
    }
    out
}

fn flush_burst<'a>(run: &mut Vec<&'a MediaRow>, out: &mut Vec<DupGroup>) {
    let members = std::mem::take(run);
    if members.len() > 1 && !all_same_content_hash(&members) {
        out.push(build_group(DupKind::Burst, members, 0));
    }
}

/// True when every member has the same *known* content hash.
fn all_same_content_hash(members: &[&MediaRow]) -> bool {
    let mut hashes = members.iter().map(|m| m.content_hash.as_deref());
    match hashes.next() {
        Some(Some(first)) => hashes.all(|h| h == Some(first)),
        // No hash computed: cannot claim they are identical.
        _ => false,
    }
}

fn max_pairwise_distance(members: &[&MediaRow]) -> u32 {
    let hashes: Vec<u64> = members.iter().filter_map(|m| m.phash).collect();
    let mut max = 0;
    for (i, &a) in hashes.iter().enumerate() {
        for &b in &hashes[i + 1..] {
            max = max.max(hashing::hamming(a, b));
        }
    }
    max
}

fn build_group(kind: DupKind, members: Vec<&MediaRow>, spread: u32) -> DupGroup {
    let mut members: Vec<MediaRow> = members.into_iter().cloned().collect();
    members.sort_by(|a, b| a.path.cmp(&b.path));
    let keeper = suggest_keeper(&members);
    let reclaimable = members
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != keeper)
        .map(|(_, r)| r.size)
        .sum();
    DupGroup {
        kind,
        members,
        keeper,
        spread,
        reclaimable,
    }
}

/// Pick which copy to keep.
///
/// Highest resolution wins: that copy holds the most information and is the one
/// a shrunken duplicate was derived from. Ties fall to the larger file (less
/// compression), then the oldest mtime (the original rather than a later copy),
/// then the shortest path (originals tend not to live in `Copy of/New folder/…`),
/// then lexicographic order so the choice is deterministic.
fn suggest_keeper(members: &[MediaRow]) -> usize {
    let score = |r: &MediaRow| {
        let pixels = match (r.width, r.height) {
            (Some(w), Some(h)) => u64::from(w) * u64::from(h),
            _ => 0,
        };
        (
            std::cmp::Reverse(pixels),
            std::cmp::Reverse(r.size),
            r.mtime,
            r.path.as_os_str().len(),
            r.path.clone(),
        )
    };
    let mut best = 0;
    for i in 1..members.len() {
        if score(&members[i]) < score(&members[best]) {
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn row(path: &str, size: u64) -> MediaRow {
        MediaRow {
            path: PathBuf::from(path),
            file_name: path.rsplit('/').next().unwrap_or(path).to_string(),
            ext: "jpg".into(),
            size,
            mtime: 1000,
            ..MediaRow::default()
        }
    }

    fn with_hash(path: &str, size: u64, hash: &str) -> MediaRow {
        MediaRow {
            content_hash: Some(hash.into()),
            ..row(path, size)
        }
    }

    fn with_phash(path: &str, size: u64, phash: u64) -> MediaRow {
        MediaRow {
            phash: Some(phash),
            ..row(path, size)
        }
    }

    fn paths(g: &DupGroup) -> Vec<String> {
        g.members
            .iter()
            .map(|m| m.path.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn exact_groups_files_with_matching_content_hash() {
        let rows = vec![
            with_hash("/lib/a.jpg", 100, "h1"),
            with_hash("/lib/copy/a.jpg", 100, "h1"),
            with_hash("/lib/b.jpg", 200, "h2"),
        ];
        let rep = find_duplicates(&rows, &DupConfig::default());
        assert_eq!(rep.groups.len(), 1);
        let g = &rep.groups[0];
        assert_eq!(g.kind, DupKind::Exact);
        assert_eq!(paths(g), vec!["/lib/a.jpg", "/lib/copy/a.jpg"]);
        assert_eq!(g.reclaimable, 100, "one of the two is redundant");
        assert_eq!(rep.total_removable(), 1);
        assert_eq!(rep.total_reclaimable(), 100);
    }

    #[test]
    fn rows_without_a_content_hash_are_never_exact_duplicates() {
        // Unhashed rows must not be lumped together just because both are NULL.
        let rows = vec![row("/lib/a.jpg", 100), row("/lib/b.jpg", 100)];
        assert!(find_duplicates(&rows, &DupConfig::default()).is_empty());
    }

    #[test]
    fn near_groups_cluster_by_perceptual_distance() {
        let cfg = DupConfig {
            near_threshold: 4,
            ..DupConfig::default()
        };
        let rows = vec![
            with_phash("/lib/orig.jpg", 900, 0b0000),
            with_phash("/lib/shrunk.jpg", 90, 0b0011), // distance 2
            with_phash("/lib/other.jpg", 500, u64::MAX), // far away
        ];
        let rep = find_duplicates(&rows, &cfg);
        assert_eq!(rep.groups.len(), 1);
        let g = &rep.groups[0];
        assert_eq!(g.kind, DupKind::Near);
        assert_eq!(paths(g), vec!["/lib/orig.jpg", "/lib/shrunk.jpg"]);
        assert_eq!(g.spread, 2);
    }

    #[test]
    fn near_threshold_is_respected() {
        let rows = vec![
            with_phash("/lib/a.jpg", 100, 0b0000),
            with_phash("/lib/b.jpg", 100, 0b1111), // distance 4
        ];
        let tight = DupConfig {
            near_threshold: 3,
            ..DupConfig::default()
        };
        assert!(find_duplicates(&rows, &tight).is_empty());

        let loose = DupConfig {
            near_threshold: 4,
            ..DupConfig::default()
        };
        assert_eq!(find_duplicates(&rows, &loose).groups.len(), 1);
    }

    #[test]
    fn near_grouping_is_transitive_and_reports_spread() {
        // A~B and B~C within threshold, but A~C is 6 apart. All three cluster,
        // and `spread` surfaces that the group is wider than the threshold.
        let cfg = DupConfig {
            near_threshold: 3,
            ..DupConfig::default()
        };
        let rows = vec![
            with_phash("/lib/a.jpg", 100, 0b000_000),
            with_phash("/lib/b.jpg", 100, 0b000_111),
            with_phash("/lib/c.jpg", 100, 0b111_111),
        ];
        let rep = find_duplicates(&rows, &cfg);
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].members.len(), 3);
        assert_eq!(rep.groups[0].spread, 6);
    }

    #[test]
    fn near_group_that_is_only_exact_duplicates_is_suppressed() {
        // Identical files share a perceptual hash too; reporting them twice
        // would just be noise.
        let mut a = with_hash("/lib/a.jpg", 100, "h1");
        let mut b = with_hash("/lib/b.jpg", 100, "h1");
        a.phash = Some(0b1010);
        b.phash = Some(0b1010);
        let rep = find_duplicates(&[a, b], &DupConfig::default());
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].kind, DupKind::Exact);
    }

    #[test]
    fn near_group_spanning_distinct_files_is_reported() {
        // Same picture, different bytes: exactly what perceptual hashing is for.
        let mut a = with_hash("/lib/full.jpg", 900, "h1");
        let mut b = with_hash("/lib/small.jpg", 90, "h2");
        a.phash = Some(0b1010);
        b.phash = Some(0b1011);
        let rep = find_duplicates(&[a, b], &DupConfig::default());
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].kind, DupKind::Near);
    }

    #[test]
    fn burst_groups_consecutive_shots_from_one_camera() {
        let shot = |path: &str, t: i64, cam: &str| MediaRow {
            date_taken: Some(t),
            camera_model: Some(cam.into()),
            ..row(path, 100)
        };
        let rows = vec![
            shot("/lib/1.jpg", 1000, "R5"),
            shot("/lib/2.jpg", 1001, "R5"),
            shot("/lib/3.jpg", 1002, "R5"),
            shot("/lib/later.jpg", 3000, "R5"), // long after: its own run
            shot("/lib/other.jpg", 1000, "Pixel"), // same instant, other camera
        ];
        let cfg = DupConfig {
            find_exact: false,
            find_near: false,
            find_burst: true,
            burst_window_secs: 2,
            ..DupConfig::default()
        };
        let rep = find_duplicates(&rows, &cfg);
        assert_eq!(rep.groups.len(), 1, "one burst run");
        let g = &rep.groups[0];
        assert_eq!(g.kind, DupKind::Burst);
        assert_eq!(paths(g), vec!["/lib/1.jpg", "/lib/2.jpg", "/lib/3.jpg"]);
    }

    #[test]
    fn burst_window_boundary_splits_runs() {
        let shot = |path: &str, t: i64| MediaRow {
            date_taken: Some(t),
            camera_model: Some("R5".into()),
            ..row(path, 100)
        };
        let cfg = DupConfig {
            find_exact: false,
            find_near: false,
            find_burst: true,
            burst_window_secs: 2,
            ..DupConfig::default()
        };
        // Exactly at the window: still one burst.
        let rep = find_duplicates(&[shot("/a.jpg", 100), shot("/b.jpg", 102)], &cfg);
        assert_eq!(rep.groups.len(), 1);
        // One second past it: two singletons, so nothing is reported.
        let rep = find_duplicates(&[shot("/a.jpg", 100), shot("/b.jpg", 103)], &cfg);
        assert!(rep.is_empty());
    }

    #[test]
    fn burst_detection_is_off_by_default() {
        let shot = |path: &str, t: i64| MediaRow {
            date_taken: Some(t),
            camera_model: Some("R5".into()),
            ..row(path, 100)
        };
        let rows = vec![shot("/lib/1.jpg", 1000), shot("/lib/2.jpg", 1001)];
        assert!(find_duplicates(&rows, &DupConfig::default()).is_empty());
    }

    #[test]
    fn shots_without_capture_time_are_not_bursts() {
        let cfg = DupConfig {
            find_exact: false,
            find_near: false,
            find_burst: true,
            ..DupConfig::default()
        };
        let rows = vec![row("/lib/a.jpg", 100), row("/lib/b.jpg", 100)];
        assert!(find_duplicates(&rows, &cfg).is_empty());
    }

    #[test]
    fn keeper_prefers_highest_resolution() {
        let big = MediaRow {
            width: Some(6000),
            height: Some(4000),
            ..with_hash("/lib/z_big.jpg", 500, "h1")
        };
        let small = MediaRow {
            width: Some(800),
            height: Some(600),
            ..with_hash("/lib/a_small.jpg", 9000, "h1")
        };
        // The small image has the larger byte size and sorts first
        // alphabetically — resolution must still win.
        let rep = find_duplicates(&[small, big], &DupConfig::default());
        let g = &rep.groups[0];
        assert_eq!(g.keeper_row().file_name, "z_big.jpg");
        assert_eq!(g.reclaimable, 9000);
        assert_eq!(g.others().count(), 1);
    }

    #[test]
    fn keeper_falls_back_to_size_then_mtime_then_path() {
        // No dimensions known: the larger file wins.
        let rep = find_duplicates(
            &[
                with_hash("/lib/a.jpg", 100, "h"),
                with_hash("/lib/b.jpg", 900, "h"),
            ],
            &DupConfig::default(),
        );
        assert_eq!(rep.groups[0].keeper_row().file_name, "b.jpg");

        // Same size: the older file is the original.
        let older = MediaRow {
            mtime: 10,
            ..with_hash("/lib/old.jpg", 100, "h")
        };
        let newer = MediaRow {
            mtime: 99,
            ..with_hash("/lib/new.jpg", 100, "h")
        };
        let rep = find_duplicates(&[newer, older], &DupConfig::default());
        assert_eq!(rep.groups[0].keeper_row().file_name, "old.jpg");

        // Fully tied: the shortest path wins, so nested copies lose.
        let rep = find_duplicates(
            &[
                with_hash("/lib/copies/deep/a.jpg", 100, "h"),
                with_hash("/lib/a.jpg", 100, "h"),
            ],
            &DupConfig::default(),
        );
        assert_eq!(rep.groups[0].keeper_row().path, PathBuf::from("/lib/a.jpg"));
    }

    #[test]
    fn keeper_choice_is_independent_of_input_order() {
        let a = with_hash("/lib/a.jpg", 100, "h");
        let b = with_hash("/lib/b.jpg", 100, "h");
        let fwd = find_duplicates(&[a.clone(), b.clone()], &DupConfig::default());
        let rev = find_duplicates(&[b, a], &DupConfig::default());
        assert_eq!(
            fwd.groups[0].keeper_row().path,
            rev.groups[0].keeper_row().path
        );
    }

    #[test]
    fn groups_are_ordered_by_kind_then_reclaimable_bytes() {
        let mut near_a = with_hash("/lib/n1.jpg", 10, "x1");
        let mut near_b = with_hash("/lib/n2.jpg", 10, "x2");
        near_a.phash = Some(0);
        near_b.phash = Some(1);

        let rows = vec![
            with_hash("/lib/e1.jpg", 5_000, "same"),
            with_hash("/lib/e2.jpg", 5_000, "same"),
            near_a,
            near_b,
        ];
        let rep = find_duplicates(&rows, &DupConfig::default());
        assert_eq!(rep.groups.len(), 2);
        assert_eq!(rep.groups[0].kind, DupKind::Exact, "strongest evidence first");
        assert_eq!(rep.groups[1].kind, DupKind::Near);
        assert_eq!(rep.count_of(DupKind::Exact), 1);
        assert_eq!(rep.count_of(DupKind::Near), 1);
    }

    #[test]
    fn empty_and_single_inputs_produce_nothing() {
        assert!(find_duplicates(&[], &DupConfig::default()).is_empty());
        let one = vec![with_hash("/lib/a.jpg", 1, "h")];
        assert!(find_duplicates(&one, &DupConfig::default()).is_empty());
    }

    #[test]
    fn three_way_exact_group_reclaims_all_but_one() {
        let rows = vec![
            with_hash("/lib/a.jpg", 100, "h"),
            with_hash("/lib/b.jpg", 100, "h"),
            with_hash("/lib/c.jpg", 100, "h"),
        ];
        let rep = find_duplicates(&rows, &DupConfig::default());
        assert_eq!(rep.groups[0].members.len(), 3);
        assert_eq!(rep.groups[0].reclaimable, 200);
        assert_eq!(rep.total_removable(), 2);
    }

    #[test]
    fn saved_images_do_not_group_against_photographs() {
        use crate::kind::MediaKind;
        // Byte-identical, but one is a photo you took and one is a reference
        // image you saved. You want to keep both, so this is not a duplicate.
        let photo = MediaRow {
            camera_model: Some("R5".into()),
            date_taken: Some(1_600_000_000),
            ..with_hash("/lib/IMG_1.jpg", 100, "same")
        };
        let saved = MediaRow {
            kind_override: Some(MediaKind::Saved),
            ..with_hash("/lib/inspo/ref.jpg", 100, "same")
        };

        let rep = find_duplicates(&[photo.clone(), saved.clone()], &DupConfig::default());
        assert!(
            rep.is_empty(),
            "different kinds must not group: {:?}",
            rep.groups.len()
        );

        // Turning the scoping off puts them back together, proving the two are
        // otherwise a perfectly good match.
        let mixed = DupConfig {
            same_kind_only: false,
            ..DupConfig::default()
        };
        assert_eq!(find_duplicates(&[photo, saved], &mixed).groups.len(), 1);
    }

    #[test]
    fn duplicates_within_one_kind_are_still_found() {
        use crate::kind::MediaKind;
        let a = MediaRow {
            kind_override: Some(MediaKind::Saved),
            ..with_hash("/lib/a.jpg", 100, "same")
        };
        let b = MediaRow {
            kind_override: Some(MediaKind::Saved),
            ..with_hash("/lib/b.jpg", 100, "same")
        };
        let rep = find_duplicates(&[a, b], &DupConfig::default());
        assert_eq!(rep.groups.len(), 1);
        assert_eq!(rep.groups[0].members.len(), 2);
    }

    #[test]
    fn kind_scoping_still_finds_groups_in_several_kinds_at_once() {
        use crate::kind::MediaKind;
        let saved = |p: &str| MediaRow {
            kind_override: Some(MediaKind::Saved),
            ..with_hash(p, 10, "s")
        };
        let shot = |p: &str| MediaRow {
            kind_override: Some(MediaKind::Photo),
            ..with_hash(p, 5000, "p")
        };
        let rows = vec![
            saved("/lib/s1.jpg"),
            saved("/lib/s2.jpg"),
            shot("/lib/p1.jpg"),
            shot("/lib/p2.jpg"),
        ];
        let rep = find_duplicates(&rows, &DupConfig::default());
        assert_eq!(rep.groups.len(), 2, "one group per kind");
        // Ordering is still by reclaimable bytes, so the photos come first.
        assert_eq!(rep.groups[0].reclaimable, 5000);
        assert_eq!(rep.groups[1].reclaimable, 10);
    }

    // UnionFind itself is exercised directly in src/union_find.rs; the near-
    // duplicate tests above already cover it end-to-end through this module.
}
