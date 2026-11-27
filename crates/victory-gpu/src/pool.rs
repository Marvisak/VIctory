//! Spare and staging block counts and capacities scaled across memory tiers.

use crate::memory_budget::Tier;

pub(crate) fn spare_kept(tier: Tier) -> usize {
    crate::memory_budget::pool_spare_blocks(crate::residency::snapshot().vram_budget, tier)
}

pub(crate) fn staging_kept(tier: Tier) -> usize {
    crate::memory_budget::staging_blocks(crate::residency::snapshot().vram_budget, tier)
}

pub(crate) fn staging_bytes_kept(tier: Tier) -> u64 {
    crate::memory_budget::staging_bytes_kept(tier)
}

pub(crate) fn shadow_bytes_kept(tier: Tier) -> usize {
    crate::memory_budget::shadow_bytes_kept(tier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_budget::MIB;

    #[test]
    fn tier_scales_pool_and_staging_caps() {
        assert_eq!(staging_bytes_kept(Tier::Low), 16 * MIB);
        assert_eq!(staging_bytes_kept(Tier::Balanced), 32 * MIB);
        assert_eq!(staging_bytes_kept(Tier::Full), 64 * MIB);
        assert_eq!(shadow_bytes_kept(Tier::Low), (64 * MIB) as usize);
        assert_eq!(shadow_bytes_kept(Tier::Balanced), (256 * MIB) as usize);
        assert_eq!(shadow_bytes_kept(Tier::Full), (512 * MIB) as usize);
    }
}
