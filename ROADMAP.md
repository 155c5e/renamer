# Roadmap

Planned features, with the design decisions that need making before each one
starts. Not commitments, and not in priority order — except that the two
cross-cutting items below are worth doing early, because both get much more
expensive later.

## Cross-cutting

### User metadata must survive renames and moves

**Blocks ratings, pins, and the parent filter.**

The catalog keys `media` rows on absolute path, and the Rename tab moves files
with `std::fs::rename` without telling the catalog. On the next index the old
path is marked missing and the new path is inserted as a fresh row with a new
id.

Today that costs nothing, because every column is derived from the file itself
and is simply recomputed. The moment a row carries something only the *user* can
supply — a rating, a "pin" flag, a hidden flag — renaming a photo silently
destroys it. Worse, it destroys it quietly: the file is fine, the app just
forgets.

Two ways out:

1. Key user-assigned metadata on **content hash** rather than path, in a
   separate table. Survives renames, moves, and re-imports. Costs a hash for
   every file rather than only size-colliding ones.
2. Have apply/undo **update catalog paths** as part of the rename batch.
   Cheaper, but only covers moves this app performs — a move in Finder or
   Nautilus still loses the metadata.

Probably both: (2) for correctness during our own renames, (1) as the durable
fallback.

### Fold new per-image analysis into the existing decode pass

Stage 4 of the indexer decodes each image exactly once and derives the
perceptual hash and the thumbnail from that single decode. Anything else
per-pixel — colour palettes, a larger working image for face detection — should
be added *while that stage is being touched*.

Adding a new decode pass later means re-reading and re-decoding the entire
library, which is hours on a large one. Adding it to the existing pass is close
to free.

---

## Features

### Face identification and people clustering

Detect faces, embed them, cluster into people, name the clusters.

- **Pipeline**: SCRFD (detect) → align → ArcFace (512-D embedding) → cluster.
  Same person is roughly cosine similarity > 0.35–0.4.
- **Runtime**: prefer [`tract`](https://lib.rs/crates/tract-onnx) (pure Rust)
  over [`ort`](https://github.com/pykeio/ort) (ONNX Runtime). `ort` is faster
  and supports CoreML/CUDA, but links a native C++ library, which breaks this
  project's "download one binary and run it" distribution. Indexing is batch
  work, so a 2–3× slower pure-Rust runtime is an easy trade.
- Small models (SCRFD-500m ~2.5 MB, a MobileFaceNet-class recognizer ~5–17 MB)
  are small enough to `include_bytes!` into the binary — no download step, fully
  offline.
- **Needs a ~1024px working image** from the decode pass. The 256px thumbnails
  are too small to detect faces reliably. See the cross-cutting note above.
- **Clustering reuses the duplicates shape**: pairwise similarity plus
  `UnionFind`, which already exists. The BK-tree does not carry over — it needs
  discrete distances — but brute-force cosine over ~150k faces is milliseconds,
  so no vector index is needed at this scale.
- **Storage**: 512 floats = 2 KB per face; ~150k faces ≈ 300 MB. Quantising to
  int8 cuts that 4×.
- **Licensing**: InsightFace's code is MIT but its pretrained packs
  (`buffalo_*`, `antelopev2`) are non-commercial research only. Fine for a
  personal library; a blocker for anything shipped commercially.
- **Expect imperfection**: good on adults facing the camera, weak on profiles,
  small faces, and children as they age. The same person will sometimes land in
  two or three clusters, so merge/split/rename UI is mandatory — and is usually
  more work than the inference itself.

### Map navigation for geotagged photos

- EXIF GPS tags are already available through `kamadak-exif`; nothing reads them
  yet. Extract `GPSLatitude`/`GPSLongitude` (plus the N/S/E/W refs), convert the
  rational degree-minute-second values to decimal degrees, and store as
  `lat`/`lon` columns.
- **Map widget**: [`walkers`](https://lib.rs/crates/walkers) is the natural fit
  for egui, but it fetches OSM tiles, so the map view needs network and should
  respect OSM's tile usage policy. A no-basemap scatter of coordinates is the
  zero-dependency fallback.
- **Offline reverse geocoding** (a bundled GeoNames-style city dump is only a
  few MB) is arguably more valuable than the map itself: it gives "Paris",
  "Tokyo" grouping without any network — and it unlocks `{city}` / `{country}`
  as **renamer template fields**, which composes nicely with the existing
  `{date_taken:%Y}/{city}/{name}` style of template.

### Separate pinned / saved images from own photos

Moodboards, inspo, screenshots and saved reference images are not photographs
you took, and mixing them distorts everything else.

- A `kind` column (photo / saved / screenshot) with a **manual override**,
  seeded by a heuristic: no EXIF camera fields, screenshot-shaped dimensions,
  no GPS, web-style filenames.
- Should **scope duplicate detection**, so a saved reference image never groups
  against a real photo.
- Interacts with the parent filter and slideshow — both usually want photos
  only by default.

### Star rating system

- 0–5 stars, keyboard-driven (`0`–`5`), visible in the grid and slideshow.
- **Decision needed**: catalog-only, or exported to XMP sidecars?
  Catalog-only is consistent with the promise never to write into picture
  folders, but the ratings die with the catalog and are invisible to Lightroom,
  digiKam and friends. Suggested: catalog is authoritative, with an explicit
  opt-in export.
- Depends on the user-metadata durability work above.

### Parent filter

A toggle that hides manually tagged images while showing photos to other people.

- A `hidden` flag plus a global mode toggle.
- **Apply it as a single predicate in the catalog query layer, never per view.**
  If each of the grid, slideshow, duplicates and search filters independently,
  one of them will eventually forget, and the failure mode is showing the exact
  thing you meant to hide.
- Be honest in the UI that this is a **display filter, not security** — the
  files are still on disk, unencrypted, under their real names. A genuine
  private vault is a different, larger feature.
- Decide whether the filter is on by default at launch (safer) or remembers its
  last state (more convenient). Safer probably wins.

### Slideshow mode

- Fullscreen, configurable interval, pause/resume, manual next/previous.
- Ordering: date, random, or rating.
- Must honour the parent filter and any rating/kind filters.
- Preload the next full-res image while displaying the current one; the existing
  thumbnail cache covers the filmstrip.

### Colour sort

- Extract a dominant-colour **palette** via k-means over downsampled pixels —
  a flat average colour turns every image into the same muddy brown.
- Sort by hue (HSL) for a rainbow arrangement; offer saturation and lightness
  as secondary sorts.
- **Compute it in the existing decode pass** — see the cross-cutting note. This
  is the cheapest possible time to add it and an expensive one to add later.
