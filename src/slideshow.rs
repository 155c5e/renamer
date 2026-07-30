//! Slideshow sequencing.
//!
//! Deliberately free of any UI: this module decides *what* to show and *in what
//! order*, and the app layer worries about textures and timers. That split keeps
//! the ordering, wrapping and shuffling testable without a display, which
//! matters because a slideshow is otherwise awkward to verify.
//!
//! The slideshow always runs over an index list handed in by the caller, so
//! whatever filtering is already in force — the parent filter, a minimum star
//! rating, a kind filter — carries over for free. There is no second code path
//! that could forget to apply them.

use crate::catalog::MediaRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideOrder {
    /// As listed — whatever the Images tab was sorted by.
    AsListed,
    /// Oldest capture first, which is what you want for a chronological replay.
    Date,
    /// Best first.
    Rating,
    Shuffle,
}

impl SlideOrder {
    pub fn label(self) -> &'static str {
        match self {
            SlideOrder::AsListed => "as listed",
            SlideOrder::Date => "date",
            SlideOrder::Rating => "rating",
            SlideOrder::Shuffle => "shuffle",
        }
    }

    pub const ALL: [SlideOrder; 4] = [
        SlideOrder::AsListed,
        SlideOrder::Date,
        SlideOrder::Rating,
        SlideOrder::Shuffle,
    ];
}

/// Smallest and largest seconds-per-slide the UI offers.
pub const MIN_INTERVAL: f32 = 1.0;
pub const MAX_INTERVAL: f32 = 30.0;
pub const DEFAULT_INTERVAL: f32 = 4.0;

/// A running slideshow over a fixed list of image indices.
#[derive(Debug, Clone, Default)]
pub struct Slideshow {
    /// Indices into the caller's image collection, in presentation order.
    order: Vec<usize>,
    pos: usize,
    pub playing: bool,
    pub interval_secs: f32,
}

impl Slideshow {
    pub fn new(order: Vec<usize>) -> Self {
        Self {
            order,
            pos: 0,
            playing: false,
            interval_secs: DEFAULT_INTERVAL,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// 0-based position in the sequence.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Index of the image currently on screen.
    pub fn current(&self) -> Option<usize> {
        self.order.get(self.pos).copied()
    }

    /// Advance, wrapping at the end so an unattended slideshow loops rather than
    /// stopping on a black screen.
    pub fn next(&mut self) {
        if self.order.is_empty() {
            return;
        }
        self.pos = (self.pos + 1) % self.order.len();
    }

    pub fn prev(&mut self) {
        if self.order.is_empty() {
            return;
        }
        self.pos = (self.pos + self.order.len() - 1) % self.order.len();
    }

    /// Jump to the start without disturbing playback state.
    pub fn restart(&mut self) {
        self.pos = 0;
    }

    /// Replace the sequence, keeping the current image on screen if it is still
    /// in the new one. Re-sorting mid-show should not yank the picture away.
    pub fn resequence(&mut self, order: Vec<usize>) {
        let showing = self.current();
        self.order = order;
        self.pos = showing
            .and_then(|idx| self.order.iter().position(|&o| o == idx))
            .unwrap_or(0);
    }
}

/// Build the presentation order from a pre-filtered list of row indices.
///
/// `visible` is the caller's already-filtered, already-sorted list, so
/// [`SlideOrder::AsListed`] is a straight copy.
pub fn build_order(
    rows: &[MediaRow],
    visible: &[usize],
    order: SlideOrder,
    seed: u64,
) -> Vec<usize> {
    let mut out: Vec<usize> = visible.to_vec();
    match order {
        SlideOrder::AsListed => {}
        SlideOrder::Date => out.sort_by(|&a, &b| {
            let key = |r: &MediaRow| r.date_taken.unwrap_or(r.mtime);
            key(&rows[a])
                .cmp(&key(&rows[b]))
                .then_with(|| rows[a].path.cmp(&rows[b].path))
        }),
        SlideOrder::Rating => out.sort_by(|&a, &b| {
            rows[b]
                .rating
                .cmp(&rows[a].rating)
                .then_with(|| rows[a].path.cmp(&rows[b].path))
        }),
        SlideOrder::Shuffle => shuffle(&mut out, seed),
    }
    out
}

/// Fisher-Yates using a small xorshift generator.
///
/// Hand-rolled because pulling in a whole RNG crate to shuffle a photo list is
/// not worth a dependency. Seeded explicitly so a shuffle can be reproduced,
/// which is what makes it testable.
fn shuffle(items: &mut [usize], seed: u64) {
    if items.len() < 2 {
        return;
    }
    // xorshift64 degenerates to all-zeroes from a zero seed.
    let mut state = if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed };
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn row(path: &str, taken: Option<i64>, rating: u8) -> MediaRow {
        MediaRow {
            path: PathBuf::from(path),
            file_name: path.rsplit('/').next().unwrap_or(path).to_string(),
            mtime: 500,
            date_taken: taken,
            rating,
            ..MediaRow::default()
        }
    }

    fn rows() -> Vec<MediaRow> {
        vec![
            row("/l/c.jpg", Some(300), 1),
            row("/l/a.jpg", Some(100), 5),
            row("/l/b.jpg", Some(200), 3),
        ]
    }

    fn named(rows: &[MediaRow], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| rows[i].file_name.clone()).collect()
    }

    #[test]
    fn as_listed_preserves_the_callers_order() {
        let rows = rows();
        let visible = vec![2, 0, 1];
        let order = build_order(&rows, &visible, SlideOrder::AsListed, 1);
        assert_eq!(named(&rows, &order), vec!["b.jpg", "c.jpg", "a.jpg"]);
    }

    #[test]
    fn date_order_is_chronological() {
        let rows = rows();
        let order = build_order(&rows, &[0, 1, 2], SlideOrder::Date, 1);
        assert_eq!(named(&rows, &order), vec!["a.jpg", "b.jpg", "c.jpg"]);
    }

    #[test]
    fn date_order_falls_back_to_mtime() {
        // Saved images have no capture time; they must still sequence sensibly
        // rather than all collapsing to the same key.
        let rows = vec![
            MediaRow {
                mtime: 900,
                ..row("/l/late.jpg", None, 0)
            },
            MediaRow {
                mtime: 100,
                ..row("/l/early.jpg", None, 0)
            },
        ];
        let order = build_order(&rows, &[0, 1], SlideOrder::Date, 1);
        assert_eq!(named(&rows, &order), vec!["early.jpg", "late.jpg"]);
    }

    #[test]
    fn rating_order_is_best_first() {
        let rows = rows();
        let order = build_order(&rows, &[0, 1, 2], SlideOrder::Rating, 1);
        assert_eq!(named(&rows, &order), vec!["a.jpg", "b.jpg", "c.jpg"]);
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let rows = rows();
        let visible: Vec<usize> = (0..3).collect();
        for seed in [1u64, 2, 42, 9999, 0] {
            let mut got = build_order(&rows, &visible, SlideOrder::Shuffle, seed);
            assert_eq!(got.len(), 3, "seed {seed} lost or duplicated entries");
            got.sort_unstable();
            assert_eq!(got, visible, "seed {seed} is not a permutation");
        }
    }

    #[test]
    fn shuffle_is_reproducible_and_seed_dependent() {
        let rows: Vec<MediaRow> = (0..40)
            .map(|i| row(&format!("/l/{i:02}.jpg"), Some(i), 0))
            .collect();
        let visible: Vec<usize> = (0..40).collect();

        let a = build_order(&rows, &visible, SlideOrder::Shuffle, 7);
        let b = build_order(&rows, &visible, SlideOrder::Shuffle, 7);
        assert_eq!(a, b, "same seed must give the same order");

        let c = build_order(&rows, &visible, SlideOrder::Shuffle, 8);
        assert_ne!(a, c, "different seeds should differ");
        // And it must actually shuffle rather than returning the input.
        assert_ne!(a, visible);
    }

    #[test]
    fn shuffle_handles_degenerate_lengths() {
        let rows = rows();
        assert!(build_order(&rows, &[], SlideOrder::Shuffle, 3).is_empty());
        assert_eq!(build_order(&rows, &[1], SlideOrder::Shuffle, 3), vec![1]);
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let mut show = Slideshow::new(vec![10, 11, 12]);
        assert_eq!(show.current(), Some(10));
        assert_eq!(show.len(), 3);

        show.next();
        assert_eq!(show.current(), Some(11));
        show.next();
        assert_eq!(show.current(), Some(12));
        // Wraps rather than stopping on a blank screen.
        show.next();
        assert_eq!(show.current(), Some(10));
        assert_eq!(show.pos(), 0);

        show.prev();
        assert_eq!(show.current(), Some(12), "backwards wraps too");
    }

    #[test]
    fn an_empty_slideshow_is_inert() {
        let mut show = Slideshow::new(Vec::new());
        assert!(show.is_empty());
        assert_eq!(show.current(), None);
        // Must not panic or divide by zero.
        show.next();
        show.prev();
        assert_eq!(show.current(), None);
    }

    #[test]
    fn resequencing_keeps_the_current_image_on_screen() {
        // Changing the order mid-show should not yank the picture away.
        let mut show = Slideshow::new(vec![10, 11, 12]);
        show.next();
        assert_eq!(show.current(), Some(11));

        show.resequence(vec![12, 11, 10]);
        assert_eq!(show.current(), Some(11), "still showing the same image");
        assert_eq!(show.pos(), 1);
    }

    #[test]
    fn resequencing_without_the_current_image_starts_over() {
        let mut show = Slideshow::new(vec![10, 11, 12]);
        show.next();
        show.next();
        assert_eq!(show.current(), Some(12));

        // The filter tightened and 12 is gone.
        show.resequence(vec![10, 11]);
        assert_eq!(show.current(), Some(10));
        assert_eq!(show.pos(), 0);

        show.resequence(Vec::new());
        assert_eq!(show.current(), None);
    }

    #[test]
    fn restart_returns_to_the_beginning() {
        let mut show = Slideshow::new(vec![10, 11, 12]);
        show.playing = true;
        show.next();
        show.restart();
        assert_eq!(show.pos(), 0);
        assert!(show.playing, "restart does not stop playback");
    }

    #[test]
    fn defaults_are_sane() {
        let show = Slideshow::new(vec![1]);
        assert!(!show.playing, "starts paused");
        assert!(show.interval_secs >= MIN_INTERVAL);
        assert!(show.interval_secs <= MAX_INTERVAL);
    }
}
