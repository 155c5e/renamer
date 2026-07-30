# Face recognition models

Face identification needs two ONNX models that are **not** in this repository:

| Role | Model | Rough size |
|------|-------|-----------|
| Detection | SCRFD (e.g. `scrfd_500m_bnkps`) | ~2.5 MB |
| Embedding | ArcFace / MobileFaceNet-class recognizer | ~5–17 MB |

Drop them here and the app picks them up:

```
~/.local/share/renamer/models/detect.onnx      # Linux
~/.local/share/renamer/models/embed.onnx
~/Library/Application Support/renamer/models/  # macOS
```

(`RENAMER_DATA_DIR` moves this, like everything else under the data directory.)

## Why they aren't vendored

**Licensing.** InsightFace's code is MIT, but its pretrained packs — `buffalo_l`,
`buffalo_s`, `antelopev2` — are released for **non-commercial research use only**.
That is fine for organizing your own photos, and not fine to redistribute inside
a public repository. Committing them here would relicense someone else's weights
by accident.

If this ever ships as anything commercial, you need either a commercial licence
from InsightFace or a permissively-licensed model.

**Size and provenance.** Even the small models are megabytes of opaque binary. A
photo organizer should not carry those in its git history, and a build that
downloads weights from a third party at compile time is not a build you can
reproduce or audit.

## Inference runtime

`tract-onnx`, not `ort`.

`ort` wraps Microsoft's ONNX Runtime and is faster, with CoreML and CUDA
backends. It also links a native C++ library, which would mean shipping a
`.so`/`.dylib` next to the binary — and this project's whole distribution story
is "download one file, `chmod +x`, run it".

Indexing is batch work done once per photo and cached, so a pure-Rust runtime
being a few times slower costs a slower first index and nothing else. That is a
good trade for keeping the single binary.

## Clustering engine

The part that groups embeddings into people (`src/faces.rs`) is already built
and fully tested — with synthetic vectors, since it has no dependency on
`tract-onnx` or on real model output. It does the same job the near-duplicate
detector does for images: single-linkage clustering via cosine similarity and
the same `UnionFind` (now shared between the two in `src/union_find.rs`),
with the same trade-off — a chain of similar-enough faces can join into one
cluster even if the two ends of the chain do not resemble each other. Treat a
cluster as a suggestion, same as a near-duplicate group: merge, split and
rename are mandatory, not optional polish.

One difference from duplicate detection: every face lands in some cluster,
including a cluster of one. A person seen in a single photo is still worth
naming, so there is no size-1 filter the way there is for duplicates.

`cluster_faces` is O(n^2) in the number of faces — there is no BK-tree
equivalent for cosine similarity over a continuous, high-dimensional vector
the way there is for a 64-bit perceptual hash's Hamming distance. Fine at a
few thousand faces; would need an approximate nearest-neighbour index at tens
of thousands. Not worth building before real usage proves it necessary.

What is still missing, in order:

1. **Inference** — decode SCRFD to find face boxes, run the recognizer on each
   crop, produce a `DetectedFace` per face. Needs the model files above.
2. **Catalog storage** — `faces` and `people` tables. Not added yet because
   there is nothing to populate them until step 1 exists.
3. **UI** — browse people, rename a cluster, merge two, split a bad one out.

## Expectations

Face recognition on a personal library works well on adults facing the camera,
and poorly on profiles, small or blurred faces, and children as they age. The
same person will land in two or three clusters sometimes.

That means merge/split/rename UI is not optional, and it is usually more work
than the inference itself. Plan for the clustering to be *reviewed*, never
trusted blindly — the same posture as near-duplicate matches.
