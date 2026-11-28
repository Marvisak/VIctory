use std::collections::HashSet;
use std::hash::Hash;
use std::sync::OnceLock;

pub(crate) const FULL_EVERY: u64 = 256;

pub(crate) fn marked() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MARKED_HEAP_PASS").is_some_and(|v| v == "1"))
}

#[derive(Default)]
pub(crate) struct Marked<K> {
    dirty: HashSet<K>,
    passes: u64,
}

impl<K: Hash + Eq + Clone> Marked<K> {
    pub(crate) fn note(&mut self, key: K) {
        self.dirty.insert(key);
    }

    pub(crate) fn plan(
        &mut self,
        dirty: impl Fn(&K) -> bool,
        all: impl FnOnce() -> Vec<K>,
    ) -> Vec<K> {
        self.passes += 1;
        let mut seen = HashSet::new();
        let mut planned = Vec::new();
        for key in std::mem::take(&mut self.dirty) {
            if seen.insert(key.clone()) {
                planned.push(key);
            }
        }
        let all = all();
        let full = self.passes.is_multiple_of(FULL_EVERY);
        for key in all {
            if (full || dirty(&key)) && seen.insert(key.clone()) {
                planned.push(key);
            }
        }
        planned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_marked_keys_are_planned_again() {
        let all = || vec![10, 20];
        let mut m = Marked::default();
        m.note(10);
        assert_eq!(m.plan(|_| false, all), vec![10]);
        assert!(m.plan(|_| false, all).is_empty());
        m.note(10);
        m.note(20);
        assert_eq!(m.plan(|_| false, all).into_iter().collect::<HashSet<_>>(), HashSet::from([10, 20]));
        assert!(m.plan(|_| false, all).is_empty());
    }

    #[test]
    fn a_full_pass_visits_every_key_and_clears_marks() {
        let all = || vec![10, 20];
        let mut m = Marked { passes: FULL_EVERY - 1, ..Default::default() };
        m.note(10);
        let planned = m.plan(|_| false, all);
        assert_eq!(planned.into_iter().collect::<HashSet<_>>(), HashSet::from([10, 20]));
        assert!(m.plan(|_| false, all).is_empty());
    }

    #[test]
    fn dirty_memory_without_a_slot_rewrite_is_planned() {
        let p = crate::watch::tests::pool(1);
        let tracked = crate::watch::track(p, crate::watch::PAGE, true).expect("pool memory is tracked");
        tracked.take(p, p + crate::watch::PAGE);
        let bit = tracked.bit().expect("a dense bit");
        assert!(crate::watch::clean_bit(Some(bit)));
        // SAFETY: the test's own write-watched page.
        unsafe { std::ptr::write_bytes(p as *mut u8, 1, crate::watch::PAGE) };
        assert!(!crate::watch::clean_bit(Some(bit)));
        let mut m = Marked::<u32>::default();
        assert_eq!(m.plan(|k| !crate::watch::clean_bit(Some(*k)), || vec![bit]), vec![bit]);
        drop(tracked);
        crate::watch::tests::free(p);
    }
}
