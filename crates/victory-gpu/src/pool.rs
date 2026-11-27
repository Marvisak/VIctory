use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Blocks made ahead so that the first piece of a new one is not read by
/// lists already on the card (`VICTORY_RIPE_BLOCKS`, 12; 0: use at once).
pub(crate) fn ripe_blocks() -> usize {
    static AHEAD: OnceLock<usize> = OnceLock::new();
    *AHEAD.get_or_init(|| {
        let asked = std::env::var("VICTORY_RIPE_BLOCKS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(12);
        if crate::host::small_memory() { asked.min(4) } else { asked }
    })
}

static UNRIPE: AtomicU64 = AtomicU64::new(0);

/// The submissions begun when a block was made, and those already finished.
#[derive(Clone, Copy)]
pub(crate) struct Ripe {
    pub(crate) made: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    pub(crate) fn now() -> Ripe {
        let (started, finished) = crate::host::submissions();
        Ripe { made: started, finished, ahead: ripe_blocks() }
    }

    pub(crate) fn is(self, made: u64) -> bool {
        self.ahead == 0 || made <= self.finished
    }

    /// The oldest block is used when none is ripe; counted for diagnostics.
    pub(crate) fn unripe(self, made: u64) -> bool {
        let late = self.ahead > 0 && !self.is(made);
        if late {
            UNRIPE.fetch_add(1, Ordering::Relaxed);
        }
        late
    }
}

#[cfg(test)]
mod tests {
    use super::Ripe;

    #[test]
    fn pieces_are_cut_from_blocks_older_than_the_lists_that_run() {
        let at = |made, finished| Ripe { made, finished, ahead: 2 };
        assert!(at(0, 0).is(0), "nothing runs: at once");
        assert!(!at(5, 4).is(5), "block 5 made while submission 5 runs");
        assert!(at(5, 5).is(5), "ripe once that submission finishes");
        assert!(Ripe { made: 0, finished: 0, ahead: 0 }.is(9), "off: every block is ripe");
    }
}
