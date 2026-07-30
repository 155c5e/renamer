# Renamer

Photo library organizer. GUI frontend (egui), Linux + macOS.

Four tools over one catalog:

- **Library** — index a folder once; metadata, hashes, colours and thumbnails
  are cached so later scans only touch files that changed.
- **Images** — browse the catalogue, sort by path, date, size, colour or
  rating, star your keepers, classify photos vs screenshots vs saved images,
  and mark images hidden.
- **Slideshow** — play the library back, ordered by date, rating or shuffle.
- **Duplicates** — find identical files, near-duplicates (the same picture at a
  different size or quality), and burst runs. Removal is reversible.
- **Rename** — the metadata-driven bulk renamer, with local and pCloud modes.

A **parent filter** in the tab bar hides marked images across every view at
once.

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
4. **Analyse** — decode each image once to produce a perceptual hash, its
   dominant colour, and a thumbnail. The slow stage on a first run; cached
   forever after.

| Option | Effect |
|--------|--------|
| Recurse subfolders | Descend into subdirectories |
| Images only | Ignore files that are not recognizable images |
| Read EXIF | Capture date, camera, lens. Reads file contents |
| Analyse images | Perceptual hashes, dominant colour, thumbnails. Required for near-duplicate detection and colour sort |

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

# Images

Browse what has been catalogued. Sort by **path**, **date**, **size**, or
**colour** — colour arranges the library by hue into a rainbow, with greyscale
images collected at the end rather than scattered through the spectrum on
meaningless hue values.

Dominant colour is found by k-means over a downsampled copy of each image,
taking the largest cluster. Averaging pixels instead would turn every photo the
same muddy brown — average a red sunset against a blue sea and you get grey.

The swatch beside each row is that dominant colour.

## Ratings

Click the stars on any row to rate 0–5. Clicking the star that is already the
rating clears it. Sort by **rating** to put your keepers first, and use the
**min ★** slider to hide everything below a threshold.

Ratings are stored against the image's *content hash*, so they survive renaming,
moving and re-importing — including renames done in the Rename tab.

They live in the catalog only. They are not written into the files or into XMP
sidecars, so Lightroom and digiKam will not see them, and they do not travel with
the photos to another machine.

## Photos, screenshots and saved images

Not everything in a picture folder is a photograph. Moodboards, inspiration and
saved reference images have no capture date and no camera, so they distort date
sorting and add noise to duplicate detection.

Each image is classified automatically:

| Kind | How it is guessed |
|------|-------------------|
| **photo** | Has camera metadata — make, model, lens, ISO, or a capture time |
| **screenshot** | Filename says so, or the dimensions match a common screen size |
| **saved** | Everything else |

The filename is trusted ahead of dimensions, since plenty of real photos are
exported at exactly 1920×1080. Use the dropdown on any row to override the guess;
`auto` puts it back. An overridden kind shows with an asterisk.

**Duplicate detection is scoped by kind.** A saved reference image that happens
to match a photo you took is not a duplicate — you want both — so the detectors
only ever group within a single kind.

## Hiding images and the parent filter

Tick **hide** on any image to mark it hidden. The **Parent filter** checkbox in
the tab bar then excludes every hidden image from every view — the image list,
duplicate groups, everything that reads the catalog.

It is enforced in one place: the catalog query layer takes a visibility argument
that no caller can skip. Views do not filter for themselves, because sooner or
later one of them would forget, and the failure mode here is showing the exact
thing you meant to hide.

The filter **starts on** each launch, for the same reason.

Two things worth being clear about:

- **It is a display filter, not security.** The files are still on disk, under
  their real names, unencrypted. Anyone with a file manager can see them.
- **Byte-identical copies share the flag.** Hiding one hides all of them, since
  they are the same picture.

Like ratings and kind overrides, hidden-ness is stored against the image's
*content hash*, not its path, so it survives renaming, moving, and re-importing
the file.

---

# Slideshow

Plays back **exactly what the Images tab is showing** — same text filter, same
minimum stars, same kind filter, same parent filter. The sequence is built from
that one list, so there is no second filtering path that could disagree and
show you something you had excluded.

| Control | |
|---------|---|
| `Space` | play / pause |
| `→` or `N` | next |
| `←` or `P` | previous |
| `Esc` | leave fullscreen |

Order can be **as listed**, **date** (oldest first, falling back to modified
time for images with no capture date), **rating** (best first), or **shuffle**.
Playback wraps at the end rather than stopping on a blank screen.

Images are decoded on a background thread and downscaled to 2560px before being
uploaded as a texture, so advancing does not stall the window and a 50-megapixel
original does not become a 200 MB texture. The previous image stays on screen
until the next one is ready.

Rating photos and then playing back everything at 4★ and above is the intended
loop: cull first, then watch.

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

Renaming also updates the catalog, so a renamed file keeps its existing row —
along with its hashes, colour and thumbnail — instead of looking like one file
vanishing and another appearing, which would force a needless re-decode.

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
