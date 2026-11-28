//! Which logical processors share a core and a last-level cache, with no
//! host calls in it ([`super`] asks the system).
//!
//! A processor whose cores are alike can still be two halves: a Ryzen with
//! two chiplets has two last-level caches, and a thread that moves from one
//! half to the other finds none of its data there. Windows tells the cores
//! and the caches apart (`GetLogicalProcessorInformationEx`), though it
//! gives every such core the same efficiency class.

/// Logical processors of processor group 0, a bit each.
pub(super) type Mask = u64;

/// `RelationProcessorCore` and `RelationCache` of
/// `LOGICAL_PROCESSOR_RELATIONSHIP`.
const CORE: u32 = 0;
const CACHE: u32 = 2;

/// The fewest logical processors a half has for the split to be made.
const HALF_LEAST: u32 = 8;

/// The two halves of [`Topology::halves`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Halves {
    pub main: Mask,
    pub helpers: Mask,
}

/// The cores and the last-level caches of processor group 0.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Topology {
    /// Each physical core: its logical processors, its efficiency class.
    pub cores: Vec<(Mask, u8)>,
    /// Each level 3 cache: the logical processors that share it.
    pub l3: Vec<Mask>,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// The masks of group 0 among `count` `GROUP_AFFINITY` (16 bytes each: a
/// mask, then its group) from `at` in a record.
fn group0(record: &[u8], at: usize, count: u16) -> Mask {
    // (A system older than the count wrote zero there: one mask.)
    (0..usize::from(count.max(1)))
        .filter_map(|i| {
            Some((
                u64_at(record, at + 16 * i)?,
                u16_at(record, at + 16 * i + 8)?,
            ))
        })
        .filter(|&(_, group)| group == 0)
        .fold(0, |all, (mask, _)| all | mask)
}

impl Topology {
    /// From what `GetLogicalProcessorInformationEx(RelationAll)` wrote:
    /// records of a kind and a size each
    /// (`SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`). A core's record has its
    /// class at byte 9, its number of masks at 30 and the masks from 32; a
    /// cache's has its level at 8, the number at 38 and the masks from 40.
    pub(super) fn parse(bytes: &[u8]) -> Topology {
        let mut topology = Topology::default();
        let mut at = 0;
        while let (Some(kind), Some(size)) = (u32_at(bytes, at), u32_at(bytes, at + 4)) {
            let size = size as usize;
            let Some(record) = (size >= 8).then(|| bytes.get(at..at + size)).flatten() else {
                break;
            };
            match kind {
                CORE => {
                    if let (Some(&class), Some(count)) = (record.get(9), u16_at(record, 30)) {
                        let mask = group0(record, 32, count);
                        if mask != 0 {
                            topology.cores.push((mask, class));
                        }
                    }
                }
                CACHE if record.get(8) == Some(&3) => {
                    if let Some(count) = u16_at(record, 38) {
                        let mask = group0(record, 40, count);
                        if mask != 0 && !topology.l3.contains(&mask) {
                            topology.l3.push(mask);
                        }
                    }
                }
                _ => {}
            }
            at += size;
        }
        topology
            .cores
            .sort_unstable_by_key(|c| c.0.trailing_zeros());
        topology.l3.sort_unstable_by_key(|m| m.trailing_zeros());
        topology
    }

    /// The two halves of a processor whose cores are alike and which has
    /// several last-level caches. None unless the caches are apart, as large
    /// as one another and each half has at least [`HALF_LEAST`] logical
    /// processors (two chiplets of six cores: twelve and twelve).
    pub(super) fn halves(&self) -> Option<Halves> {
        let size = self.l3.first()?.count_ones();
        let alike = self.cores.windows(2).all(|w| w[0].1 == w[1].1);
        let apart =
            self.l3.iter().fold(0, |all, m| all | m).count_ones() == size * self.l3.len() as u32;
        if !alike || self.l3.len() < 2 || !apart || self.l3.iter().any(|m| m.count_ones() != size) {
            return None;
        }
        let (lower, upper) = self.l3.split_at(self.l3.len() / 2);
        let or = |caches: &[Mask]| caches.iter().fold(0, |all, m| all | m);
        let halves = Halves {
            main: or(lower),
            helpers: or(upper),
        };
        (halves.main.count_ones() >= HALF_LEAST && halves.helpers.count_ones() >= HALF_LEAST)
            .then_some(halves)
    }
}

/// The ids of the processor sets `(id, logical processor)` whose logical
/// processor is in `mask`.
pub(super) fn sets_in(mask: Mask, sets: &[(u32, u8)]) -> Vec<u32> {
    sets.iter()
        .filter(|s| s.1 < 64 && mask >> s.1 & 1 != 0)
        .map(|s| s.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A core's record, as the system writes it.
    fn core(mask: Mask, class: u8) -> Vec<u8> {
        let mut r = vec![0u8; 48];
        r[0..4].copy_from_slice(&CORE.to_le_bytes());
        r[4..8].copy_from_slice(&48u32.to_le_bytes());
        r[9] = class;
        r[30..32].copy_from_slice(&1u16.to_le_bytes());
        r[32..40].copy_from_slice(&mask.to_le_bytes());
        r
    }

    /// A cache's record: its level, its masks by group.
    fn cache(level: u8, masks: &[(Mask, u16)], count: u16) -> Vec<u8> {
        let mut r = vec![0u8; 40 + 16 * masks.len()];
        let size = r.len() as u32;
        r[0..4].copy_from_slice(&CACHE.to_le_bytes());
        r[4..8].copy_from_slice(&size.to_le_bytes());
        r[8] = level;
        r[38..40].copy_from_slice(&count.to_le_bytes());
        for (i, (mask, group)) in masks.iter().enumerate() {
            r[40 + 16 * i..48 + 16 * i].copy_from_slice(&mask.to_le_bytes());
            r[48 + 16 * i..50 + 16 * i].copy_from_slice(&group.to_le_bytes());
        }
        r
    }

    /// `cores` cores of two threads each, in caches of `per` cores.
    fn processor(cores: u32, per: u32, class: impl Fn(u32) -> u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        // Caches first and cores from the last down: the order is ours.
        for c in 0..cores.div_ceil(per) {
            let mask = (c * per..((c + 1) * per).min(cores)).fold(0, |m, k| m | 0b11 << (2 * k));
            bytes.extend(cache(3, &[(mask, 0)], 1));
            bytes.extend(cache(2, &[(mask & 0b11 << (2 * c * per), 0)], 1));
        }
        for k in (0..cores).rev() {
            bytes.extend(core(0b11 << (2 * k), class(k)));
        }
        bytes
    }

    #[test]
    fn twelve_cores_in_two_caches_are_two_halves() {
        let t = Topology::parse(&processor(12, 6, |_| 0));
        assert_eq!(t.cores.len(), 12);
        assert_eq!(t.cores[0], (0b11, 0));
        assert_eq!(t.l3, [0xfff, 0xfff << 12]);
        assert_eq!(
            t.halves(),
            Some(Halves {
                main: 0xfff,
                helpers: 0xfff << 12
            })
        );
        // Twelve cores in caches of three: two chiplets of two caches each.
        let four = Topology::parse(&processor(12, 3, |_| 0));
        assert_eq!(four.l3.len(), 4);
        assert_eq!(four.halves(), t.halves());
    }

    #[test]
    fn one_cache_or_two_kinds_of_core_are_not_split() {
        // Eight cores under one cache.
        assert_eq!(Topology::parse(&processor(8, 8, |_| 0)).halves(), None);
        // Two caches, but cores of two kinds.
        let mixed = Topology::parse(&processor(12, 6, |k| u8::from(k < 6)));
        assert_eq!(mixed.halves(), None);
        // Two caches of two cores each: too small to give one away.
        assert_eq!(Topology::parse(&processor(4, 2, |_| 0)).halves(), None);
        // Caches of different sizes.
        let mut uneven = Topology::parse(&processor(12, 6, |_| 0));
        uneven.l3 = vec![0xff, 0xffff << 8];
        assert_eq!(uneven.halves(), None);
        // Nothing known.
        assert_eq!(Topology::parse(&[]).halves(), None);
    }

    #[test]
    fn other_groups_short_records_and_repeats_are_left_out() {
        let mut bytes = cache(3, &[(0xf0, 1), (0x0f, 0)], 2);
        // The same cache told again.
        bytes.extend(cache(3, &[(0x0f, 0)], 1));
        // A system that wrote no count: one mask.
        bytes.extend(cache(3, &[(0xf00, 0)], 0));
        bytes.extend(core(0b11, 0));
        // A record of another kind, then one cut short.
        let mut other = core(0b1100, 0);
        other[0..4].copy_from_slice(&7u32.to_le_bytes());
        bytes.extend(other);
        bytes.extend(&core(0b1100, 0)[..20]);
        let t = Topology::parse(&bytes);
        assert_eq!(t.l3, [0x0f, 0xf00]);
        assert_eq!(t.cores, [(0b11, 0)]);
        // A record that says a size of nothing ends the reading.
        assert_eq!(Topology::parse(&[0; 16]), Topology::default());
    }

    #[test]
    fn sets_are_picked_by_their_logical_processor() {
        let sets: Vec<(u32, u8)> = (0..24).map(|i| (256 + i, i as u8)).collect();
        assert_eq!(sets_in(0b11 << 10, &sets), [266, 267]);
        assert_eq!(sets_in(0xfff << 12, &sets).len(), 12);
        assert!(sets_in(0, &sets).is_empty());
        assert!(sets_in(u64::MAX, &[(1, 200)]).is_empty());
    }
}
