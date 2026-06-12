use std::fs;
use std::path::Path;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};

/// Parsed metadata for a single file. Dates are stored as `DateTime<Local>`
/// so templates can apply arbitrary strftime formats; everything else is a
/// pre-rendered string.
#[derive(Debug, Clone, Default)]
pub struct FileMeta {
    pub name: String,   // file stem (no extension)
    pub ext: String,    // extension without dot
    pub parent: String, // immediate parent directory name
    pub size: u64,

    pub date_taken: Option<DateTime<Local>>,
    pub date_created: Option<DateTime<Local>>,
    pub date_modified: Option<DateTime<Local>>,
    pub date_accessed: Option<DateTime<Local>>,

    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<String>,
    pub width: Option<String>,
    pub height: Option<String>,
}

impl FileMeta {
    /// Read metadata, optionally including EXIF.
    ///
    /// EXIF requires opening and reading file *contents*, which on a streamed
    /// cloud filesystem (pCloud, Dropbox, …) forces a download. The cheap
    /// filesystem fields (name, size, dates) do not. Pass `want_exif: false`
    /// to stay metadata-only and avoid downloads.
    pub fn read_with(path: &Path, want_exif: bool) -> Self {
        let mut m = FileMeta::default();

        m.name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        m.ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        m.parent = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        if let Ok(fsmeta) = fs::metadata(path) {
            m.size = fsmeta.len();
            m.date_created = fsmeta.created().ok().map(systime_to_local);
            m.date_modified = fsmeta.modified().ok().map(systime_to_local);
            m.date_accessed = fsmeta.accessed().ok().map(systime_to_local);
        }

        if want_exif && is_image_ext(&m.ext) {
            read_exif(path, &mut m);
        }
        m
    }

    /// Resolve a template field. `fmt` is the optional strftime spec that
    /// follows the colon in `{field:fmt}`. Returns `None` for unknown or
    /// unavailable fields so the engine can flag the row.
    pub fn field(&self, name: &str, fmt: Option<&str>) -> Option<String> {
        let date = |d: &Option<DateTime<Local>>| {
            d.as_ref()
                .map(|dt| dt.format(fmt.unwrap_or("%Y-%m-%d")).to_string())
        };
        match name {
            "name" => Some(self.name.clone()),
            "ext" => Some(self.ext.clone()),
            // `parent` and `folder` are aliases: the name of the directory the
            // file currently sits in.
            "parent" | "folder" => Some(self.parent.clone()),
            "size" => Some(self.size.to_string()),
            "date_taken" => date(&self.date_taken),
            "date_created" => date(&self.date_created),
            "date_modified" => date(&self.date_modified),
            "date_accessed" => date(&self.date_accessed),
            "camera_make" => self.camera_make.clone(),
            "camera_model" => self.camera_model.clone(),
            "lens" => self.lens.clone(),
            "iso" => self.iso.clone(),
            "width" => self.width.clone(),
            "height" => self.height.clone(),
            _ => None,
        }
    }
}

/// Field names usable in templates, for the GUI help panel.
pub const FIELDS: &[&str] = &[
    "name",
    "ext",
    "parent",
    "folder",
    "size",
    "date_taken",
    "date_created",
    "date_modified",
    "date_accessed",
    "camera_make",
    "camera_model",
    "lens",
    "iso",
    "width",
    "height",
];

/// Template fields whose value comes from EXIF, i.e. require reading file
/// contents. Used to decide whether a scan needs to open files at all.
pub const EXIF_FIELDS: &[&str] = &[
    "date_taken",
    "camera_make",
    "camera_model",
    "lens",
    "iso",
    "width",
    "height",
];

/// True if any of `fields` is EXIF-derived.
pub fn needs_exif(fields: &[String]) -> bool {
    fields.iter().any(|f| EXIF_FIELDS.contains(&f.as_str()))
}

/// File extensions worth attempting EXIF on. Avoids opening non-images.
pub fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext.to_lowercase().as_str(),
        "jpg" | "jpeg" | "tif" | "tiff" | "heic" | "heif" | "png" | "webp" | "dng" | "cr2"
            | "nef" | "arw" | "raf" | "rw2" | "orf"
    )
}

fn systime_to_local(t: std::time::SystemTime) -> DateTime<Local> {
    DateTime::<Local>::from(t)
}

fn read_exif(path: &Path, m: &mut FileMeta) {
    use exif::{In, Reader, Tag, Value};

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut buf = std::io::BufReader::new(file);
    let exif = match Reader::new().read_from_container(&mut buf) {
        Ok(e) => e,
        Err(_) => return, // not an image / no EXIF
    };

    let str_field = |tag: Tag| {
        exif.get_field(tag, In::PRIMARY).map(|f| {
            f.display_value()
                .to_string()
                .trim_matches('"')
                .to_string()
        })
    };

    m.camera_make = str_field(Tag::Make);
    m.camera_model = str_field(Tag::Model);
    m.lens = str_field(Tag::LensModel);
    m.iso = str_field(Tag::PhotographicSensitivity);
    m.width = str_field(Tag::PixelXDimension);
    m.height = str_field(Tag::PixelYDimension);

    // Prefer DateTimeOriginal (shutter time), fall back to DateTime.
    let raw = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTime, In::PRIMARY));
    if let Some(field) = raw {
        if let Value::Ascii(ref vec) = field.value {
            if let Some(bytes) = vec.first() {
                if let Ok(s) = std::str::from_utf8(bytes) {
                    // EXIF format: "YYYY:MM:DD HH:MM:SS"
                    if let Ok(naive) =
                        NaiveDateTime::parse_from_str(s.trim(), "%Y:%m:%d %H:%M:%S")
                    {
                        m.date_taken =
                            Local.from_local_datetime(&naive).single();
                    }
                }
            }
        }
    }
}
