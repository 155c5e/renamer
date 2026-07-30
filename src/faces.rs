//! Face clustering: turn face embeddings into people.
//!
//! This module is deliberately independent of any inference runtime. Detection
//! and embedding (SCRFD + an ArcFace-family recognizer, run through
//! `tract-onnx`) will eventually produce a [`DetectedFace`] per face found in
//! an image; nothing here cares how that vector was produced. That split means
//! the clustering logic — the part with actual decisions to get right — can be
//! developed and tested with synthetic vectors today, with no model files and
//! no ONNX graph, months before the inference side is wired up. See
//! `docs/face-models.md` for why the model files themselves are not vendored.
//!
//! Clustering is single-linkage over cosine similarity: two faces join the same
//! cluster if a chain of pairwise similarities above `threshold` connects them,
//! even if the endpoints of that chain are not directly similar. This is the
//! same trade-off the near-duplicate detector makes, for the same reason — it
//! is simple, cheap, and it is what a `UnionFind` naturally gives you.
//! It also means a cluster can drift: A resembles B, B resembles C, but A and C
//! look like different people to a human. As with near-duplicates and bursts,
//! the answer is review, not blind trust — clusters are suggestions, and the
//! caller must offer merge/split/rename, never a silent auto-apply.
//!
//! Unlike duplicate groups, every face ends up in *some* cluster — including a
//! cluster of one. A person who appears in a single photo is still a person
//! worth naming; there is no size-1 filter here the way there is for
//! duplicates.
//!
//! Nothing outside this module's own tests calls into it yet: there is no
//! inference pass producing real `DetectedFace` values, and no UI. The
//! `#![allow(dead_code)]` below reflects that honestly rather than papering
//! over it item by item; it should come off once the indexer or app wires
//! this in.
#![allow(dead_code)]

use crate::union_find::UnionFind;

/// A face found in an image, wherever it came from.
///
/// Deliberately not tied to the catalog: nothing here requires the `faces` and
/// `people` tables that will eventually store these (out of scope until
/// inference exists to populate them). `media_id` is left as a plain `i64` so
/// the caller can key it to a `catalog::MediaRow` however it likes.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedFace {
    pub media_id: i64,
    /// Index of this face within its image, for photos with more than one.
    pub face_index: usize,
    pub bbox: BBox,
    /// Embedding vector from the recognizer — typically 512-D for an ArcFace
    /// model, but nothing here assumes a specific length. Need not be
    /// pre-normalized; [`cosine_similarity`] normalizes defensively.
    pub embedding: Vec<f32>,
}

/// Bounding box in pixels of the image that was analysed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Tuning for a clustering pass.
#[derive(Debug, Clone, Copy)]
pub struct ClusterConfig {
    /// Minimum cosine similarity for two faces to count as the same person.
    ///
    /// ArcFace-family embeddings typically separate same/different identity
    /// somewhere around 0.4, but the right default here leans conservative:
    /// merging two different people into one cluster is a worse mistake than
    /// splitting one person into two, because a wrong merge can go unnoticed
    /// while a split is a single click to fix once you spot it.
    pub similarity_threshold: f32,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.5,
        }
    }
}

/// One cluster of faces believed to be the same person.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersonCluster {
    /// Indices into the caller's `&[DetectedFace]` slice, ascending.
    pub members: Vec<usize>,
    /// Mean of the members' normalized embeddings, itself re-normalized.
    /// The comparison point for adding a face without re-clustering
    /// everything — see [`assign_to_nearest`].
    pub centroid: Vec<f32>,
}

impl PersonCluster {
    /// Member closest to the centroid: the natural "cover photo" for this
    /// person, being the most representative face on hand.
    pub fn representative(&self, faces: &[DetectedFace]) -> Option<usize> {
        self.members
            .iter()
            .copied()
            .max_by(|&a, &b| {
                let sa = cosine_similarity(&faces[a].embedding, &self.centroid);
                let sb = cosine_similarity(&faces[b].embedding, &self.centroid);
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            })
    }
}

/// Cosine similarity in `[-1, 1]`, or `0.0` for degenerate input (a zero vector,
/// or mismatched lengths) rather than panicking or returning NaN.
///
/// Normalizes both vectors internally, so callers may pass raw model output —
/// there is no requirement that embeddings arrive pre-normalized.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

/// L2-normalize `v` in place. A zero vector is left as-is rather than
/// dividing by zero.
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn mean_embedding(faces: &[DetectedFace], members: &[usize]) -> Vec<f32> {
    let dim = members
        .first()
        .map(|&i| faces[i].embedding.len())
        .unwrap_or(0);
    let mut sum = vec![0f32; dim];
    for &i in members {
        for (s, v) in sum.iter_mut().zip(&faces[i].embedding) {
            *s += v;
        }
    }
    let n = members.len().max(1) as f32;
    for s in sum.iter_mut() {
        *s /= n;
    }
    normalize(&mut sum);
    sum
}

/// Cluster every face in `faces` into people.
///
/// Every input index appears in exactly one output cluster — this is a
/// partition, not a filter. A face with no close match becomes a cluster of
/// one, because a person seen once is still a person.
///
/// This is an all-pairs comparison, O(n^2) in the number of faces. Perceptual
/// hashes get a BK-tree because Hamming distance over a 64-bit hash supports
/// exact radius queries with pruning; cosine similarity over a continuous,
/// high-dimensional embedding has no equivalent exact structure this simple.
/// At a few thousand faces that is milliseconds; at the tens of thousands a
/// large library's face count could reach, it becomes the slow part of
/// indexing. Approximate nearest-neighbour search (locality-sensitive hashing,
/// or a proper ANN index) is the fix if that ever bites, but is not worth the
/// complexity before real usage proves it necessary.
pub fn cluster_faces(faces: &[DetectedFace], cfg: &ClusterConfig) -> Vec<PersonCluster> {
    let mut uf = UnionFind::new(faces.len());
    for i in 0..faces.len() {
        for j in (i + 1)..faces.len() {
            if cosine_similarity(&faces[i].embedding, &faces[j].embedding)
                >= cfg.similarity_threshold
            {
                uf.union(i, j);
            }
        }
    }

    let mut by_root: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for i in 0..faces.len() {
        by_root.entry(uf.find(i)).or_default().push(i);
    }

    // Deterministic order: by each cluster's smallest member index, rather than
    // HashMap iteration order.
    let mut clusters: Vec<Vec<usize>> = by_root.into_values().collect();
    clusters.sort_by_key(|members| members[0]);

    clusters
        .into_iter()
        .map(|members| {
            let centroid = mean_embedding(faces, &members);
            PersonCluster { members, centroid }
        })
        .collect()
}

/// Which existing cluster a new face belongs to, without re-clustering
/// everything already assigned.
///
/// Mirrors the indexer's own approach to growth: re-examining every face on
/// every scan does not scale, so a face is only compared against the existing
/// clusters' centroids. Returns `None` when nothing is close enough, meaning
/// the caller should start a new cluster of one.
///
/// This is an approximation of re-running [`cluster_faces`] from scratch: a
/// face here compares only against centroids, not every individual member of
/// every cluster, so it can miss a match that full reclustering would find
/// (particularly in a cluster with wide internal spread). Good enough for
/// incremental indexing; a full recluster remains the source of truth.
pub fn assign_to_nearest(
    embedding: &[f32],
    clusters: &[PersonCluster],
    cfg: &ClusterConfig,
) -> Option<usize> {
    clusters
        .iter()
        .enumerate()
        .map(|(i, c)| (i, cosine_similarity(embedding, &c.centroid)))
        .filter(|(_, sim)| *sim >= cfg.similarity_threshold)
        .max_by(|(ia, sa), (ib, sb)| {
            sa.partial_cmp(sb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(ib.cmp(ia)) // tie-break to the lower cluster index
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(media_id: i64, embedding: Vec<f32>) -> DetectedFace {
        DetectedFace {
            media_id,
            face_index: 0,
            bbox: BBox { x: 0, y: 0, w: 10, h: 10 },
            embedding,
        }
    }

    /// A 4-D toy embedding space is enough to test clustering behaviour
    /// without needing real 512-D model output.
    fn near(base: [f32; 4], jitter: f32) -> Vec<f32> {
        base.iter().map(|v| v + jitter).collect()
    }

    #[test]
    fn cosine_similarity_matches_known_geometry() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) - (-1.0)).abs() < 1e-6);
        // Magnitude must not matter, only direction.
        assert!((cosine_similarity(&[1.0, 0.0], &[50.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_handles_degenerate_input_without_panicking() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[0.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn normalize_produces_unit_length_and_leaves_zero_alone() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
        assert!((len - 1.0).abs() < 1e-6);

        let mut zero = vec![0.0, 0.0];
        normalize(&mut zero);
        assert_eq!(zero, vec![0.0, 0.0]);
    }

    #[test]
    fn similar_embeddings_cluster_together() {
        let base = [1.0, 0.2, -0.3, 0.5];
        let faces = vec![
            face(1, near(base, 0.0)),
            face(2, near(base, 0.01)),
            face(3, near(base, -0.01)),
        ];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members, vec![0, 1, 2]);
    }

    #[test]
    fn dissimilar_embeddings_stay_apart() {
        let faces = vec![
            face(1, vec![1.0, 0.0, 0.0, 0.0]),
            face(2, vec![0.0, 1.0, 0.0, 0.0]),
        ];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].members, vec![0]);
        assert_eq!(clusters[1].members, vec![1]);
    }

    #[test]
    fn every_face_lands_somewhere_even_alone() {
        // Unlike duplicate groups, singletons are not dropped: a person seen
        // once is still a person.
        let faces = vec![face(1, vec![1.0, 0.0]), face(2, vec![0.0, 1.0])];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        let total: usize = clusters.iter().map(|c| c.members.len()).sum();
        assert_eq!(total, faces.len());
    }

    #[test]
    fn threshold_boundary_is_inclusive() {
        // Two orthogonal unit-ish vectors nudged to a known similarity.
        let a = vec![1.0, 0.0];
        let b = vec![0.5f32.sqrt(), 0.5f32.sqrt()]; // cos(45 deg) ~= 0.7071
        let sim = cosine_similarity(&a, &b);

        let cfg_at = ClusterConfig {
            similarity_threshold: sim,
        };
        let faces = vec![face(1, a.clone()), face(2, b.clone())];
        assert_eq!(cluster_faces(&faces, &cfg_at).len(), 1, "== threshold joins");

        let cfg_above = ClusterConfig {
            similarity_threshold: sim + 0.001,
        };
        assert_eq!(
            cluster_faces(&faces, &cfg_above).len(),
            2,
            "just above threshold must not join"
        );
    }

    #[test]
    fn clustering_is_transitive_across_a_chain() {
        // A~B and B~C within threshold, A~C is not, directly. All three still
        // group, the same trade-off near-duplicate grouping makes.
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.7, 0.7, 0.0];
        let c = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b) >= 0.5);
        assert!(cosine_similarity(&b, &c) >= 0.5);
        assert!(cosine_similarity(&a, &c) < 0.5, "endpoints must not be directly close");

        let faces = vec![face(1, a), face(2, b), face(3, c)];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 3);
    }

    #[test]
    fn multiple_faces_in_one_photo_are_not_forced_together() {
        // Two different people photographed side by side must not merge just
        // because they share a media_id.
        let faces = vec![
            face(42, vec![1.0, 0.0, 0.0]),
            face(42, vec![0.0, 1.0, 0.0]),
        ];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn centroid_is_the_normalized_mean() {
        let faces = vec![
            face(1, vec![1.0, 0.0]),
            face(2, vec![1.0, 0.0]),
            face(3, vec![0.98, 0.02]), // close enough to join at default threshold
        ];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), 1);
        let c = &clusters[0].centroid;
        let len = (c[0] * c[0] + c[1] * c[1]).sqrt();
        assert!((len - 1.0).abs() < 1e-5, "centroid must be unit length");
        assert!(c[0] > c[1], "centroid should lean toward the dominant direction");
    }

    #[test]
    fn representative_is_the_member_closest_to_the_centroid() {
        let faces = vec![
            face(1, vec![1.0, 0.0]),
            face(2, vec![0.9, 0.1]), // slightly off-centroid
            face(3, vec![1.0, 0.01]), // closest to the average direction
        ];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), 1);
        let rep = clusters[0].representative(&faces).unwrap();
        assert_eq!(rep, 2, "index 2 (face 3) sits closest to the cluster centroid");
    }

    #[test]
    fn representative_of_empty_cluster_is_none() {
        let empty = PersonCluster::default();
        assert_eq!(empty.representative(&[]), None);
    }

    #[test]
    fn clustering_empty_input_yields_no_clusters() {
        assert!(cluster_faces(&[], &ClusterConfig::default()).is_empty());
    }

    #[test]
    fn single_face_forms_its_own_cluster() {
        let faces = vec![face(1, vec![1.0, 0.0, 0.0])];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members, vec![0]);
    }

    #[test]
    fn cluster_output_is_ordered_by_first_member_index() {
        let faces = vec![
            face(1, vec![1.0, 0.0, 0.0]),
            face(2, vec![0.0, 1.0, 0.0]),
            face(3, vec![0.0, 0.0, 1.0]),
        ];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        let starts: Vec<usize> = clusters.iter().map(|c| c.members[0]).collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted, "clusters are ordered by ascending first member");
    }

    #[test]
    fn grouping_is_independent_of_scan_order() {
        // The same three faces, discovered in a different order (as would
        // happen if the filesystem walk visited files in another sequence).
        // Regardless of index shuffling, each face must end up grouped with the
        // same real-world identities as before.
        let a = face(1, vec![1.0, 0.0, 0.0]);
        let b = face(2, vec![0.9, 0.1, 0.0]); // close to `a`
        let c = face(3, vec![0.0, 0.0, 1.0]); // far from both

        let group_of = |clusters: &[PersonCluster], faces: &[DetectedFace], media_id: i64| {
            clusters
                .iter()
                .position(|cl| {
                    cl.members
                        .iter()
                        .any(|&i| faces[i].media_id == media_id)
                })
                .unwrap()
        };

        let forward = vec![a.clone(), b.clone(), c.clone()];
        let reversed = vec![c.clone(), b.clone(), a.clone()];

        let cf = cluster_faces(&forward, &ClusterConfig::default());
        let cr = cluster_faces(&reversed, &ClusterConfig::default());

        // `a` and `b` share a cluster in both orderings...
        assert_eq!(group_of(&cf, &forward, 1), group_of(&cf, &forward, 2));
        assert_eq!(group_of(&cr, &reversed, 1), group_of(&cr, &reversed, 2));
        // ...and `c` is never pulled in with them, in either ordering.
        assert_ne!(group_of(&cf, &forward, 1), group_of(&cf, &forward, 3));
        assert_ne!(group_of(&cr, &reversed, 1), group_of(&cr, &reversed, 3));
    }

    #[test]
    fn assign_to_nearest_finds_the_closest_cluster_above_threshold() {
        let faces = vec![
            face(1, vec![1.0, 0.0, 0.0]),
            face(2, vec![0.0, 1.0, 0.0]),
        ];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), 2);

        // A new face close to the first cluster's direction.
        let got = assign_to_nearest(&[0.99, 0.01, 0.0], &clusters, &ClusterConfig::default());
        assert_eq!(got, Some(0));
    }

    #[test]
    fn assign_to_nearest_returns_none_when_nothing_is_close_enough() {
        let faces = vec![face(1, vec![1.0, 0.0, 0.0])];
        let clusters = cluster_faces(&faces, &ClusterConfig::default());

        let got = assign_to_nearest(&[0.0, 1.0, 0.0], &clusters, &ClusterConfig::default());
        assert_eq!(got, None, "orthogonal to the only cluster: needs a new one");
    }

    #[test]
    fn assign_to_nearest_on_no_clusters_is_none() {
        assert_eq!(
            assign_to_nearest(&[1.0, 0.0], &[], &ClusterConfig::default()),
            None
        );
    }

    #[test]
    fn moderate_scale_clustering_completes_and_is_correct() {
        // 300 faces across 6 well-separated identities in 16-D space: enough
        // to exercise the O(n^2) path for real without making the test suite
        // slow (300^2 = 90,000 sixteen-float comparisons is microseconds).
        const DIM: usize = 16;
        const PEOPLE: usize = 6;
        const PER_PERSON: usize = 50;

        let mut identities = Vec::new();
        for p in 0..PEOPLE {
            let mut v = vec![0.0f32; DIM];
            v[p * (DIM / PEOPLE)] = 10.0; // near-orthogonal basis vectors
            identities.push(v);
        }

        let mut faces = Vec::new();
        let mut expected_person_of = Vec::new();
        // Deterministic pseudo-jitter so the test has no external RNG dependency.
        let mut state: u64 = 0xC0FF_EE00_D15E_A5E5;
        for p in 0..PEOPLE {
            for _ in 0..PER_PERSON {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let jitter = ((state % 1000) as f32 / 1000.0 - 0.5) * 0.05;
                let emb: Vec<f32> = identities[p].iter().map(|v| v + jitter).collect();
                faces.push(face(p as i64, emb));
                expected_person_of.push(p);
            }
        }

        let clusters = cluster_faces(&faces, &ClusterConfig::default());
        assert_eq!(clusters.len(), PEOPLE, "each identity forms its own cluster");

        // Every member of a cluster must trace back to the same synthetic
        // identity; no cluster may mix two people.
        for cluster in &clusters {
            let people: std::collections::HashSet<usize> = cluster
                .members
                .iter()
                .map(|&i| expected_person_of[i])
                .collect();
            assert_eq!(people.len(), 1, "cluster must not mix identities");
        }
        let total: usize = clusters.iter().map(|c| c.members.len()).sum();
        assert_eq!(total, faces.len());
    }
}
