use std::collections::BTreeMap;

pub(crate) struct DirtyRanges<const LIMIT: usize> {
    ends: BTreeMap<u64, (u64, u64)>,
}

impl<const LIMIT: usize> DirtyRanges<LIMIT> {
    pub(crate) const fn new() -> Self {
        assert!(LIMIT > 0);
        Self {
            ends: BTreeMap::new(),
        }
    }

    pub(crate) fn insert(&mut self, mut start: u64, mut end: u64, generation: u64) -> bool {
        if start >= end {
            return false;
        }
        let mut newest = generation;
        if let Some((&left, &(left_end, left_gen))) = self.ends.range(..=start).next_back()
            && left_end >= start
        {
            if left_end >= end {
                return false;
            }
            start = left;
            newest = newest.max(left_gen);
            self.ends.remove(&left);
        }
        while let Some((&right, &(right_end, right_gen))) = self.ends.range(start..=end).next() {
            end = end.max(right_end);
            newest = newest.max(right_gen);
            self.ends.remove(&right);
        }
        if self.ends.len() < LIMIT {
            self.ends.insert(start, (end, newest));
            return false;
        }
        let left = self
            .ends
            .range(..start)
            .next_back()
            .map(|(&s, &(e, g))| (s, start - e, g));
        let right = self
            .ends
            .range(end..)
            .next()
            .map(|(&s, &(e, g))| (s, e, s - end, g));
        match (left, right) {
            (Some((s, gap, g)), r) if r.is_none_or(|(_, _, other, _)| gap <= other) => {
                self.ends.insert(s, (end, newest.max(g)));
            }
            (_, Some((s, e, _, g))) => {
                self.ends.remove(&s);
                self.ends.insert(start, (e, newest.max(g)));
            }
            _ => unreachable!("a full nonempty map has a neighbour"),
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn into_ranges(self) -> impl ExactSizeIterator<Item = (u64, u64)> {
        self.ends.into_iter().map(|(start, (end, _))| (start, end))
    }

    #[cfg(test)]
    pub(crate) fn overlaps(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return false;
        }
        self.ends
            .range(..end)
            .next_back()
            .is_some_and(|(&a, &(b, _))| a < end && b > start)
    }

    pub(crate) fn newest_since(&self, start: u64, end: u64, generation: u64) -> Option<u64> {
        if start >= end {
            return None;
        }
        let mut newest = None;
        for (_, &(b, epoch)) in self.ends.range(..end).rev() {
            if b <= start {
                break;
            }
            if epoch > generation {
                newest = newest.max(Some(epoch));
            }
        }
        newest
    }
}

impl<const LIMIT: usize> Default for DirtyRanges<LIMIT> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_order_writes_bridge_both_neighbours_without_losing_holes() {
        let mut ranges = DirtyRanges::<4>::new();
        for (start, end) in [(10, 20), (40, 50), (25, 30), (20, 25), (29, 41), (0, 2), (10, 15)] {
            assert!(!ranges.insert(start, end, 1));
        }
        assert_eq!(ranges.into_ranges().collect::<Vec<_>>(), [(0, 2), (10, 50)]);
    }

    #[test]
    fn page_written_invalidation_preserves_bytes_at_capacity() {
        let mut written = [false; 509];
        let mut ranges = DirtyRanges::<7>::new();
        for i in 0..130 {
            let start = i * 47 % 503;
            let end = start + (i % 5) + 1;
            ranges.insert(start as u64, end as u64, i as u64 + 1);
            written[start..end].fill(true);
        }
        let held: Vec<_> = ranges.into_ranges().collect();
        assert!(held.len() <= 7);
        assert!(held.windows(2).all(|pair| pair[0].1 < pair[1].0));
        for (i, dirty) in written.into_iter().enumerate() {
            if dirty {
                assert!(held.iter().any(|&(a, b)| (a..b).contains(&(i as u64))));
            }
        }
    }

    #[test]
    fn overlap_check_finds_a_covered_texture_span() {
        let mut ranges = DirtyRanges::<4>::new();
        ranges.insert(100, 120, 1);
        ranges.insert(200, 240, 2);
        assert!(ranges.overlaps(110, 130));
        assert!(ranges.overlaps(150, 210));
        assert!(!ranges.overlaps(120, 150));
        assert!(!ranges.overlaps(150, 150));
        assert_eq!(ranges.newest_since(110, 130, 0), Some(1));
        assert_eq!(ranges.newest_since(110, 130, 1), None);
        assert_eq!(ranges.newest_since(150, 210, 1), Some(2));
    }
}
