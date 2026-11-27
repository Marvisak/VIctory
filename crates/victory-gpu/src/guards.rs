use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub(crate) const TDR_MS: u32 = 2_000;
pub(crate) const BUDGET_HIGH: f64 = 0.90;
pub(crate) const BUDGET_LOW: f64 = 0.85;

const HEADROOM_MS: u32 = 500;
const MIN_WAIT_MS: u32 = 25;
const DEFAULT_TDR_S: u32 = TDR_MS / 1000;
const MAX_TDR_S: u32 = 60;
const MAX_TDR_WAIT_MS: u32 = MAX_TDR_S * 1000 - HEADROOM_MS;
const WAIT_SLICE_MS: u32 = 50;
pub(crate) const FALLBACK_MS: u32 = HEADROOM_MS;

const ASSUMED_RAM_MIB: u64 = 16 << 10;
const CONSTRAINED_RAM_MIB: u64 = 16 << 10;
const DEFAULT_HOST_BUDGET_MIB: u64 = 6 << 10;
const MIN_HOST_BUDGET_MIB: u64 = 256;

fn mib_to_mb(mib: u64) -> f64 {
    mib as f64 * 1024.0 * 1024.0 / 1e6
}

pub(crate) fn parse_size(text: &str) -> Option<u64> {
    text.trim().parse().ok().filter(|&n| n > 0)
}

pub(crate) fn submission_wait_ms(requested: Option<u32>, tdr_delay_s: Option<u32>) -> u32 {
    let tdr = tdr_delay_s.unwrap_or(DEFAULT_TDR_S).clamp(1, MAX_TDR_S);
    let default = (tdr * 1000).saturating_sub(HEADROOM_MS).max(MIN_WAIT_MS);
    requested
        .unwrap_or(default)
        .clamp(MIN_WAIT_MS, MAX_TDR_WAIT_MS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    Done,
    TimedOut,
    Failed,
}

impl Wait {
    pub(crate) fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::Done,
            258 => Self::TimedOut,
            _ => Self::Failed,
        }
    }

    pub(crate) fn fallback(self) -> Fallback {
        match self {
            Self::Done => Fallback::None,
            Self::TimedOut => Fallback::Poll,
            Self::Failed => Fallback::Stop,
        }
    }
}

/// Waits in slices until `deadline`, completing only when `completed` holds.
/// A single slice timing out says nothing about the fence, so it keeps going;
/// only a completion or a failed wait ends the loop early.
pub(crate) fn wait_deadline(
    now: impl Fn() -> Instant,
    deadline: Instant,
    slice: Duration,
    mut completed: impl FnMut() -> bool,
    mut wait: impl FnMut(Duration) -> Wait,
) -> Wait {
    loop {
        if completed() {
            return Wait::Done;
        }
        let Some(left) = deadline.checked_duration_since(now()) else {
            return Wait::TimedOut;
        };
        if left.is_zero() {
            return Wait::TimedOut;
        }
        // `WaitForSingleObject` treats zero as a poll, not an infinite wait;
        // give a sub-millisecond remainder one whole millisecond.
        let millis = left.min(slice).as_millis().max(1) as u32;
        match wait(Duration::from_millis(u64::from(millis))) {
            Wait::Done => {
                if completed() {
                    return Wait::Done;
                }
            }
            Wait::Failed => return Wait::Failed,
            Wait::TimedOut => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fallback {
    None,
    Poll,
    Stop,
}

/// An explicit request is honoured as-is. Without one, the conservative
/// 6 GiB cap applies only to a constrained machine; larger cards use the
/// DXGI budget reported to [`over_budget`].
pub(crate) fn host_budget_mib(requested: Option<u64>, installed_mib: Option<u64>) -> Option<u64> {
    let installed = installed_mib.unwrap_or(ASSUMED_RAM_MIB);
    match requested {
        Some(mib) => Some(mib.max(MIN_HOST_BUDGET_MIB)),
        None if installed <= CONSTRAINED_RAM_MIB => {
            Some(DEFAULT_HOST_BUDGET_MIB.min(installed / 2).max(MIN_HOST_BUDGET_MIB))
        }
        None => None,
    }
}

pub(crate) fn over_budget(used: f64, budget: f64, cap: Option<f64>) -> u64 {
    if !used.is_finite() || !budget.is_finite() || budget <= 0.0 {
        return 0;
    }
    let effective = match cap {
        Some(cap) if cap.is_finite() && cap > 0.0 => budget.min(cap),
        _ => budget,
    };
    if used <= effective * BUDGET_HIGH {
        return 0;
    }
    ((used - effective * BUDGET_LOW) * 1e6).max(0.0) as u64
}

#[cfg(windows)]
pub(crate) fn submission_wait() -> Duration {
    static WAIT: OnceLock<Duration> = OnceLock::new();
    *WAIT.get_or_init(|| {
        let requested = std::env::var("VICTORY_D3D12_SUBMISSION_WAIT_MS")
            .ok()
            .and_then(|text| parse_size(&text))
            .and_then(|ms| u32::try_from(ms).ok());
        let tdr = crate::tdr::Tdr::read().delay;
        Duration::from_millis(u64::from(submission_wait_ms(requested, tdr)))
    })
}

#[cfg(windows)]
pub(crate) fn wait_slice() -> Duration {
    Duration::from_millis(u64::from(WAIT_SLICE_MS))
}

#[cfg(windows)]
pub(crate) fn fallback_wait() -> Duration {
    Duration::from_millis(u64::from(FALLBACK_MS))
}

#[cfg(windows)]
pub(crate) fn host_budget_mb() -> Option<f64> {
    static BUDGET: OnceLock<Option<f64>> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let requested = std::env::var("VICTORY_HOST_BUDGET_MIB")
            .ok()
            .and_then(|text| parse_size(&text));
        host_budget_mib(requested, installed_ram_mib()).map(mib_to_mb)
    })
}

#[cfg(windows)]
fn installed_ram_mib() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is initialized and has its length set.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
    ok.then_some(status.ullTotalPhys >> 20)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn submission_wait_scales_with_a_longer_tdr_delay() {
        assert_eq!(submission_wait_ms(None, None), TDR_MS - HEADROOM_MS);
        assert_eq!(submission_wait_ms(None, Some(10)), 10_000 - HEADROOM_MS);
        assert_eq!(submission_wait_ms(None, Some(60)), MAX_TDR_WAIT_MS);
        assert_eq!(submission_wait_ms(Some(10_000), None), 10_000);
        assert_eq!(submission_wait_ms(Some(60_000), Some(10)), MAX_TDR_WAIT_MS);
        assert_eq!(submission_wait_ms(Some(1), None), MIN_WAIT_MS);
        assert_eq!(submission_wait_ms(Some(900), None), 900);
        assert_eq!(submission_wait_ms(Some(900), Some(1)), 900);
        assert_eq!(submission_wait_ms(Some(900), Some(0)), 900);
        const { assert!(TDR_MS - HEADROOM_MS + FALLBACK_MS <= TDR_MS) };
    }

    #[test]
    fn invalid_overrides_fall_back_to_the_default() {
        assert_eq!(parse_size(" 750 "), Some(750));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("-1"), None);
        assert_eq!(parse_size("many"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn wait_results_choose_their_fallback() {
        assert_eq!(Wait::from_raw(0), Wait::Done);
        assert_eq!(Wait::from_raw(258), Wait::TimedOut);
        assert_eq!(Wait::from_raw(128), Wait::Failed);
        assert_eq!(Wait::from_raw(u32::MAX), Wait::Failed);
        assert_eq!(Wait::Done.fallback(), Fallback::None);
        assert_eq!(Wait::TimedOut.fallback(), Fallback::Poll);
        assert_eq!(Wait::Failed.fallback(), Fallback::Stop);
    }

    #[test]
    fn wait_loop_slices_until_the_deadline() {
        let base = Instant::now() + Duration::from_secs(1);
        let ticks = Cell::new(0u64);
        let slices = RefCell::new(Vec::new());
        let outcome = wait_deadline(
            || base + Duration::from_millis(ticks.get()),
            base + Duration::from_millis(150),
            Duration::from_millis(50),
            || false,
            |slice| {
                slices.borrow_mut().push(slice);
                ticks.set(ticks.get() + 50);
                Wait::TimedOut
            },
        );
        assert_eq!(outcome, Wait::TimedOut);
        assert_eq!(ticks.get(), 150);
        assert_eq!(slices.into_inner(), vec![Duration::from_millis(50); 3]);
    }

    #[test]
    fn wait_loop_finishes_on_a_late_completion() {
        let base = Instant::now() + Duration::from_secs(1);
        let ticks = Cell::new(0u64);
        let done = Cell::new(false);
        let outcome = wait_deadline(
            || base + Duration::from_millis(ticks.get() * 50),
            base + Duration::from_millis(500),
            Duration::from_millis(50),
            || done.get(),
            |_| {
                ticks.set(ticks.get() + 1);
                if ticks.get() == 3 {
                    done.set(true);
                    Wait::Done
                } else {
                    Wait::TimedOut
                }
            },
        );
        assert_eq!(outcome, Wait::Done);
        assert_eq!(ticks.get(), 3);
    }

    #[test]
    fn wait_loop_stops_on_a_failed_wait() {
        let base = Instant::now() + Duration::from_secs(1);
        let ticks = Cell::new(0u64);
        let outcome = wait_deadline(
            || base + Duration::from_millis(ticks.get() * 50),
            base + Duration::from_millis(500),
            Duration::from_millis(50),
            || false,
            |_| {
                ticks.set(ticks.get() + 1);
                Wait::Failed
            },
        );
        assert_eq!(outcome, Wait::Failed);
        assert_eq!(ticks.get(), 1);
    }

    #[test]
    fn wait_loop_waits_at_least_a_millisecond_for_a_submillisecond_remainder() {
        let base = Instant::now() + Duration::from_secs(1);
        let ticks = Cell::new(0u64);
        let slices = RefCell::new(Vec::new());
        let outcome = wait_deadline(
            || base + Duration::from_millis(ticks.get()),
            base + Duration::from_micros(500),
            Duration::from_millis(50),
            || false,
            |slice| {
                slices.borrow_mut().push(slice);
                ticks.set(1);
                Wait::TimedOut
            },
        );
        assert_eq!(outcome, Wait::TimedOut);
        assert_eq!(slices.into_inner(), vec![Duration::from_millis(1)]);
    }

    #[test]
    fn an_explicit_host_budget_is_not_clamped_to_six_gib() {
        assert_eq!(host_budget_mib(Some(16 * 1024), Some(64 * 1024)), Some(16 * 1024));
        assert_eq!(host_budget_mib(Some(32 * 1024), Some(32 * 1024)), Some(32 * 1024));
        assert_eq!(host_budget_mib(Some(64), Some(16 * 1024)), Some(MIN_HOST_BUDGET_MIB));
    }

    #[test]
    fn the_conservative_cap_only_applies_to_constrained_machines() {
        assert_eq!(host_budget_mib(None, None), Some(DEFAULT_HOST_BUDGET_MIB));
        assert_eq!(host_budget_mib(None, Some(512)), Some(MIN_HOST_BUDGET_MIB));
        assert_eq!(host_budget_mib(None, Some(4 * 1024)), Some(2 * 1024));
        assert_eq!(host_budget_mib(None, Some(32 * 1024)), None);
        assert_eq!(host_budget_mib(None, Some(u64::MAX)), None);
    }

    #[test]
    fn a_mib_budget_reaches_the_dxgi_mb_scale() {
        assert!((mib_to_mb(1) - 1.048_576).abs() < f64::EPSILON);
        assert!((mib_to_mb(DEFAULT_HOST_BUDGET_MIB) - 6_442.450_944).abs() < f64::EPSILON);
    }

    #[test]
    fn the_effective_budget_caps_eviction_and_ignores_unknown_values() {
        assert_eq!(over_budget(100.0, 1000.0, Some(1000.0)), 0);
        let over = over_budget(100.0, 1000.0, Some(105.0));
        assert!(over > 0, "the host cap brings forward eviction");
        assert_eq!(over_budget(100.0, 1000.0, Some(0.0)), 0);
        assert_eq!(over_budget(100.0, 1000.0, None), 0);
        assert!(over_budget(950.0, 1000.0, None) > 0, "the DXGI budget still caps eviction");
        assert_eq!(over_budget(100.0, f64::NAN, None), 0);
        assert_eq!(over_budget(f64::INFINITY, 1000.0, None), 0);
        assert_eq!(over_budget(100.0, 1000.0, Some(f64::NAN)), 0);
    }
}
