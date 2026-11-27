use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Counter {
    FrameWait,
    LateFrameWait,
    Submission,
    SlowSubmission,
}

impl Counter {
    const N: usize = 4;

    fn index(self) -> usize {
        self as usize
    }
}

static COUNT: [AtomicU64; Counter::N] = [const { AtomicU64::new(0) }; Counter::N];
#[cfg(feature = "diagnostics")]
static LAST: [AtomicU64; Counter::N] = [const { AtomicU64::new(0) }; Counter::N];

fn on(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v != "0")
}

fn flag(name: &str, default: bool) -> bool {
    std::env::var_os(name).map_or(default, |v| on(Some(v.as_os_str())))
}

fn parse_ms(value: Option<&str>, default: u64) -> Duration {
    let ms = value
        .map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default);
    Duration::from_millis(ms)
}

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        flag("VICTORY_STATS_SHM", cfg!(feature = "diagnostics")) || flag("VICTORY_TELEMETRY", false)
    })
}

#[cfg(feature = "diagnostics")]
pub(crate) fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_GPU_TIMING"))
}

#[cfg(feature = "diagnostics")]
pub(crate) fn frame_log() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| flag("VICTORY_FRAME_LOG", cfg!(feature = "diagnostics")))
}

pub(crate) fn late_waits() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| flag("VICTORY_LATE_WAITS", false))
}

pub(crate) fn slow_submit() -> Duration {
    static LIMIT: OnceLock<Duration> = OnceLock::new();
    *LIMIT.get_or_init(|| parse_ms(std::env::var("VICTORY_SLOW_SUBMIT_MS").ok().as_deref(), 250))
}

pub(crate) fn add(counter: Counter, n: u64) {
    if enabled() {
        COUNT[counter.index()].fetch_add(n, Relaxed);
    }
}

#[cfg(any(test, feature = "diagnostics"))]
fn delta(source: &AtomicU64, last: &AtomicU64) -> u64 {
    let n = source.load(Relaxed);
    n.saturating_sub(last.fetch_max(n, Relaxed))
}

#[cfg(feature = "diagnostics")]
pub(crate) fn log_count(source: &AtomicU64, last: &AtomicU64) -> u64 {
    if !enabled() {
        return source.swap(0, Relaxed);
    }
    delta(source, last)
}

#[cfg(feature = "diagnostics")]
pub(crate) fn sample() -> [u64; Counter::N] {
    std::array::from_fn(|i| log_count(&COUNT[i], &LAST[i]))
}

#[cfg(feature = "diagnostics")]
pub(crate) fn log() {
    if enabled() {
        let [waits, late, submits, slow] = sample();
        tracing::info!(waits, late, submits, slow, "frame and timing guard counts");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_defaults_are_opt_in_and_bounded() {
        assert!(!on(None));
        assert!(on(Some(std::ffi::OsStr::new("1"))));
        assert!(!on(Some(std::ffi::OsStr::new("0"))));
        assert_eq!(parse_ms(None, 250), Duration::from_millis(250));
        assert_eq!(parse_ms(Some(" 5 "), 250), Duration::from_millis(5));
        assert_eq!(parse_ms(Some("0"), 250), Duration::from_millis(250));
        assert_eq!(parse_ms(Some("bad"), 250), Duration::from_millis(250));
        assert_eq!(Counter::SlowSubmission.index(), 3);
    }

    #[test]
    fn monitor_deltas_are_monotonic_and_do_not_reset_the_source() {
        let source = AtomicU64::new(7);
        let last = AtomicU64::new(0);
        assert_eq!(delta(&source, &last), 7);
        assert_eq!(delta(&source, &last), 0);
        source.store(11, Relaxed);
        assert_eq!(delta(&source, &last), 4);
        assert_eq!(source.load(Relaxed), 11);
    }
}
