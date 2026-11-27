//! Host-memory and GPU VRAM admission and residency controller.
//!
//! Controls adaptive DXGI and system RAM budget scaling, texture admission,
//! spare block pools, and hysteresis-driven pressure transitions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::memory_budget::{GIB, MIB, MemorySnapshot, PressureHysteresis, Tier};

static HYSTERESIS: Mutex<PressureHysteresis> = Mutex::new(PressureHysteresis::new());
static PRESSURE: AtomicBool = AtomicBool::new(false);

/// Query whether residency throttling is active:
/// `VICTORY_RESIDENCY=auto|0|1` (auto-enabled on `Low` tier).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if let Ok(v) = std::env::var("VICTORY_RESIDENCY") {
            match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => return true,
                "0" | "false" | "no" | "off" => return false,
                "auto" => return tier() == Tier::Low,
                _ => {}
            }
        }
        tier() == Tier::Low
    })
}

fn override_tier() -> Option<Tier> {
    static TIER: OnceLock<Option<Tier>> = OnceLock::new();
    *TIER.get_or_init(|| {
        let value = std::env::var("VICTORY_MEMORY_TIER").ok()?;
        match value.trim().to_ascii_lowercase().as_str() {
            "low" | "l" | "0" => Some(Tier::Low),
            "balanced" | "b" | "1" => Some(Tier::Balanced),
            "full" | "f" | "2" => Some(Tier::Full),
            _ => None,
        }
    })
}

pub(crate) fn snapshot() -> MemorySnapshot {
    let device = crate::host::host().map(|h| &h.device);
    crate::memory_budget::sample(device).unwrap_or_else(MemorySnapshot::unknown)
}

pub(crate) fn tier() -> Tier {
    static CACHED: OnceLock<Tier> = OnceLock::new();
    *CACHED.get_or_init(|| override_tier().unwrap_or_else(|| snapshot().tier()))
}

pub(crate) fn pressured() -> bool {
    PRESSURE.load(Ordering::Relaxed)
}

/// Called once per frame / submission sweep to sample memory and update hysteresis.
pub(crate) fn refresh() {
    let snap = snapshot();
    let current_pressured = pressured();
    let instantaneous_vote = snap.instantaneous_pressure(current_pressured);

    let next_pressured = {
        let mut hyst = HYSTERESIS.lock().unwrap_or_else(|e| e.into_inner());
        hyst.update(instantaneous_vote)
    };

    if next_pressured != current_pressured {
        PRESSURE.store(next_pressured, Ordering::Relaxed);
        if next_pressured {
            tracing::warn!(
                vram_budget_mib = snap.vram_budget.unwrap_or(0) >> 20,
                vram_used_mib = snap.vram_used.unwrap_or(0) >> 20,
                ram_avail_mib = snap.ram_available >> 20,
                "memory pressure entered (2 consecutive frames)"
            );
        } else {
            tracing::info!(
                vram_budget_mib = snap.vram_budget.unwrap_or(0) >> 20,
                vram_used_mib = snap.vram_used.unwrap_or(0) >> 20,
                ram_avail_mib = snap.ram_available >> 20,
                "memory pressure cleared (2 consecutive frames)"
            );
        }
    }
}

pub(crate) fn admit_texture_pass() -> bool {
    !on() || snapshot().commit_available >= 512 * MIB
}

pub(crate) fn allow_staging_growth() -> bool {
    !on() || snapshot().commit_available >= GIB
}

pub(crate) fn texture_keep_ms() -> u64 {
    static KEEP: OnceLock<bool> = OnceLock::new();
    let keep = *KEEP.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_KEEP").is_none_or(|value| value != "0"));
    crate::memory_budget::texture_keep_ms(tier(), pressured(), keep)
}

pub(crate) fn inplace_cap() -> u64 {
    static OVERRIDE: OnceLock<Option<u64>> = OnceLock::new();
    let override_mb = *OVERRIDE.get_or_init(|| {
        std::env::var("VICTORY_INPLACE_MB")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
    });
    override_mb.map_or_else(|| crate::memory_budget::inplace_cap(tier()), |mb| mb.saturating_mul(MIB).max(MIB))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residency_tier_override() {
        assert!(matches!(tier(), Tier::Low | Tier::Balanced | Tier::Full));
    }
}
