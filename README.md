# Renamer

Bulk file renamer / organizer. GUI frontend (egui), metadata-driven templates.
Linux + macOS.

## Run

```sh
cargo run --release
```

## How it works

1. Pick a **Source folder** (and optionally a separate **Output folder**).
2. Write a **Template** using `{field}` placeholders.
3. **Preview** — see a before/after table with collision / error flags.
4. **Apply rename** — folders in the template are created automatically.

Slashes in a template create subfolders. Field values containing slashes are
sanitized to `_` so only template slashes make directories.

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
{date_taken:%Y-%m-%d}      -> 2026-06-12
{date_taken:%Y}/{date_taken:%m}   -> 2026/06  (year/month subfolders)
```

The counter takes a zero-pad width:

```
{n:03}   -> 001, 002, 003 …
```

## Examples

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

## Undo

Each **Apply** writes an undo log (`.renamer_undo.log`) into the output root and
enables the **Undo** button, which moves every file back and removes folders the
rename left empty. The log persists, so re-picking the same source/output folder
in a later session reloads it and the Undo button works again. The log file is
ignored by scans and never gets renamed itself.
