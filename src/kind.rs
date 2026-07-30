//! What sort of image a file is: a photograph you took, a screenshot, or an
//! image you saved from somewhere.
//!
//! Mixing these distorts everything else in the app. A moodboard full of saved
//! reference images has no capture dates, so it drags date sorting around; a
//! screenshot has no camera, so it clusters oddly; and a saved JPEG grouping
//! against a real photo in the duplicates view is just noise.
//!
//! Classification is a *suggestion* derived from what the file itself says,
//! always overridable. The override is user-assigned metadata, so like ratings
//! and the hidden flag it is keyed on content hash and survives renames.

use crate::catalog::MediaRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaKind {
    /// Taken with a camera.
    Photo,
    /// A capture of a screen.
    Screenshot,
    /// Saved or downloaded from somewhere else — moodboards, inspiration,
    /// reference images.
    Saved,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Photo => "photo",
            MediaKind::Screenshot => "screenshot",
            MediaKind::Saved => "saved",
        }
    }

    /// Stored form. Kept separate from `label` so display wording can change
    /// without silently rewriting what is in the database.
    pub fn as_str(self) -> &'static str {
        self.label()
    }

    pub fn parse(s: &str) -> Option<MediaKind> {
        match s {
            "photo" => Some(MediaKind::Photo),
            "screenshot" => Some(MediaKind::Screenshot),
            "saved" => Some(MediaKind::Saved),
            _ => None,
        }
    }

    pub const ALL: [MediaKind; 3] = [
        MediaKind::Photo,
        MediaKind::Screenshot,
        MediaKind::Saved,
    ];
}

/// Screen resolutions common enough to be worth recognizing. Checked in both
/// orientations, so a portrait phone screenshot matches its landscape entry.
const SCREEN_SIZES: &[(u32, u32)] = &[
    // Desktop / laptop
    (1280, 720),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2048, 1152),
    (2560, 1440),
    (2560, 1600),
    (2880, 1800),
    (3024, 1964),
    (3456, 2234),
    (3840, 2160),
    (5120, 2880),
    // Phones
    (750, 1334),
    (828, 1792),
    (1080, 1920),
    (1125, 2436),
    (1170, 2532),
    (1179, 2556),
    (1242, 2688),
    (1284, 2778),
    (1290, 2796),
    (1440, 3120),
];

/// Filename fragments that give a screenshot away regardless of dimensions.
const SCREENSHOT_HINTS: &[&str] = &[
    "screenshot",
    "screen shot",
    "screen_shot",
    "screen-shot",
    "screencapture",
    "screen capture",
];

/// Best guess at what a file is, from the file alone.
///
/// Camera metadata is the strongest positive signal for a photograph, and a
/// filename is the strongest signal for a screenshot — far more reliable than
/// dimensions, since plenty of real photos happen to be 1920x1080. Dimensions
/// only get a vote when nothing else has an opinion.
pub fn classify(row: &MediaRow) -> MediaKind {
    let name = row.file_name.to_lowercase();
    if SCREENSHOT_HINTS.iter().any(|h| name.contains(h)) {
        return MediaKind::Screenshot;
    }

    // A camera wrote this: make, model, lens, ISO or a capture time.
    let from_camera = row.camera_make.is_some()
        || row.camera_model.is_some()
        || row.lens.is_some()
        || row.iso.is_some()
        || row.date_taken.is_some();
    if from_camera {
        return MediaKind::Photo;
    }

    if let (Some(w), Some(h)) = (row.width, row.height) {
        if SCREEN_SIZES
            .iter()
            .any(|&(sw, sh)| (w == sw && h == sh) || (w == sh && h == sw))
        {
            return MediaKind::Screenshot;
        }
    }

    MediaKind::Saved
}

/// The kind actually in force: the user's override if they set one, otherwise
/// the guess.
pub fn effective(row: &MediaRow) -> MediaKind {
    row.kind_override.unwrap_or_else(|| classify(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn row(name: &str) -> MediaRow {
        MediaRow {
            path: PathBuf::from(format!("/lib/{name}")),
            file_name: name.to_string(),
            ..MediaRow::default()
        }
    }

    fn camera(name: &str) -> MediaRow {
        MediaRow {
            camera_make: Some("Canon".into()),
            camera_model: Some("R5".into()),
            date_taken: Some(1_600_000_000),
            ..row(name)
        }
    }

    fn sized(name: &str, w: u32, h: u32) -> MediaRow {
        MediaRow {
            width: Some(w),
            height: Some(h),
            ..row(name)
        }
    }

    #[test]
    fn camera_metadata_means_photograph() {
        assert_eq!(classify(&camera("IMG_1234.jpg")), MediaKind::Photo);

        // Any single camera signal is enough — plenty of files carry only one.
        let only_date = MediaRow {
            date_taken: Some(1),
            ..row("a.jpg")
        };
        assert_eq!(classify(&only_date), MediaKind::Photo);

        let only_iso = MediaRow {
            iso: Some("400".into()),
            ..row("b.jpg")
        };
        assert_eq!(classify(&only_iso), MediaKind::Photo);
    }

    #[test]
    fn filename_identifies_screenshots() {
        for name in [
            "Screenshot 2024-01-01 at 10.00.00.png",
            "Screen Shot 2019-05-05.png",
            "screen_shot_1.png",
            "screencapture-site.png",
            "SCREENSHOT.PNG",
        ] {
            assert_eq!(classify(&row(name)), MediaKind::Screenshot, "{name}");
        }
    }

    #[test]
    fn filename_beats_camera_metadata_for_screenshots() {
        // A screenshot of a photo can inherit EXIF-looking fields; the explicit
        // filename is the more trustworthy signal.
        let mut r = camera("Screenshot of holiday.png");
        r.width = Some(400);
        r.height = Some(300);
        assert_eq!(classify(&r), MediaKind::Screenshot);
    }

    #[test]
    fn screen_dimensions_suggest_a_screenshot_when_nothing_else_does() {
        assert_eq!(classify(&sized("a.png", 1920, 1080)), MediaKind::Screenshot);
        assert_eq!(classify(&sized("b.png", 2560, 1440)), MediaKind::Screenshot);
        // Portrait phone capture: the same entry, transposed.
        assert_eq!(classify(&sized("c.png", 1170, 2532)), MediaKind::Screenshot);
        assert_eq!(classify(&sized("d.png", 2532, 1170)), MediaKind::Screenshot);
    }

    #[test]
    fn a_real_photo_at_screen_dimensions_is_still_a_photo() {
        // Dimensions are the weakest signal and must not override camera data —
        // plenty of real photos are exported at 1920x1080.
        let mut r = camera("IMG_9.jpg");
        r.width = Some(1920);
        r.height = Some(1080);
        assert_eq!(classify(&r), MediaKind::Photo);
    }

    #[test]
    fn anything_else_is_a_saved_image() {
        assert_eq!(classify(&row("inspo-01.jpg")), MediaKind::Saved);
        assert_eq!(classify(&sized("moodboard.png", 733, 1100)), MediaKind::Saved);
        assert_eq!(classify(&row("tumblr_abc123.png")), MediaKind::Saved);
    }

    #[test]
    fn user_override_wins_over_the_guess() {
        let mut r = camera("IMG_1.jpg");
        assert_eq!(effective(&r), MediaKind::Photo);

        r.kind_override = Some(MediaKind::Saved);
        assert_eq!(effective(&r), MediaKind::Saved);
        // The underlying guess is unchanged; only the effective value moves.
        assert_eq!(classify(&r), MediaKind::Photo);
    }

    #[test]
    fn stored_form_roundtrips() {
        for k in MediaKind::ALL {
            assert_eq!(MediaKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(MediaKind::parse("nonsense"), None);
        assert_eq!(MediaKind::parse(""), None);
    }
}
