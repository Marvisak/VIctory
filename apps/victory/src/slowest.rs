//! Where a warm-up's time went: the time of each translation or pipeline
//! made, summed over the threads that made them, and the slowest few by
//! name, for the report `translate --cache` and the pre-build print at
//! their end.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How many of the slowest are named.
const KEPT: usize = 10;

/// Times of one kind of work.
#[derive(Default)]
pub(crate) struct Slowest {
    /// All of them, in µs, and how many.
    total_us: AtomicU64,
    count: AtomicU64,
    /// The slowest, slowest first.
    kept: Mutex<Vec<(Duration, String)>>,
}

impl Slowest {
    /// One took `time`; `name` names it (asked only if it is among the
    /// slowest).
    pub(crate) fn add(&self, time: Duration, name: impl FnOnce() -> String) {
        self.total_us.fetch_add(
            time.as_micros().try_into().unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.count.fetch_add(1, Ordering::Relaxed);
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if kept.len() == KEPT && kept.last().is_some_and(|l| l.0 >= time) {
            return;
        }
        kept.push((time, name()));
        kept.sort_by_key(|k| std::cmp::Reverse(k.0));
        kept.truncate(KEPT);
    }

    /// The time of all of them (summed over threads), and how many.
    pub(crate) fn total(&self) -> (Duration, u64) {
        (
            Duration::from_micros(self.total_us.load(Ordering::Relaxed)),
            self.count.load(Ordering::Relaxed),
        )
    }

    /// The report: a line for the total, then one per slowest.
    pub(crate) fn lines(&self, what: &str) -> Vec<String> {
        let (total, count) = self.total();
        let mut out = vec![format!(
            "  {what}: {count} in {:.1} s summed over threads ({:.0} ms each on average)",
            total.as_secs_f64(),
            total.as_secs_f64() * 1e3 / count.max(1) as f64
        )];
        for (time, name) in self.kept.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            out.push(format!("    {:>8.2} s  {name}", time.as_secs_f64()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// It sums every time, keeps the slowest [`KEPT`] by name, slowest
    /// first, and names only those.
    #[test]
    fn it_sums_all_and_names_the_slowest() {
        let s = Slowest::default();
        let mut named = 0;
        // Slowest first: the ones after the first KEPT are not named.
        for ms in (1..=30u64).rev() {
            s.add(Duration::from_millis(ms), || {
                named += 1;
                format!("item {ms}")
            });
        }
        let (total, count) = s.total();
        assert_eq!(count, 30);
        assert_eq!(total, Duration::from_millis(30 * 31 / 2));
        let lines = s.lines("pipelines");
        assert_eq!(lines.len(), 1 + KEPT);
        assert!(lines[1].ends_with("item 30"), "{}", lines[1]);
        assert!(lines[KEPT].ends_with("item 21"), "{}", lines[KEPT]);
        assert_eq!(named, KEPT, "only the slowest so far are named");
    }
}
