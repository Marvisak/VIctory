use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, IUnknown, IUnknown_Vtbl, implement, interface};

const MIN_LOCAL_BYTES: u64 = 8 << 30;
/// ReBAR bytes held by our live GPU upload buffers. A resource keeps the
/// guard below until it is destroyed, so the count follows every clone.
static LIVE: AtomicU64 = AtomicU64::new(0);
const LIVE_GUARD: GUID = GUID::from_u128(0x5b54_2d7e_369d_4c1a_b1d9_6c73_8f42_a901);

/// A private COM interface: a resource can hold one as private data without
/// the bytes guard being tied to any of the resource's Rust clones.
#[interface("5b542d7e-369d-4c1a-b1d9-6c738f42a901")]
unsafe trait ILiveGuard: IUnknown {}

/// Attached to a GPU upload heap resource: its destruction releases the
/// bytes against `VICTORY_REBAR_MB`, however many clones the resource has.
#[implement(ILiveGuard)]
struct LiveGuard(u64);

impl ILiveGuard_Impl for LiveGuard_Impl {}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        LIVE.fetch_sub(self.0, Ordering::Relaxed);
    }
}

fn live_guard(bytes: u64) -> IUnknown {
    LIVE.fetch_add(bytes, Ordering::Relaxed);
    let guard: ILiveGuard = LiveGuard(bytes).into();
    guard.into()
}

pub(crate) fn live_bytes() -> u64 {
    LIVE.load(Ordering::Relaxed)
}

/// Counts a buffer's bytes until the resource (and all its clones) die.
pub(crate) fn track(resource: &ID3D12Resource, bytes: u64) {
    let guard = live_guard(bytes);
    // SAFETY: a valid GUID and COM interface; the resource holds its own
    // reference until it is destroyed, which drops `LiveGuard_Impl`.
    if let Err(e) = unsafe { resource.SetPrivateDataInterface(&LIVE_GUARD, &guard) } {
        tracing::warn!(error = %e, "no ReBAR lifetime tracking for a buffer");
    }
}

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_REBAR"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn eligible(
    on: bool,
    supported: bool,
    budget: Option<u64>,
    size: u64,
    limit: u64,
    live: u64,
    used: u64,
    kept: u64,
) -> bool {
    let Some(budget) = budget else {
        return false;
    };
    if !on || !supported || size == 0 || budget <= MIN_LOCAL_BYTES {
        return false;
    }
    let headroom = (budget - budget / 10).saturating_sub(used).saturating_sub(kept);
    size <= limit.saturating_sub(live) && size <= headroom
}

pub(crate) fn supported(device: &ID3D12Device) -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
        // SAFETY: the output matches the feature and its size.
        let ok = unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS16,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        }
        .is_ok()
            && options.GPUUploadHeapSupported.as_bool();
        tracing::info!(supported = ok, "ReBAR GPU upload heaps");
        ok
    })
}

/// `live` counts what the host has already put in GPU upload heaps: both
/// against `VICTORY_REBAR_MB` and against the local budget's headroom, so
/// many small buffers cannot each pass a cap meant for all of them.
fn fits(size: u64, budget: u64, used: u64, limit: u64, live: u64) -> bool {
    eligible(true, true, Some(budget), size, limit, live, used, live)
}

pub(crate) fn placement(size: u64, device: &ID3D12Device) -> bool {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    if !enabled() || !supported(device) {
        return false;
    }
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_REBAR_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(2048)
            .min(12288)
            << 20
    });
    let Some([(used_mb, budget_mb), _]) = crate::host::video_memory() else {
        return false;
    };
    let bytes = |mb: f64| (mb.max(0.0) * 1_000_000.0) as u64;
    fits(size, bytes(budget_mb), bytes(used_mb), limit, live_bytes())
}

#[cfg(test)]
mod tests {
    use super::{eligible, fits, live_bytes, live_guard};
    use std::sync::Mutex;

    const LIMIT: u64 = 1 << 30;
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn non_rebar_adapters_fall_back() {
        assert!(!eligible(true, false, Some(16 << 30), 4096, LIMIT, 0, 0, 0));
    }

    #[test]
    fn an_eight_gib_adapter_falls_back() {
        assert!(!eligible(true, true, Some(8 << 30), 4096, LIMIT, 0, 0, 0));
        assert!(eligible(true, true, Some(12 << 30), 4096, LIMIT, 0, 0, 0));
    }

    #[test]
    fn a_switched_off_or_zero_size_falls_back() {
        assert!(!eligible(false, true, Some(12 << 30), 4096, LIMIT, 0, 0, 0));
        assert!(!eligible(true, true, Some(12 << 30), 0, LIMIT, 0, 0, 0));
        assert!(!eligible(true, true, None, 4096, LIMIT, 0, 0, 0));
    }

    #[test]
    fn headroom_and_live_bytes_bound_the_placement() {
        let budget = 12 << 30;
        assert!(!eligible(true, true, Some(budget), LIMIT + 1, LIMIT, 0, 0, 0));
        assert!(!eligible(true, true, Some(budget), 1 << 30, LIMIT, 0, budget, 0));
        assert!(!eligible(true, true, Some(budget), 1 << 30, LIMIT, 0, 0, 10 << 30));
        assert!(eligible(true, true, Some(budget), 1 << 28, LIMIT, 0, budget / 4, 0));
    }

    /// `VICTORY_REBAR_MB=1`: one 1 MiB buffer fills the cap; 16 live ones
    /// cannot each take another MiB, however much local budget there is.
    #[test]
    fn the_rebar_limit_is_cumulative_across_live_buffers() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let limit = 1 << 20;
        let budget = 12 << 30;
        assert!(fits(limit, budget, 0, limit, 0));
        assert!(!fits(limit, budget, 0, limit, limit));
        assert!(!fits(1 << 16, budget, 0, limit, 16 * limit));
        assert!(fits(limit, budget, 0, 2 * limit, limit));
        assert!(!fits(limit, budget, budget, 2 * limit, 0));
    }

    #[test]
    fn live_rebar_bytes_are_released_with_the_resource_guard() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let before = live_bytes();
        let guard = live_guard(4096);
        assert_eq!(live_bytes(), before + 4096);
        drop(guard);
        assert_eq!(live_bytes(), before);
    }
}
