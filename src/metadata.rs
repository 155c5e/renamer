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

    /// Decimal degrees, south and west negative. Both are set together or not
    /// at all — half a coordinate is useless.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
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

/// Convert an EXIF GPS coordinate to signed decimal degrees.
///
/// EXIF stores position as three rationals — degrees, minutes, seconds — with the
/// hemisphere in a separate reference tag, so south and west come back positive
/// and have to be negated.
fn gps_degrees(exif: &exif::Exif, coord: exif::Tag, reference: exif::Tag) -> Option<f64> {
    use exif::{In, Value};

    let field = exif.get_field(coord, In::PRIMARY)?;
    let parts = match &field.value {
        Value::Rational(v) => v,
        _ => return None,
    };
    if parts.len() < 3 {
        return None;
    }
    // Some cameras put the whole value in degrees and leave minutes/seconds
    // zero; the arithmetic handles that without a special case.
    let degrees = parts[0].to_f64() + parts[1].to_f64() / 60.0 + parts[2].to_f64() / 3600.0;

    let hemisphere = exif
        .get_field(reference, In::PRIMARY)
        .map(|f| f.display_value().to_string().trim_matches('"').to_uppercase())
        .unwrap_or_default();
    if hemisphere.starts_with('S') || hemisphere.starts_with('W') {
        Some(-degrees)
    } else {
        Some(degrees)
    }
}

/// Format a coordinate pair the way a human reads one.
pub fn format_coords(lat: f64, lon: f64) -> String {
    format!(
        "{:.5}°{}, {:.5}°{}",
        lat.abs(),
        if lat >= 0.0 { "N" } else { "S" },
        lon.abs(),
        if lon >= 0.0 { "E" } else { "W" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_format_with_hemispheres() {
        assert_eq!(format_coords(51.5074, -0.1278), "51.50740°N, 0.12780°W");
        assert_eq!(format_coords(-33.8688, 151.2093), "33.86880°S, 151.20930°E");
        assert_eq!(format_coords(0.0, 0.0), "0.00000°N, 0.00000°E");
    }

    #[test]
    fn image_extensions_cover_raw_and_heic() {
        // GPS lives in EXIF, which RAW and HEIC both carry, so they must stay in
        // the "worth reading metadata from" list even though they cannot be
        // decoded for thumbnails.
        for ext in ["jpg", "JPEG", "heic", "cr2", "nef", "dng", "arw"] {
            assert!(is_image_ext(ext), "{ext}");
        }
        assert!(!is_image_ext("txt"));
        assert!(!is_image_ext(""));
    }

    #[test]
    fn geotag_defaults_to_absent() {
        let m = FileMeta::default();
        assert!(m.lat.is_none() && m.lon.is_none());
    }
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

    // Geotag. Only accepted as a pair, since one axis alone cannot place a photo.
    if let (Some(lat), Some(lon)) = (
        gps_degrees(&exif, Tag::GPSLatitude, Tag::GPSLatitudeRef),
        gps_degrees(&exif, Tag::GPSLongitude, Tag::GPSLongitudeRef),
    ) {
        // Reject implausible values rather than letting a corrupt tag put a
        // photo in the middle of the ocean at (0, 0).
        if lat.abs() <= 90.0 && lon.abs() <= 180.0 && !(lat == 0.0 && lon == 0.0) {
            m.lat = Some(lat);
            m.lon = Some(lon);
        }
    }

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
