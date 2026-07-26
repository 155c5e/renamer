# Renamer

Photo library organizer. GUI frontend (egui), Linux + macOS.

Three tools over one catalog:

- **Library** — index a folder once; metadata, hashes and thumbnails are cached
  so later scans only touch files that changed.
- **Duplicates** — find identical files, near-duplicates (the same picture at a
  different size or quality), and burst runs. Removal is reversible.
- **Rename** — the metadata-driven bulk renamer, with local and pCloud modes.

## Download

Prebuilt binaries — no Rust needed. All releases:
<https://github.com/155c5e/renamer/releases>

| Platform | Latest download |
|----------|-----------------|
| Linux (x86_64) | [renamer-linux-x86_64](https://github.com/155c5e/renamer/releases/latest/download/renamer-linux-x86_64) |
| macOS (Apple Silicon) | [renamer-macos-arm64](https://github.com/155c5e/renamer/releases/latest/download/renamer-macos-arm64) |

After downloading:

```sh
# Linux
chmod +x renamer-linux-x86_64 && ./renamer-linux-x86_64

# macOS (strip Gatekeeper quarantine on the unsigned binary, once)
chmod +x renamer-macos-arm64
xattr -d com.apple.quarantine renamer-macos-arm64
./renamer-macos-arm64
```

## Build / run from source

```sh
cargo run --release
```

SQLite is compiled in (`rusqlite` with the `bundled` feature), so there is no
system database to install.

---

# Library

Pick a **Library folder** and click **Index library**. Indexing runs in stages,
cheapest first, so expensive work only happens where it is needed:

1. **Scan** — stat every file. Anything whose size and modified time match the
   catalog is skipped entirely: no EXIF read, no hash, no decode. This is what
   makes re-indexing an unchanged library nearly free.
2. **Reconcile** — files that disappeared are marked missing.
3. **Hash** — BLAKE3 content hashes, but *only* for files that share a size with
   another file. A file with a unique size cannot be a byte-duplicate of
   anything, so on a typical library most files are never read at all.
4. **Analyse** — decode each image once to produce both a perceptual hash and a
   thumbnail. The slow stage on a first run; cached forever after.

| Option | Effect |
|--------|--------|
| Recurse subfolders | Descend into subdirectories |
| Images only | Ignore files that are not recognizable images |
| Read EXIF | Capture date, camera, lens. Reads file contents |
| Analyse images | Perceptual hashes + thumbnails. Required for near-duplicate detection |

## Nothing is written into your picture folders

No sidecar files, no database, no `.thumbnails` directories. Everything the app
produces lives under the standard per-user locations:

| | Linux | macOS |
|---|---|---|
| Catalog | `~/.local/share/renamer/catalog.sqlite` | `~/Library/Application Support/renamer/catalog.sqlite` |
| Thumbnails | `~/.cache/renamer/thumbs/` | `~/Library/Caches/renamer/thumbs/` |
| Quarantine | `~/.local/share/renamer/quarantine/` | `~/Library/Application Support/renamer/quarantine/` |

`RENAMER_DATA_DIR` and `RENAMER_CACHE_DIR` override the data and cache roots.
Pointing the data dir at the same disk as a large library makes quarantining an
instant rename instead of a copy.

The thumbnail cache is purely regenerable and safe to delete at any time (there
is a button for it). The catalog and quarantine are not — they hold your index
and your not-yet-deleted files.

---

# Duplicates

Three detectors, because "duplicate" means three different things in a real
library. Each can be toggled independently.

| Kind | Finds | Confidence |
|------|-------|------------|
| **Identical** | Byte-for-byte copies, via content hash | Provable, no false positives |
| **Near-duplicate** | The same picture resized, recompressed, or converted | High, but review |
| **Burst** | Frames shot within N seconds on the same camera | A heuristic about intent |

**Similarity tolerance** is the maximum perceptual-hash distance (in bits) that
still counts as the same picture. `0` is identical after downscaling, `6` (the
default) tolerates heavy recompression, and past roughly `12` merely similar
photos start matching. Near-duplicate grouping is transitive — if A matches B and
B matches C, all three group together — so each group reports the widest
difference it contains.

Near-duplicate detection needs perceptual hashes, so it only sees files indexed
with **Analyse images** on. HEIC and camera RAW cannot be decoded by the bundled
decoders, so they take part in identical-file detection but not near-duplicate
detection.

## Keeper suggestions

Every group nominates a keeper: highest resolution first, then largest file, then
oldest modified time, then shortest path. You can override it with the **keep**
radio button on any member.

Identical groups are armed by default because acting on them is safe. Near and
burst groups start switched off, since those are judgement calls.

## Nothing is ever deleted

**Quarantine** moves the non-keepers into the quarantine folder and records the
move, mirroring the original tree so quarantined files stay identifiable:

```
~/.local/share/renamer/quarantine/batch-0007/home/me/Pictures/2019/IMG_1234.jpg
```

**Undo last quarantine** puts every file in the batch back where it came from. If
something else now occupies the original path, that file is left in quarantine
and reported rather than overwriting whatever is there.

The app never unlinks a photo. Emptying the quarantine is a manual step, in your
own file manager, once you are satisfied — so a false-positive near-duplicate
match can never cost you data.

---

# Rename

1. Pick a **Source folder** (and optionally a separate **Output folder**).
2. Write a **Template** using `{field}` placeholders.
3. **Preview** — a before/after table with collision / error flags.
4. **Apply rename** — folders in the template are created automatically.

Slashes in a template create subfolders. Field values containing slashes are
sanitized to `_` so only template slashes make directories.

Scanning, renaming and undo run on a background thread with a progress bar, so
the window stays responsive on slow or streamed folders. EXIF is only read when
the template actually uses an EXIF field (and only for image files) — a rename
using just filesystem fields never opens file contents.

## Template fields

| Field | Meaning |
|-------|---------|
| `{name}` | original filename without extension |
| `{ext}` | extension (without dot) |
| `{parent}` / `{folder}` | name of the directory the file sits in |
| `{size}` | file size in bytes |
| `{date_taken}` | EXIF shutter time (photos) |
| `{date_created}` | filesystem created time |
| `{date_modified}` | filesystem modified time |
| `{date_accessed}` | filesystem accessed time |
| `{camera_make}` `{camera_model}` `{lens}` `{iso}` `{width}` `{height}` | EXIF camera info |
| `{n}` | sequence counter (1-based) |

### Formatting

Date fields take a [strftime](https://docs.rs/chrono/latest/chrono/format/strftime/index.html)
spec after a colon:

```
{date_taken:%Y-%m-%d}             -> 2026-06-12
{date_taken:%Y}/{date_taken:%m}   -> 2026/06  (year/month subfolders)
```

The counter takes a zero-pad width:

```
{n:03}   -> 001, 002, 003 …
```

### Examples

Sort photos into `Year/Month/` folders, keep original name:

```
{date_taken:%Y}/{date_taken:%B}/{name}
```

Rename to date + sequence:

```
{date_taken:%Y%m%d}_{n:03}
```

"Keep extension" (on by default) appends the original extension if the template
doesn't already end with one.

## Rename undo

Each **Apply** writes an undo log (`.renamer_undo.log`) into the output root and
enables the **Undo** button, which moves every file back and removes folders the
rename left empty. The log persists, so re-picking the same source/output folder
in a later session reloads it and Undo works again. The log file is ignored by
scans and never gets renamed itself.

Note this one log *does* live in the output folder, unlike the catalog: it is
deliberately tied to the folder it describes, so moving that folder to another
machine carries its undo history along.

## pCloud mode

Big folders streamed from pCloud are slow because the local filesystem downloads
files on access. **pCloud** mode skips that entirely: it talks to the pCloud API,
so listing and renaming happen server-side with no downloads.

1. Switch **Mode** to **pCloud**, pick your data **Region** (US / EU), log in
   with your pCloud email + password (sent once over HTTPS, not stored; cleared
   from memory after login).
2. Enter the **Remote folder** (e.g. `/Camera`).
3. **Preview** / **Apply rename** / **Undo** work as they do locally — renames
   and folder creation are server-side.

Caveats:

- The API does **not** expose EXIF, so `date_taken` / `camera_*` / `iso` are
  unavailable in this mode. Use `date_modified` / `date_created` (and
  `width` / `height`, which the API does provide for images).
- pCloud undo is in-memory for the session (not persisted like local undo).
- The Library and Duplicates tabs are local-only. Cataloguing a pCloud folder
  would mean downloading every file to hash and decode it, which is exactly what
  pCloud mode exists to avoid.
