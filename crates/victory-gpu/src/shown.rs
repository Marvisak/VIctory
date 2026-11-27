//! The images the title presents, and whether its frame wrote them.
//!
//! A draw whose pipeline is still being made is dropped (`pipeline::make`),
//! and leaves its render targets as they were. When that draw is the one
//! that writes a presented image, the image still holds what the title drew
//! into it the last time; with two back buffers presented in turn, the
//! window then alternates between two old frames. Grand Theft Auto VI's
//! world start on a cold driver cache did that for 46 frames: the final
//! upscale (`PS_TAA2_SpatialUpscaleWithDithering`) was one of ~1,900
//! pipelines first used in one frame and made 41 s later, and until then
//! the window showed the fade's last black frame and the first dim one in
//! turn.
//!
//! So such a draw puts its pipeline first in the workers' queue, and the
//! present of an image it left unwritten is not shown: the window keeps
//! the last frame it showed, for at most [`HOLD_LIMIT`].
//! `VICTORY_SHOW_UNWRITTEN=1` shows them as before.
//!
//! Only draws whose pipeline is being made count: one that is refused
//! never comes, and holding for it would hold for ever. One that is being
//! made may never come either (its worker died, or the driver's compile
//! hangs): past the limit the frames are shown again, as they come.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Presented images remembered, at most (the title's back buffers, and a
/// few of an earlier size).
const IMAGES: usize = 8;

/// The longest run of held frames. The pipeline held for is the next one a
/// worker takes, so it waits for the first of the workers to finish what
/// it is making: in the cold-cache runs that was a driver compile of
/// 4–6 s (10.5 s at most), and the final upscale itself was made in
/// 3.3 ms. Ten seconds of a still window is past any of those; longer, a
/// frozen window is worse than the old frames.
const HOLD_LIMIT: Duration = Duration::from_secs(10);

/// What to do with a presented frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    /// Show it.
    Show,
    /// Show it; it ends a run of `held` frames not shown.
    ShowAgain { held: u64 },
    /// Do not show it (`first`: it starts a run of held frames).
    Hold { first: bool },
    /// Show it, and those after it, though unwritten: `held` frames were
    /// held for [`HOLD_LIMIT`] waiting for the pipelines `waited_for`.
    GiveUp { held: u64, waited_for: String },
}

/// Images presented, newest last; those a dropped draw left unwritten since
/// their last present, and the pipelines those draws waited for; and the
/// current run of held frames.
#[derive(Default)]
struct Shown {
    images: Vec<usize>,
    unwritten: Vec<usize>,
    waited_for: Vec<String>,
    /// When the run of held frames started, and how many it holds.
    held: Option<(Instant, u64)>,
    /// Held for [`HOLD_LIMIT`]: unwritten frames are shown until one is
    /// written again.
    gave_up: bool,
}

impl Shown {
    const fn new() -> Shown {
        Shown {
            images: Vec::new(),
            unwritten: Vec::new(),
            waited_for: Vec::new(),
            held: None,
            gave_up: false,
        }
    }

    /// A draw into `targets` waiting for the pipeline `waited_for` names
    /// was dropped: whether one is a presented image, which is then
    /// unwritten until it is next presented.
    fn dropped(&mut self, targets: &[usize], waited_for: impl FnOnce() -> String) -> bool {
        let mut any = false;
        for &t in targets.iter().filter(|t| self.images.contains(t)) {
            any = true;
            if !self.unwritten.contains(&t) {
                self.unwritten.push(t);
            }
        }
        if any {
            let w = waited_for();
            if !self.waited_for.contains(&w) {
                self.waited_for.push(w);
            }
        }
        any
    }

    /// `image` is presented: whether a dropped draw left it unwritten.
    fn unwritten(&mut self, image: usize) -> bool {
        if let Some(i) = self.images.iter().position(|&s| s == image) {
            self.images.remove(i);
        } else if self.images.len() == IMAGES {
            let evicted = self.images.remove(0);
            // A later allocation may reuse its address.
            self.unwritten.retain(|&s| s != evicted);
        }
        self.images.push(image);
        let unwritten = self.unwritten.iter().position(|&s| s == image);
        unwritten.map(|i| self.unwritten.swap_remove(i)).is_some()
    }

    /// A frame presenting `images` at `now`, holding for at most `limit`;
    /// with `show_all` it holds none, and counts none.
    fn frame(
        &mut self,
        images: impl IntoIterator<Item = usize>,
        now: Instant,
        limit: Duration,
        show_all: bool,
    ) -> Frame {
        // Every image, to note each as presented.
        let unwritten = images
            .into_iter()
            .fold(false, |any, image| self.unwritten(image) | any);
        if show_all {
            self.held = None;
            self.gave_up = false;
            self.waited_for.clear();
            return Frame::Show;
        }
        if !unwritten {
            self.gave_up = false;
            self.waited_for.clear();
            return match self.held.take() {
                Some((_, held)) => Frame::ShowAgain { held },
                None => Frame::Show,
            };
        }
        if self.gave_up {
            return Frame::Show;
        }
        let (since, held) = self.held.get_or_insert((now, 0));
        if now.duration_since(*since) >= limit {
            let held = *held;
            self.held = None;
            self.gave_up = true;
            return Frame::GiveUp {
                held,
                waited_for: self.waited_for.join(", "),
            };
        }
        *held += 1;
        Frame::Hold { first: *held == 1 }
    }
}

static SHOWN: Mutex<Shown> = Mutex::new(Shown::new());

/// `VICTORY_SHOW_UNWRITTEN=1`: show every presented image, as before.
static SHOW_ALL: LazyLock<bool> = LazyLock::new(|| crate::switch::on("VICTORY_SHOW_UNWRITTEN"));

/// A draw into `targets` (title resources) was dropped because the
/// pipeline `waited_for` names is being made: whether one of them is an
/// image the title presents.
pub(crate) fn dropped(targets: &[usize], waited_for: impl FnOnce() -> String) -> bool {
    !targets.is_empty()
        && SHOWN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dropped(targets, waited_for)
}

/// The title presents `images` (title resources): whether to show them.
pub(crate) fn frame(images: impl IntoIterator<Item = usize>) -> Frame {
    SHOWN.lock().unwrap_or_else(|e| e.into_inner()).frame(
        images,
        Instant::now(),
        HOLD_LIMIT,
        *SHOW_ALL,
    )
}

#[cfg(test)]
mod tests {
    use super::{Frame, IMAGES, Shown};
    use std::time::{Duration, Instant};

    const LIMIT: Duration = Duration::from_secs(10);

    fn upscale() -> String {
        "[vertex 0309a446e1a48352, pixel 4f4ec7b16673362f]".into()
    }

    /// Two back buffers presented in turn; the final draw into one is
    /// dropped: that one is held once, the other is shown.
    #[test]
    fn an_image_a_dropped_draw_left_unwritten_is_not_shown() {
        let mut s = Shown::default();
        let t = Instant::now();
        let (a, b, other) = (0x1230, 0x1300, 0x4000);
        assert_eq!(s.frame([a], t, LIMIT, false), Frame::Show);
        assert_eq!(s.frame([b], t, LIMIT, false), Frame::Show);
        assert!(!s.dropped(&[other], upscale), "not a presented image");
        assert!(s.dropped(&[other, a], upscale));
        assert!(s.dropped(&[a], upscale), "dropped twice, held once");
        assert_eq!(s.frame([b], t, LIMIT, false), Frame::Show);
        assert_eq!(s.frame([a], t, LIMIT, false), Frame::Hold { first: true });
        assert!(s.dropped(&[b], upscale));
        assert_eq!(
            s.frame([other, b], t, LIMIT, false),
            Frame::Hold { first: false },
            "any plane"
        );
        assert_eq!(
            s.frame([a], t, LIMIT, false),
            Frame::ShowAgain { held: 2 },
            "written again since"
        );
        assert_eq!(s.frame([b], t, LIMIT, false), Frame::Show);
    }

    /// An image never presented is not one, whatever is drawn into it.
    #[test]
    fn a_target_never_presented_is_never_held() {
        let mut s = Shown::default();
        assert!(!s.dropped(&[0x10], upscale));
        assert_eq!(s.frame([0x10], Instant::now(), LIMIT, false), Frame::Show);
    }

    /// Images of an earlier size (a resize) are forgotten after a while.
    #[test]
    fn old_images_are_forgotten() {
        let mut s = Shown::default();
        for image in 1..=IMAGES + 1 {
            assert_eq!(s.frame([image], Instant::now(), LIMIT, false), Frame::Show);
        }
        assert!(!s.dropped(&[1], upscale));
        assert!(s.dropped(&[IMAGES + 1], upscale));
    }

    /// A pipeline that never comes holds the window for the limit, then
    /// its frames are shown as they come, until one is written again.
    #[test]
    fn a_hold_is_given_up_after_the_limit() {
        let mut s = Shown::default();
        let t = Instant::now();
        let a = 0x1230;
        assert_eq!(s.frame([a], t, LIMIT, false), Frame::Show);
        let mut at = |secs: u64| {
            s.dropped(&[a], upscale);
            s.frame([a], t + Duration::from_secs(secs), LIMIT, false)
        };
        assert_eq!(at(1), Frame::Hold { first: true });
        assert_eq!(at(5), Frame::Hold { first: false });
        assert_eq!(at(10), Frame::Hold { first: false });
        assert_eq!(
            at(11),
            Frame::GiveUp {
                held: 3,
                waited_for: upscale()
            }
        );
        assert_eq!(at(12), Frame::Show, "given up: shown as they come");
        assert_eq!(at(30), Frame::Show);
        assert_eq!(
            s.frame([a], t + Duration::from_secs(31), LIMIT, false),
            Frame::Show
        );
        s.dropped(&[a], upscale);
        assert_eq!(
            s.frame([a], t + Duration::from_secs(32), LIMIT, false),
            Frame::Hold { first: true },
            "written once since: a new run of holds"
        );
    }

    /// An image evicted from those remembered is forgotten as unwritten
    /// too: an allocation reusing its address is not held.
    #[test]
    fn an_evicted_image_is_not_left_unwritten() {
        let mut s = Shown::default();
        let t = Instant::now();
        assert_eq!(s.frame([1], t, LIMIT, false), Frame::Show);
        assert!(s.dropped(&[1], upscale));
        for image in 2..=IMAGES + 1 {
            assert_eq!(s.frame([image], t, LIMIT, false), Frame::Show);
        }
        assert_eq!(s.frame([1], t, LIMIT, false), Frame::Show, "a new image");
    }

    /// With `VICTORY_SHOW_UNWRITTEN=1` nothing is held, and no run of held
    /// frames is counted.
    #[test]
    fn show_all_holds_and_counts_nothing() {
        let mut s = Shown::default();
        let t = Instant::now();
        let a = 0x1230;
        assert_eq!(s.frame([a], t, LIMIT, true), Frame::Show);
        for secs in [1, 11, 12] {
            assert!(s.dropped(&[a], upscale), "its pipeline is still hurried");
            let at = t + Duration::from_secs(secs);
            assert_eq!(s.frame([a], at, LIMIT, true), Frame::Show);
        }
        // Held and given up as before once the switch is off: no run was
        // counted, so none ends or gives up with a count.
        assert!(s.dropped(&[a], upscale));
        assert_eq!(s.frame([a], t, LIMIT, false), Frame::Hold { first: true });
    }
}
