//! Renderer port 7: a sharded submit journal (`VICTORY_PORT_JOURNAL=1`, off
//! by default). A page noted in the current generation was already marked
//! for every tracker that held it, so marking it again before a take is
//! redundant. Notes live in [`SHARDS`] parts, each under its own lock: a
//! submission touching one part does not wait for another.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::RwLock;

/// Pieces of `1 << SHARD_SHIFT` pages (64 MiB), every [`SHARDS`]th in one part.
const SHARD_SHIFT: u32 = 10;
const SHARDS: usize = 64;

fn shard_of(page: usize) -> usize {
    (page >> SHARD_SHIFT) % SHARDS
}

/// Bumped when a tracker takes a page: notes from before the take no longer
/// say it is marked.
static GENERATION: AtomicU64 = AtomicU64::new(1);

struct Journal([RwLock<BTreeMap<usize, u64>>; SHARDS]);

impl Journal {
    const fn new() -> Self {
        Self([const { RwLock::new(BTreeMap::new()) }; SHARDS])
    }

    /// The pages of `[first, last)` not noted at `gen`; notes them. A page
    /// noted at an older generation is marked again, as are pages the
    /// switch does not cover.
    fn claim(&self, first: usize, last: usize, epoch: u64) -> Vec<usize> {
        let mut fresh = Vec::new();
        if !crate::watch::port_journal() {
            return (first..last).collect();
        }
        for page in first..last {
            // Only this part's lock is held, and never another part's:
            // every caller takes the parts in the same page order.
            let mut note = self.0[shard_of(page)].write().unwrap_or_else(|e| e.into_inner());
            if note.insert(page, epoch) != Some(epoch) {
                fresh.push(page);
            }
        }
        fresh
    }
}

static JOURNAL: Journal = Journal::new();

pub(crate) fn on() -> bool {
    crate::watch::port_journal()
}

pub(crate) fn generation() -> u64 {
    GENERATION.load(Relaxed)
}

pub(crate) fn advance() {
    GENERATION.fetch_add(1, Relaxed);
}

/// The pages of `[first, last)` that still need marking.
pub(crate) fn claim(first: usize, last: usize) -> Vec<usize> {
    JOURNAL.claim(first, last, generation())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enable(on: Option<bool>) {
        crate::watch::PORT_JOURNAL_IN_TEST.set(on);
    }

    #[test]
    fn ranges_name_their_parts() {
        let p = 1 << SHARD_SHIFT;
        assert_eq!(shard_of(3), 0);
        assert_eq!(shard_of(p - 1), 0);
        assert_eq!(shard_of(p), 1);
        assert_eq!(shard_of(p + 1), 1);
        assert_eq!(shard_of(63 * p), 63);
        assert_eq!(shard_of(65 * p), 1);
        let mut seen = [false; SHARDS];
        for page in 1..1 + SHARDS * p {
            seen[shard_of(page)] = true;
        }
        assert!(seen.iter().all(|part| *part));
    }

    #[test]
    fn claims_are_per_shard_and_per_generation() {
        enable(Some(true));
        assert!(on());
        let epoch = generation().wrapping_add(1);
        let a = 16;
        let b = 17 << SHARD_SHIFT;
        assert_eq!(JOURNAL.claim(a, a + 3, epoch), [16, 17, 18]);
        assert!(JOURNAL.claim(a, a + 3, epoch).is_empty());
        assert_eq!(JOURNAL.claim(b, b + 2, epoch), vec![b, b + 1]);
        assert!(JOURNAL.claim(b, b + 2, epoch).is_empty());
        assert_eq!(JOURNAL.claim(a, a + 1, epoch + 1), [a]);
        assert_eq!(shard_of(a), 0);
        assert_eq!(shard_of(b), 17 % SHARDS);
        enable(Some(false));
    }

    #[test]
    fn without_the_switch_every_page_is_fresh() {
        enable(Some(false));
        let epoch = generation().wrapping_add(2);
        assert_eq!(JOURNAL.claim(8, 10, epoch), [8, 9]);
        assert_eq!(JOURNAL.claim(8, 10, epoch), [8, 9]);
        assert!(!on());
    }
}
