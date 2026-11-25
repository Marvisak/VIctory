//! What [`super::Lower::live_edges`] knows of the lanes along a path back
//! through phis: the last few branch conditions taken ([`Known`]), and the
//! sets of them each phi was reached with ([`Reached`]).

use std::collections::HashSet;

use crate::structure::Bool;

/// The conditions a path keeps; fewer conditions are weaker, so keeping only
/// the last few stays sound.
pub(super) const MAX_KNOWN: usize = 4;

/// A branch condition and the side a lane took.
pub(super) type Condition = (Bool, bool);

/// The last few branch conditions taken, in order (a value, not a heap
/// allocation: shaders with control flow push thousands of them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Known {
    conditions: [Condition; MAX_KNOWN],
    len: usize,
}

impl Known {
    /// Nothing known.
    pub(super) const NONE: Known = Known {
        conditions: [(Bool::FALSE, false); MAX_KNOWN],
        len: 0,
    };

    pub(super) fn get(&self) -> &[Condition] {
        &self.conditions[..self.len]
    }

    /// With `c` after the others, the oldest dropped past the limit.
    pub(super) fn and(mut self, c: Condition) -> Known {
        if self.len == MAX_KNOWN {
            self.conditions.copy_within(1.., 0);
            self.len -= 1;
        }
        self.conditions[self.len] = c;
        self.len += 1;
        self
    }

    /// The same conditions as a set: sorted, without repeats.
    fn set(self) -> Known {
        let mut sorted = self.conditions;
        sorted[..self.len].sort_unstable();
        sorted[..self.len].iter().fold(Known::NONE, |s, &c| {
            if s.get().last() == Some(&c) {
                s
            } else {
                s.and(c)
            }
        })
    }

    /// The subsets of a set, as sets.
    fn subsets(self) -> impl Iterator<Item = Known> {
        (0..1usize << self.len).map(move |m| {
            (0..self.len)
                .filter(|j| m >> j & 1 == 1)
                .fold(Known::NONE, |s, j| s.and(self.conditions[j]))
        })
    }
}

/// The sets of conditions a phi was reached with, as sets.
#[derive(Default)]
pub(super) struct Reached {
    sets: Vec<Known>,
    /// The same sets once there are [`Self::INDEXED`] of them: a set has at
    /// most that many subsets, so from then on looking each subset up costs
    /// less than a scan of the sets (a phi of some programs is reached with
    /// tens of thousands).
    index: HashSet<Known>,
}

impl Reached {
    const INDEXED: usize = 1 << MAX_KNOWN;

    /// Adds `known` unless an earlier set is a subset of it (it knows of no
    /// fewer lanes than before, so nothing new is reached): whether it was
    /// added.
    pub(super) fn add(&mut self, known: Known) -> bool {
        let covered = if self.sets.len() < Self::INDEXED {
            self.sets
                .iter()
                .any(|s| s.get().iter().all(|c| known.get().contains(c)))
        } else {
            known.set().subsets().any(|s| self.index.contains(&s))
        };
        if covered {
            return false;
        }
        self.sets.push(known.set());
        if self.sets.len() == Self::INDEXED {
            self.index.extend(self.sets.iter().copied());
        } else if self.sets.len() > Self::INDEXED {
            self.index.extend(self.sets.last().copied());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path's conditions from a seed: two to four of twelve, repeats
    /// possible, in any order.
    fn path(seed: &mut u64) -> Known {
        let mut next = || {
            *seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (*seed >> 33) as u32
        };
        let len = 2 + next() % 3;
        (0..len).fold(Known::NONE, |k, _| {
            let n = next() % 12;
            k.and((Bool::test(2 + n / 2), n % 2 == 1))
        })
    }

    /// Past the sets kept for a scan, the index finds the same covering
    /// sets as a scan of every earlier one.
    #[test]
    fn reached_sets_match_a_scan_past_the_index() {
        let (mut seed, mut reached, mut scanned) = (7, Reached::default(), Vec::<Known>::new());
        for _ in 0..4000 {
            let k = path(&mut seed);
            let new = !scanned
                .iter()
                .any(|s| s.get().iter().all(|c| k.get().contains(c)));
            if new {
                scanned.push(k);
            }
            assert_eq!(reached.add(k), new, "{:?}", k.get());
        }
        assert!(scanned.len() > 4 * Reached::INDEXED, "{}", scanned.len());
    }

    #[test]
    fn a_set_is_sorted_without_repeats_and_has_its_subsets() {
        let (a, b) = ((Bool::test(2), true), (Bool::test(3), false));
        let k = Known::NONE.and(b).and(a).and(b);
        assert_eq!(k.set().get(), [a, b]);
        let subsets: Vec<Known> = k.set().subsets().collect();
        let expect = [
            Known::NONE,
            Known::NONE.and(a),
            Known::NONE.and(b),
            Known::NONE.and(a).and(b),
        ];
        assert_eq!(subsets, expect);
    }
}
