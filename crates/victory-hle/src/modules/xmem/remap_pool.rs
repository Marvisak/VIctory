use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

const MAX_BYTES: usize = 256 * 64 * 1024;
const MAX_ENTRY: usize = 8 << 20;
const MAX_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Key(pub(super) usize, pub(super) usize);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Cached {
    pub(super) address: usize,
    pub(super) len: usize,
    pub(super) key: Key,
}

#[derive(Default)]
pub(super) struct Pool {
    entries: VecDeque<Cached>,
    bytes: usize,
}

fn overlaps(a: Cached, address: usize, len: usize) -> bool {
    a.address < address.saturating_add(len) && address < a.address.saturating_add(a.len)
}

impl Pool {
    pub(super) fn take(&mut self, address: usize, len: usize, key: Key) -> bool {
        let Some(at) = self
            .entries
            .iter()
            .position(|e| e.address == address && e.len == len && e.key == key)
        else {
            return false;
        };
        let entry = self.entries.remove(at).expect("position found");
        self.bytes -= entry.len;
        true
    }

    pub(super) fn insert(&mut self, entry: Cached) -> Vec<Cached> {
        if entry.len == 0 || entry.len > MAX_ENTRY {
            return vec![entry];
        }
        let mut evicted = Vec::new();
        let mut at = 0;
        while at < self.entries.len() {
            let old = self.entries[at];
            if overlaps(old, entry.address, entry.len) {
                self.entries.remove(at);
                self.bytes -= old.len;
                evicted.push(old);
            } else {
                at += 1;
            }
        }
        while self.bytes + entry.len > MAX_BYTES || self.entries.len() >= MAX_ENTRIES {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= old.len;
            evicted.push(old);
        }
        self.entries.push_back(entry);
        self.bytes += entry.len;
        evicted
    }

    pub(super) fn release(&mut self, address: usize, len: usize) -> Vec<Cached> {
        let mut evicted = Vec::new();
        let mut at = 0;
        while at < self.entries.len() {
            let old = self.entries[at];
            if overlaps(old, address, len) {
                self.entries.remove(at);
                self.bytes -= old.len;
                evicted.push(old);
            } else {
                at += 1;
            }
        }
        evicted
    }
}

static POOL: LazyLock<Mutex<Pool>> = LazyLock::new(|| Mutex::new(Pool::default()));

pub(super) fn take(address: usize, len: usize, key: Key) -> bool {
    POOL.lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(address, len, key)
}

pub(super) fn insert(entry: Cached) -> Vec<Cached> {
    POOL.lock().unwrap_or_else(|e| e.into_inner()).insert(entry)
}

pub(super) fn release(address: usize, len: usize) -> Vec<Cached> {
    POOL.lock()
        .unwrap_or_else(|e| e.into_inner())
        .release(address, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(address: usize, len: usize, index: usize) -> Cached {
        Cached {
            address,
            len,
            key: Key(index, index),
        }
    }

    #[test]
    fn cached_remaps_are_reused_only_for_the_same_slice() {
        let mut pool = Pool::default();
        assert!(pool.insert(entry(0x1_0000, 64 << 10, 1)).is_empty());
        assert!(pool.take(0x1_0000, 64 << 10, Key(1, 1)));
        assert!(!pool.take(0x1_0000, 64 << 10, Key(1, 2)));
        assert!(!pool.take(0x2_0000, 64 << 10, Key(1, 1)));
    }

    #[test]
    fn cached_remaps_are_evicted_past_the_budget() {
        let mut pool = Pool::default();
        let mut first = None;
        for i in 0..5 {
            let entry = entry(0x1_0000 + i * (4 << 20), 4 << 20, i);
            if i == 0 {
                first = Some(entry);
            }
            let evicted = pool.insert(entry);
            if i == 4 {
                assert_eq!(evicted, vec![first.expect("first entry")]);
            } else {
                assert!(evicted.is_empty());
            }
        }
        assert!(!pool.take(0x1_0000, 4 << 20, Key(0, 0)));
    }

    #[test]
    fn cached_remaps_are_released_when_the_range_is_reused() {
        let mut pool = Pool::default();
        assert!(pool.insert(entry(0x1_0000, 64 << 10, 1)).is_empty());
        assert!(pool.insert(entry(0x2_0000, 64 << 10, 2)).is_empty());
        assert_eq!(
            pool.release(0x1_0000, 64 << 10),
            vec![entry(0x1_0000, 64 << 10, 1)]
        );
        assert!(!pool.take(0x1_0000, 64 << 10, Key(1, 1)));
        assert!(pool.take(0x2_0000, 64 << 10, Key(2, 2)));
    }

    #[test]
    fn oversized_remaps_are_not_cached() {
        let mut pool = Pool::default();
        let entry = entry(0x1_0000, MAX_ENTRY + PAGE_SIZE, 1);
        assert_eq!(pool.insert(entry), vec![entry]);
        assert!(!pool.take(entry.address, entry.len, entry.key));
    }

    const PAGE_SIZE: usize = 64 << 10;
}
