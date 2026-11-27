//! Shared DXGI and system RAM memory budget controller with tiered scaling.
//!
//! Samples `IDXGIAdapter3::QueryVideoMemoryInfo` (`Budget`, `CurrentUsage`,
//! `AvailableForReservation`) and `GlobalMemoryStatusEx` / physical RAM,
//! classifying tiers:
//! - `Low`: `Budget <= 8 GiB || TotalRam <= 16 GiB`
//! - `Balanced`: `Budget <= 12 GiB || TotalRam <= 32 GiB`
//! - `Full`: `Budget >= 16 GiB && TotalRam >= 32 GiB`
//!
//! Includes 2-frame hysteresis pressure transitions and cap formulas.

pub(crate) const MIB: u64 = 1 << 20;
pub(crate) const GIB: u64 = 1 << 30;

/// Tier classification based on VRAM budget and system RAM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Tier {
    Low,
    Balanced,
    Full,
}

/// A point-in-time sample of DXGI video memory and system RAM status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemorySnapshot {
    pub(crate) vram_budget: Option<u64>,
    pub(crate) vram_used: Option<u64>,
    pub(crate) vram_available_reservation: Option<u64>,
    pub(crate) ram_total: u64,
    pub(crate) ram_available: u64,
    pub(crate) commit_limit: u64,
    pub(crate) commit_available: u64,
}

impl MemorySnapshot {
    pub(crate) fn unknown() -> Self {
        Self {
            vram_budget: None,
            vram_used: None,
            vram_available_reservation: None,
            ram_total: u64::MAX,
            ram_available: u64::MAX,
            commit_limit: u64::MAX,
            commit_available: u64::MAX,
        }
    }

    /// Classify into memory tier.
    pub(crate) fn tier(self) -> Tier {
        classify_tier(self.vram_budget, self.ram_total)
    }

    /// Render-target reserve: `clamp(Budget * 0.10, 512 MiB, 3072 MiB)`.
    pub(crate) fn render_target_reserve(self) -> u64 {
        render_target_reserve(self.vram_budget)
    }

    /// Texture admission ceiling: `0.78 * Budget` on `Low` tier,
    /// `min(0.90 * Budget, Budget - reserves)` on `Full`.
    pub(crate) fn texture_admission_ceiling(self, tier: Tier) -> u64 {
        texture_admission_ceiling(self.vram_budget, tier)
    }

    /// Instantaneous pressure check for a single sample:
    /// enter pressure at `> 0.82` VRAM or `< 20%` free RAM (`> 80%` used RAM).
    /// exit pressure at `< 0.70` VRAM and `>= 20%` free RAM.
    pub(crate) fn instantaneous_pressure(self, currently_pressured: bool) -> bool {
        let vram_ratio = self.vram_budget.map_or(0, |budget| {
            ratio(self.vram_used.unwrap_or(0), budget)
        });
        let ram_used_ratio = if self.ram_total == 0 || self.ram_total == u64::MAX {
            0
        } else {
            ratio(self.ram_total.saturating_sub(self.ram_available), self.ram_total)
        };

        if currently_pressured {
            // Exit pressure when VRAM < 0.70 AND free RAM >= 20% (ram_used_ratio <= 80%)
            vram_ratio >= 7_000 || ram_used_ratio > 8_000
        } else {
            // Enter pressure when VRAM > 0.82 OR free RAM < 20% (ram_used_ratio > 80%)
            vram_ratio > 8_200 || ram_used_ratio > 8_000
        }
    }
}

/// Ratio in basis points (10,000 = 100%).
pub(crate) fn ratio(used: u64, limit: u64) -> u64 {
    if limit == 0 {
        return 0;
    }
    (u128::from(used.min(limit)) * 10_000 / u128::from(limit)) as u64
}

/// Classify hardware memory into tiers:
/// - `Low`: `Budget <= 8 GiB || TotalRam <= 16 GiB`
/// - `Balanced`: `Budget <= 12 GiB || TotalRam <= 32 GiB`
/// - `Full`: `Budget >= 16 GiB && TotalRam >= 32 GiB`
pub(crate) fn classify_tier(vram_budget: Option<u64>, ram_total: u64) -> Tier {
    if ram_total <= 16 * GIB || vram_budget.is_some_and(|b| b <= 8 * GIB) {
        Tier::Low
    } else if vram_budget.is_none_or(|b| b >= 16 * GIB) && ram_total >= 32 * GIB {
        Tier::Full
    } else {
        Tier::Balanced
    }
}

/// Render-target reserve: `clamp(Budget * 0.10, 512 MiB, 3072 MiB)`.
pub(crate) fn render_target_reserve(vram_budget: Option<u64>) -> u64 {
    let budget = vram_budget.unwrap_or(16 * GIB);
    (budget / 10).clamp(512 * MIB, 3072 * MIB)
}

/// Texture admission ceiling:
/// - `Low`: `0.78 * Budget`
/// - `Balanced`: `Budget.saturating_sub(reserve)`
/// - `Full`: `min(0.90 * Budget, Budget - reserves)`
pub(crate) fn texture_admission_ceiling(vram_budget: Option<u64>, tier: Tier) -> u64 {
    let Some(budget) = vram_budget else {
        return u64::MAX;
    };
    let reserve = render_target_reserve(Some(budget));
    match tier {
        Tier::Low => ((budget as f64) * 0.78) as u64,
        Tier::Balanced => budget.saturating_sub(reserve),
        Tier::Full => {
            let ninety_pct = ((budget as f64) * 0.90) as u64;
            ninety_pct.min(budget.saturating_sub(reserve))
        }
    }
}

/// Pool spare blocks: `min(64, floor(Budget * 0.05 / 64 MiB))`.
pub(crate) fn pool_spare_blocks(vram_budget: Option<u64>, tier: Tier) -> usize {
    match tier {
        Tier::Low => 2,
        Tier::Balanced | Tier::Full => {
            let budget = vram_budget.unwrap_or(16 * GIB);
            let blocks = (budget / 20) / (64 * MIB);
            (blocks as usize).clamp(2, 64)
        }
    }
}

/// Staging blocks: `min(8, floor(Budget * 0.01 / block))` where block is 64 MiB.
pub(crate) fn staging_blocks(vram_budget: Option<u64>, tier: Tier) -> usize {
    match tier {
        Tier::Low => 2,
        Tier::Balanced | Tier::Full => {
            let budget = vram_budget.unwrap_or(16 * GIB);
            let blocks = (budget / 100) / (64 * MIB);
            (blocks as usize).clamp(2, 8)
        }
    }
}

/// Staging bytes kept based on tier.
pub(crate) fn staging_bytes_kept(tier: Tier) -> u64 {
    match tier {
        Tier::Low => 16 * MIB,
        Tier::Balanced => 32 * MIB,
        Tier::Full => 64 * MIB,
    }
}

/// Shadow bytes kept based on tier.
pub(crate) fn shadow_bytes_kept(tier: Tier) -> usize {
    match tier {
        Tier::Low => (64 * MIB) as usize,
        Tier::Balanced => (256 * MIB) as usize,
        Tier::Full => (512 * MIB) as usize,
    }
}

/// Texture idle timeouts (in milliseconds):
/// Scales down on `Low` tier (`KEPT_MS=1_000`, `LONG_MS=500`, `NORMAL_MS=250`)
/// while preserving full-tier timeouts on `>= 16 GiB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextureTimeouts {
    pub(crate) kept_ms: u64,
    pub(crate) long_ms: u64,
    pub(crate) normal_ms: u64,
}

pub(crate) fn texture_idle_timeouts(tier: Tier) -> TextureTimeouts {
    match tier {
        Tier::Low => TextureTimeouts {
            kept_ms: 1_000,
            long_ms: 500,
            normal_ms: 250,
        },
        Tier::Balanced => TextureTimeouts {
            kept_ms: 60_000,
            long_ms: 10_000,
            normal_ms: 2_500,
        },
        Tier::Full => TextureTimeouts {
            kept_ms: 120_000,
            long_ms: 30_000,
            normal_ms: 5_000,
        },
    }
}

pub(crate) fn texture_keep_ms(tier: Tier, pressured: bool, keep: bool) -> u64 {
    let timeouts = texture_idle_timeouts(tier);
    if !keep {
        return timeouts.normal_ms;
    }
    match (tier, pressured) {
        (Tier::Low, true) => timeouts.normal_ms,
        (Tier::Low, false) => timeouts.kept_ms,
        (Tier::Balanced, true) => timeouts.long_ms,
        (Tier::Balanced, false) => timeouts.kept_ms,
        (Tier::Full, true) => timeouts.long_ms,
        (Tier::Full, false) => timeouts.kept_ms,
    }
}

pub(crate) fn inplace_cap(tier: Tier) -> u64 {
    match tier {
        Tier::Low => 512 * MIB,
        Tier::Balanced => 2048 * MIB,
        Tier::Full => 4096 * MIB,
    }
}

pub(crate) fn worker_count(tier: Tier) -> usize {
    let half = std::thread::available_parallelism().map_or(2, |n| n.get() / 2);
    match tier {
        Tier::Low => half.clamp(2, 4),
        Tier::Balanced | Tier::Full => half.clamp(2, 8),
    }
}

/// 2-frame hysteresis state machine for memory pressure.
/// Requires 2 consecutive frames voting for a state change before switching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PressureHysteresis {
    pressured: bool,
    pending_state: bool,
    consecutive_frames: u8,
}

impl Default for PressureHysteresis {
    fn default() -> Self {
        Self::new()
    }
}

impl PressureHysteresis {
    pub(crate) const fn new() -> Self {
        Self {
            pressured: false,
            pending_state: false,
            consecutive_frames: 0,
        }
    }

    #[cfg(test)]
    pub(crate) const fn is_pressured(&self) -> bool {
        self.pressured
    }

    /// Update hysteresis with instantaneous pressure vote for the current frame.
    /// Returns the active pressure state after the update.
    pub(crate) fn update(&mut self, vote: bool) -> bool {
        if vote == self.pressured {
            // Vote agrees with current active state; reset pending transition
            self.pending_state = vote;
            self.consecutive_frames = 0;
        } else {
            // Vote differs from current active state
            if self.pending_state == vote {
                self.consecutive_frames = self.consecutive_frames.saturating_add(1);
                if self.consecutive_frames >= 2 {
                    self.pressured = vote;
                    self.consecutive_frames = 0;
                }
            } else {
                self.pending_state = vote;
                self.consecutive_frames = 1;
            }
        }
        self.pressured
    }
}

/// Query system memory and DXGI adapter memory.
#[cfg(windows)]
pub(crate) fn sample(device: Option<&windows::Win32::Graphics::Direct3D12::ID3D12Device>) -> Option<MemorySnapshot> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: zeroed bytes are a valid value for this structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` has its length set and is a valid out pointer.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return None;
    }

    let (vram_budget, vram_used, vram_avail_res) = if let Some(dev) = device {
        sample_dxgi_video_memory(dev)
    } else {
        (None, None, None)
    };

    Some(MemorySnapshot {
        vram_budget,
        vram_used,
        vram_available_reservation: vram_avail_res,
        ram_total: status.ullTotalPhys,
        ram_available: status.ullAvailPhys,
        commit_limit: status.ullTotalPageFile,
        commit_available: status.ullAvailPageFile,
    })
}

/// Query `IDXGIAdapter3::QueryVideoMemoryInfo` for local memory.
#[cfg(windows)]
fn sample_dxgi_video_memory(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
) -> (Option<u64>, Option<u64>, Option<u64>) {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory2, DXGI_CREATE_FACTORY_DEBUG, DXGI_CREATE_FACTORY_FLAGS,
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIFactory4,
    };
    // SAFETY: GetAdapterLuid is a standard D3D12 device query.
    let luid = unsafe { device.GetAdapterLuid() };
    let debug = cfg!(feature = "diagnostics")
        && std::env::var_os("VICTORY_D3D12_DEBUG").is_some_and(|v| v != "0");
    let flags = if debug {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        DXGI_CREATE_FACTORY_FLAGS(0)
    };

    // SAFETY: standard DXGI factory creation.
    let factory: IDXGIFactory4 = match unsafe { CreateDXGIFactory2(flags) } {
        Ok(f) => f,
        Err(_) => return (None, None, None),
    };

    // SAFETY: querying adapter by LUID.
    let adapter: IDXGIAdapter3 = match unsafe { factory.EnumAdapterByLuid(luid) } {
        Ok(a) => a,
        Err(_) => return (None, None, None),
    };

    let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    // SAFETY: valid query on IDXGIAdapter3.
    let res = unsafe {
        adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
    };
    if res.is_ok() {
        (
            Some(info.Budget),
            Some(info.CurrentUsage),
            Some(info.AvailableForReservation),
        )
    } else {
        (None, None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_classification_scales_accurately() {
        // Low: Budget <= 8 GiB || TotalRam <= 16 GiB
        assert_eq!(classify_tier(Some(8 * GIB), 16 * GIB), Tier::Low);
        assert_eq!(classify_tier(Some(8 * GIB), 32 * GIB), Tier::Low);
        assert_eq!(classify_tier(Some(16 * GIB), 16 * GIB), Tier::Low);
        assert_eq!(classify_tier(Some(6 * GIB), 64 * GIB), Tier::Low);

        // Balanced: Budget <= 12 GiB || TotalRam <= 32 GiB (and not Low)
        assert_eq!(classify_tier(Some(12 * GIB), 32 * GIB), Tier::Balanced);
        assert_eq!(classify_tier(Some(10 * GIB), 64 * GIB), Tier::Balanced);
        assert_eq!(classify_tier(Some(16 * GIB), 24 * GIB), Tier::Balanced);

        // Full: Budget >= 16 GiB && TotalRam >= 32 GiB
        assert_eq!(classify_tier(Some(16 * GIB), 32 * GIB), Tier::Full);
        assert_eq!(classify_tier(Some(24 * GIB), 64 * GIB), Tier::Full);
        assert_eq!(classify_tier(None, 64 * GIB), Tier::Full);
    }

    #[test]
    fn cap_formulas_conform_to_spec() {
        let budget_8g = 8 * GIB;
        let budget_16g = 16 * GIB;
        let budget_24g = 24 * GIB;

        // Render-target reserve: clamp(Budget * 0.10, 512 MiB, 3072 MiB)
        assert_eq!(render_target_reserve(Some(budget_8g)), (8 * GIB) / 10); // 819.2 MiB
        assert_eq!(render_target_reserve(Some(4 * GIB)), 512 * MIB); // clamped min
        assert_eq!(render_target_reserve(Some(64 * GIB)), 3072 * MIB); // clamped max

        // Texture admission ceiling
        // Low tier: 0.78 * Budget
        let low_ceiling = texture_admission_ceiling(Some(budget_8g), Tier::Low);
        assert_eq!(low_ceiling, ((budget_8g as f64) * 0.78) as u64);

        // Full tier: min(0.90 * Budget, Budget - reserves)
        let full_ceiling = texture_admission_ceiling(Some(budget_24g), Tier::Full);
        let expected_res = render_target_reserve(Some(budget_24g));
        let expected_90 = ((budget_24g as f64) * 0.90) as u64;
        assert_eq!(full_ceiling, expected_90.min(budget_24g - expected_res));

        // Pool spare blocks and staging blocks
        assert_eq!(pool_spare_blocks(Some(budget_8g), Tier::Low), 2);
        assert_eq!(staging_blocks(Some(budget_8g), Tier::Low), 2);

        let full_spare = pool_spare_blocks(Some(budget_16g), Tier::Full);
        assert_eq!(full_spare, ((16 * GIB / 20) / (64 * MIB)) as usize);

        let full_staging = staging_blocks(Some(budget_16g), Tier::Full);
        assert_eq!(full_staging, ((16 * GIB / 100) / (64 * MIB)) as usize);

        // Texture timeouts scale down on Low tier
        let low_timeouts = texture_idle_timeouts(Tier::Low);
        assert_eq!(low_timeouts.kept_ms, 1_000);
        assert_eq!(low_timeouts.long_ms, 500);
        assert_eq!(low_timeouts.normal_ms, 250);

        let full_timeouts = texture_idle_timeouts(Tier::Full);
        assert_eq!(full_timeouts.kept_ms, 120_000);
        assert_eq!(full_timeouts.long_ms, 30_000);
        assert_eq!(full_timeouts.normal_ms, 5_000);
    }

    #[test]
    fn two_frame_hysteresis_transitions() {
        let mut hyst = PressureHysteresis::new();
        assert!(!hyst.is_pressured());

        // 1 frame of pressure: should NOT switch
        assert!(!hyst.update(true));
        assert!(!hyst.is_pressured());

        // 2 consecutive frames of pressure: should switch to pressured
        assert!(hyst.update(true));
        assert!(hyst.is_pressured());

        // Continued pressure stays pressured
        assert!(hyst.update(true));
        assert!(hyst.is_pressured());

        // 1 frame of calm: should NOT switch back
        assert!(hyst.update(false));
        assert!(hyst.is_pressured());

        // Glitch back to pressure: resets calm counter
        assert!(hyst.update(true));
        assert!(hyst.is_pressured());

        // 1 frame of calm: should NOT switch back
        assert!(hyst.update(false));
        assert!(hyst.is_pressured());

        // 2nd consecutive frame of calm: should switch back to unpressured
        assert!(!hyst.update(false));
        assert!(!hyst.is_pressured());
    }
}
