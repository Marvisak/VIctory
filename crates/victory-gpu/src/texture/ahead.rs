pub(crate) fn limit() -> u64 {
    match crate::residency::tier() {
        crate::memory_budget::Tier::Low if crate::residency::pressured() => 16 * crate::memory_budget::MIB,
        crate::memory_budget::Tier::Low => 32 * crate::memory_budget::MIB,
        crate::memory_budget::Tier::Balanced => 64 * crate::memory_budget::MIB,
        crate::memory_budget::Tier::Full => 96 * crate::memory_budget::MIB,
    }
}

pub(crate) fn admit(in_flight: u64, bytes: u64) -> bool {
    crate::residency::allow_staging_growth() && in_flight.saturating_add(bytes) <= limit()
}
