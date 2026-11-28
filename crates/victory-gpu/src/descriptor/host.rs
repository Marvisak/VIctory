//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, VIEWS, View, view_at};
use crate::host::host;

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count` (none in sampler heaps): [`DYNAMIC`]
    /// where the host takes them, else [`MIN_DYNAMIC`].
    dynamic: u32,
    sampler: bool,
    /// The record (view index and generation) each slot was last made from.
    made: Mutex<Vec<u64>>,
    /// Buffer slots whose title record was valid but whose loose host copy
    /// was denied by the ordinary nonlocal budget. The epoch prevents every
    /// draw from attempting the same allocation while that budget is shut.
    failed_buffers: Mutex<std::collections::HashMap<u32, u64>>,
    /// Sampled texture slots observed in a heap-indexing shader that could
    /// not get a host copy. Read marks report a slot only once, so these
    /// need an explicit retry when admission or existing-heap space returns.
    observed_texture_misses: Mutex<std::collections::HashMap<u32, TextureMiss>>,
    /// A shader-used texture slot that stayed null, with the result of its
    /// last host materialization attempt. Diagnostics only; no allocation is
    /// retried merely to classify a failed view.
    #[cfg(feature = "diagnostics")]
    failed_textures: Mutex<std::collections::HashMap<u32, &'static str>>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    /// With it the view's own copy of title memory, if it has one.
    kept: Mutex<Vec<Kept>>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
    /// Slot pairs kept for host copies' views (`crate::memory`), past the
    /// dynamic slots and below the null buffer views: the next never
    /// taken, those given back, and where they end.
    pairs: Mutex<(u32, Vec<u32>)>,
    pairs_end: u32,
    /// View heaps: the view-stride table translated shaders read
    /// (`victory_shader::emit::VIEW_STRIDES`), a dword per title slot: the
    /// byte stride of the buffer view there, 0 for none. Mapped at the
    /// address kept beside it.
    strides: Option<(ID3D12Resource, usize)>,
    /// View heaps: the view-mark table translated shaders read
    /// (`victory_shader::emit`, "Write marks"), two dwords per slot as the
    /// stride table has one: the mark bit and phase of the unordered buffer
    /// view there, `VIEW_MARK_NONE` for none. Mapped at the address kept
    /// beside it.
    marks: Option<(ID3D12Resource, usize)>,
    /// Per title slot and per dynamic slot (tables made afresh lie there,
    /// and shaders index the view addresses at the slots their tables are
    /// in), the console descriptor's dwords 0-3 as far as the host knows
    /// them (shaders reading a descriptor as data, `crate::memory`;
    /// [`write_address`]): the buffer, its mapping and host address, made on
    /// first use.
    addresses: std::sync::OnceLock<Option<(ID3D12Resource, usize, u64)>>,
    /// What the title slots view that title memory feeds (`crate::bindless`).
    pub(crate) viewed: std::sync::Arc<crate::bindless::Viewed>,
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// Retry attempts, restored host views, and stale records discarded since
/// the previous diagnostic report.
#[cfg(feature = "diagnostics")]
static OBSERVED_TEXTURE_RETRY: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
pub(crate) fn take_observed_texture_retry() -> [u64; 3] {
    std::array::from_fn(|i| OBSERVED_TEXTURE_RETRY[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

#[derive(Clone, Copy)]
struct TextureMiss {
    /// The view generation that failed. A title rewrite invalidates it.
    key: u64,
    /// Last existing-heap free epoch at which this slot was attempted.
    reuse_epoch: u64,
    /// Whether ordinary streaming admission was open at that attempt.
    room_open: bool,
}

impl TextureMiss {
    fn ready(self, reuse_epoch: u64, room_open: bool) -> bool {
        reuse_epoch > self.reuse_epoch || room_open && !self.room_open
    }
}

fn title_record_matches(handle: u64, key: u64) -> bool {
    let mut record = [0u8; 12];
    if !crate::host::read_into(handle as *const u8, &mut record) {
        return false;
    }
    let tag = u32::from_le_bytes(record[0..4].try_into().unwrap());
    let view = u32::from_le_bytes(record[4..8].try_into().unwrap());
    let generation = u32::from_le_bytes(record[8..12].try_into().unwrap());
    tag == VIEW_TAG && (u64::from(generation) << 32 | u64::from(view)) == key
}

/// A slot can be null while the shader still runs. Count only slots named by
/// a table's used-resource list or by observed heap reads, rather than every
/// null descriptor parked in a large shader-visible heap.
#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum UsedNullSource { Table, ObservedHeap }

#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsedNullReason { Budget, InvalidOrEmpty, Other }

#[cfg(feature = "diagnostics")]
static USED_NULL: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];
#[cfg(feature = "diagnostics")]
static USED_NULL_WARNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Only actual shader uses classified as ordinary-budget failures enter
/// this bounded per-interval tally. Keying by title range instead of slot
/// keeps repeated descriptors of the same bytes together.
#[cfg(feature = "diagnostics")]
const USED_BUDGET_RANGE_CAP: usize = 4096;

#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct UsedBudgetKey {
    source: UsedNullSource,
    address: u64,
    len: u64,
}

#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Debug)]
struct UsedBudgetStat {
    uses: u64,
    example_index: u32,
    example_pipeline: usize,
    example_root_slot: u8,
}

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct UsedBudgetRanges {
    ranges: std::collections::HashMap<UsedBudgetKey, UsedBudgetStat>,
    total_uses: u64,
    overflow_uses: u64,
}

#[cfg(feature = "diagnostics")]
struct UsedBudgetReport {
    total_uses: u64,
    tracked_distinct_ranges: usize,
    overflow_uses: u64,
    top: Vec<(UsedBudgetKey, UsedBudgetStat)>,
}

#[cfg(feature = "diagnostics")]
impl UsedBudgetRanges {
    fn record(&mut self, key: UsedBudgetKey, index: u32, pipeline: usize, root_slot: u8, cap: usize) {
        self.total_uses = self.total_uses.saturating_add(1);
        if let Some(stat) = self.ranges.get_mut(&key) {
            stat.uses = stat.uses.saturating_add(1);
        } else if self.ranges.len() < cap {
            self.ranges.insert(key, UsedBudgetStat {
                uses: 1, example_index: index, example_pipeline: pipeline,
                example_root_slot: root_slot,
            });
        } else {
            self.overflow_uses = self.overflow_uses.saturating_add(1);
        }
    }

    fn drain_top(&mut self, limit: usize) -> UsedBudgetReport {
        let state = std::mem::take(self);
        let tracked_distinct_ranges = state.ranges.len();
        let mut top: Vec<_> = state.ranges.into_iter().collect();
        top.sort_unstable_by(|(a_key, a), (b_key, b)| {
            b.uses.cmp(&a.uses)
                .then_with(|| a_key.address.cmp(&b_key.address))
                .then_with(|| a_key.len.cmp(&b_key.len))
        });
        top.truncate(limit);
        UsedBudgetReport {
            total_uses: state.total_uses, tracked_distinct_ranges,
            overflow_uses: state.overflow_uses, top,
        }
    }
}

#[cfg(feature = "diagnostics")]
static USED_BUDGET_RANGES: std::sync::OnceLock<Mutex<UsedBudgetRanges>> = std::sync::OnceLock::new();

/// Table/observed-heap by host-copy-unavailable, placed-unavailable,
/// format-incompatible, other, and invalid/empty title addresses. Counts
/// uses, not unique descriptor slots.
#[cfg(feature = "diagnostics")]
static USED_NULL_TEXTURE: [std::sync::atomic::AtomicU64; 10] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 10];
#[cfg(feature = "diagnostics")]
static USED_NULL_TEXTURE_WARNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// A statically named table buffer is needed by a draw even when the ordinary
// loose-copy budget is closed. Keep this reserve separate from vertex/index
// admission: at most 64 MiB of cached copies, and 256 new attempts in one
// submission epoch. The Shadow remains pinned by the descriptor and by any
// submitted list that uses it. At high nonlocal usage, shadow admission may
// only reuse an already touched write-back range.
const TABLE_PRIORITY_WINDOW: u64 = 256 << 10;
const TABLE_PRIORITY_MAX_SPAN: u64 = 2 * TABLE_PRIORITY_WINDOW;
const TABLE_PRIORITY_LIVE_BYTES: u64 = 64 << 20;
const TABLE_PRIORITY_ATTEMPTS_PER_EPOCH: usize = 256;

#[derive(Default)]
struct TablePriority {
    epoch: u64,
    attempted: std::collections::HashSet<(u64, u64)>,
    live: std::collections::HashMap<usize, (std::sync::Weak<crate::shadow::Shadow>, u64)>,
}

static TABLE_PRIORITY: std::sync::OnceLock<Mutex<TablePriority>> = std::sync::OnceLock::new();

#[cfg(feature = "diagnostics")]
static TABLE_PRIORITY_STATS: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];
#[cfg(feature = "diagnostics")]
static TABLE_PRIORITY_DENIAL_COUNTS: [std::sync::atomic::AtomicU64; 7] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 7];
#[cfg(feature = "diagnostics")]
static TABLE_PRIORITY_DENIAL_WARNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "diagnostics")]
static TABLE_PRIORITY_REMADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "diagnostics")]
static TABLE_PRIORITY_REMAKE_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A shader-statically-used buffer view spanning exactly two aligned title
/// windows. These need a shared copy before an earlier one-window table view
/// can take either half; later mirroring reuses the preseeded Shadow.
pub(crate) fn table_union_candidate(view: &View) -> Option<(u64, u64)> {
    let (address, len) = match view {
        View::Constant(address, size) => (*address, u64::from(*size)),
        _ => buffer_range(view)?,
    };
    let end = address.checked_add(len).filter(|_| len > 0)?;
    let start = address & !(TABLE_PRIORITY_WINDOW - 1);
    let span = end.checked_next_multiple_of(TABLE_PRIORITY_WINDOW)?.checked_sub(start)?;
    let stride = u64::from(view_stride(view));
    (span == TABLE_PRIORITY_MAX_SPAN && stride > 0 && (address - start).is_multiple_of(stride))
        .then_some((start, len))
}

/// Uses the same bounded, overlap-safe admission as a table view that first
/// failed ordinary mirroring. A preseed never rewrites a host descriptor.
pub(crate) fn preseed_table_union(view: &View) -> bool {
    table_union_candidate(view).is_some() && table_priority_copy(view).is_some()
}

#[cfg(feature = "diagnostics")]
fn note_table_priority_denial(address: u64, len: u64, reason: Option<crate::shadow::TableUnionDenial>) {
    use crate::shadow::TableUnionDenial as D;
    use std::sync::atomic::Ordering::Relaxed;
    let group = match reason {
        Some(D::InvalidRange | D::TitleFree | D::StrideMismatch) => 0,
        Some(D::Cached) => 1,
        Some(D::PartialRegisteredOverlap | D::LooseOverlap) => 2,
        Some(D::RootLooseCap | D::MultiwindowCap | D::TableCap) => 3,
        Some(D::ReuseDisabled | D::LocalHeadroom | D::NoReusableSpan) => 4,
        Some(D::RegisteredUnavailable | D::ReusedAllocationFailed | D::HostAllocationFailed) => 5,
        None => 6,
    };
    TABLE_PRIORITY_DENIAL_COUNTS[group].fetch_add(1, Relaxed);
    let seen = TABLE_PRIORITY_DENIAL_WARNED.fetch_add(1, Relaxed) + 1;
    if seen <= 16 || seen.is_power_of_two() {
        tracing::warn!(seen, ?reason, address = format_args!("{address:#x}"), len,
            detail = %crate::shadow::diagnose_missed_buffer(address, len, true),
            "statically used table priority copy denied");
    }
}

fn table_priority_copy(view: &View) -> Option<std::sync::Arc<crate::shadow::Shadow>> {
    let (address, len) = match view {
        View::Constant(address, size) => (*address, u64::from(*size)),
        _ => buffer_range(view)?,
    };
    let end = address.checked_add(len).filter(|_| len > 0)?;
    let start = address & !(TABLE_PRIORITY_WINDOW - 1);
    let stride = u64::from(view_stride(view));
    let span = end.checked_next_multiple_of(TABLE_PRIORITY_WINDOW)?.checked_sub(start)?;
    if span > TABLE_PRIORITY_MAX_SPAN || stride == 0 || !(address - start).is_multiple_of(stride) {
        return None;
    }

    // A concurrent draw may have made a coherent covering copy after the
    // ordinary mirror failed. Reusing it needs no priority allocation and
    // must remain possible even when the table-priority cap is full.
    if let Some(shadow) = crate::shadow::existing_required_table_copy(address, len)
        && shadow.offset(address) % stride == 0
    {
        #[cfg(feature = "diagnostics")]
        TABLE_PRIORITY_STATS[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return Some(shadow);
    }

    let mut state = TABLE_PRIORITY.get_or_init(|| Mutex::new(TablePriority::default()))
        .lock().unwrap_or_else(|e| e.into_inner());
    let epoch = crate::shadow::admission_epoch();
    if state.epoch != epoch {
        state.epoch = epoch;
        state.attempted.clear();
    }
    state.live.retain(|_, (shadow, _)| shadow.strong_count() > 0);
    let live_bytes: u64 = state.live.values().map(|(_, bytes)| *bytes).sum();
    // A registered, cached, or ordinary-admitted Shadow may already cover
    // this view. Only a newly created priority loose copy spends this cap.
    let priority_creation_allowed = live_bytes.checked_add(span)
        .is_some_and(|bytes| bytes <= TABLE_PRIORITY_LIVE_BYTES);
    let key = (start, span);
    if state.attempted.contains(&key) || state.attempted.len() >= TABLE_PRIORITY_ATTEMPTS_PER_EPOCH {
        return None;
    }
    state.attempted.insert(key);
    #[cfg(feature = "diagnostics")]
    TABLE_PRIORITY_STATS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // This is the same overlap-safe union admission as required vertex and
    // index data. A covering Shadow is reused, so SRV/CBV reads and UAV
    // write-back cannot diverge into independent copies of the title bytes.
    #[cfg(feature = "diagnostics")]
    let (candidate, reason) = crate::shadow::diagnose_required_table_union(
        address, len, priority_creation_allowed);
    #[cfg(not(feature = "diagnostics"))]
    let candidate = crate::shadow::find_required_union_with_room(
        address, len, priority_creation_allowed);
    let stride_mismatch = candidate.as_ref().is_some_and(|(shadow, _)| shadow.offset(address) % stride != 0);
    let Some((shadow, created_priority)) = candidate.filter(|(shadow, _)| shadow.offset(address) % stride == 0) else {
        #[cfg(feature = "diagnostics")]
        {
            TABLE_PRIORITY_STATS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if matches!(reason, Some(crate::shadow::TableUnionDenial::TableCap)) {
                TABLE_PRIORITY_STATS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            note_table_priority_denial(address, len, if stride_mismatch {
                Some(crate::shadow::TableUnionDenial::StrideMismatch)
            } else { reason });
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = stride_mismatch;
        return None;
    };
    if created_priority {
        debug_assert_eq!(shadow.allocation_len(), span);
        state.live.insert(std::sync::Arc::as_ptr(&shadow) as usize,
            (std::sync::Arc::downgrade(&shadow), span));
    }
    #[cfg(feature = "diagnostics")]
    TABLE_PRIORITY_STATS[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(shadow)
}

/// Uses, not distinct slots, since the last five-second diagnostic report.
#[cfg(feature = "diagnostics")]
pub(crate) fn log_used_null() {
    use std::sync::atomic::Ordering::Relaxed;
    let counts: [u64; 6] = std::array::from_fn(|i| USED_NULL[i].swap(0, Relaxed));
    if counts.iter().any(|&n| n > 0) {
        tracing::warn!(table_budget = counts[0], table_invalid_or_empty = counts[1],
            table_other = counts[2], observed_heap_budget = counts[3],
            observed_heap_invalid_or_empty = counts[4], observed_heap_other = counts[5],
            "actual shader-use sites with null host buffer descriptors (events, not unique slots)");
    } else {
        tracing::info!(table_budget = 0, table_invalid_or_empty = 0,
            table_other = 0, observed_heap_budget = 0,
            observed_heap_invalid_or_empty = 0, observed_heap_other = 0,
            "actual shader-use sites with null host buffer descriptors (events, not unique slots)");
    }
    if let Some(tallies) = USED_BUDGET_RANGES.get() {
        let report = tallies.lock().unwrap_or_else(|e| e.into_inner()).drain_top(16);
        if report.total_uses > 0 {
            let top: Vec<String> = report.top.iter().map(|(key, stat)| {
                format!("{:?} address={:#x} len={} uses={} example_index={} example_pipeline={:#x} example_root_slot={}",
                    key.source, key.address, key.len, stat.uses,
                    stat.example_index, stat.example_pipeline, stat.example_root_slot)
            }).collect();
            tracing::warn!(budget_uses = report.total_uses,
                tracked_distinct_ranges = report.tracked_distinct_ranges,
                overflow_uses = report.overflow_uses,
                distinct_complete = report.overflow_uses == 0, ?top,
                "actual shader-used budget-null buffer ranges (top 16 per interval)");
        }
    }
    let textures: [u64; 10] = std::array::from_fn(|i| USED_NULL_TEXTURE[i].swap(0, Relaxed));
    if textures.iter().any(|&n| n > 0) {
        tracing::warn!(table_host_copy_unavailable = textures[0], table_placed_texture_unavailable = textures[1],
            table_format_incompatible = textures[2], table_other = textures[3], table_invalid_or_empty = textures[4],
            observed_heap_host_copy_unavailable = textures[5], observed_heap_placed_texture_unavailable = textures[6],
            observed_heap_format_incompatible = textures[7], observed_heap_other = textures[8],
            observed_heap_invalid_or_empty = textures[9],
            "shader-used texture slots with null host views (events, not unique slots)");
    } else {
        tracing::info!(table_host_copy_unavailable = 0, table_placed_texture_unavailable = 0,
            table_format_incompatible = 0, table_other = 0, table_invalid_or_empty = 0,
            observed_heap_host_copy_unavailable = 0, observed_heap_placed_texture_unavailable = 0,
            observed_heap_format_incompatible = 0, observed_heap_other = 0,
            observed_heap_invalid_or_empty = 0,
            "shader-used texture slots with null host views (events, not unique slots)");
    }
    let [attempted, denied_copy, denied_cap, admitted, reused] =
        std::array::from_fn(|i| TABLE_PRIORITY_STATS[i].swap(0, Relaxed));
    let remade = TABLE_PRIORITY_REMADE.swap(0, Relaxed);
    let remake_failed = TABLE_PRIORITY_REMAKE_FAILED.swap(0, Relaxed);
    if attempted + denied_cap + remade + remake_failed + reused > 0 {
        let (live_copies, live_bytes) = TABLE_PRIORITY.get().map_or((0, 0), |state| {
            let state = state.lock().unwrap_or_else(|e| e.into_inner());
            (state.live.len(), state.live.values().map(|(_, bytes)| *bytes).sum::<u64>())
        });
        tracing::info!(attempted_unique_windows = attempted, admitted_unique_windows = admitted,
            denied_unique_windows = denied_copy, cap_denied_uses = denied_cap,
            remade_slots = remade, remake_failed_slots = remake_failed, reused_slots = reused,
            live_copies, live_bytes,
            "statically used table buffers recovered through bounded priority copies");
    }
    let [invalid, cached, overlap, reserve, memory, allocation, unknown] =
        std::array::from_fn(|i| TABLE_PRIORITY_DENIAL_COUNTS[i].swap(0, Relaxed));
    if invalid + cached + overlap + reserve + memory + allocation + unknown > 0 {
        tracing::info!(invalid, cached, overlap, reserve, memory, allocation, unknown,
            "statically used table priority denial branches");
    }
}

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!("{:#x} past its end (slot {})", va - start - bytes, (va - start) / step)
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out seconds into the world (every world run on an RX 9070 XT,
/// each a device removal shortly after; #59).
const DYNAMIC: u32 = 1 << 18;

/// The dynamic slots of a heap whose host would not take [`DYNAMIC`] more
/// (a capped heap, NVIDIA): the title's slots keep their room first.
const MIN_DYNAMIC: u32 = 1 << 16;

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        // Every slot a shader can index (title slots, also past a capped
        // heap, and dynamic ones): shaders read the table as a root view,
        // which has no bounds, at indices clamped below `HEAP_VIEWS`.
        let strides = (!sampler)
            .then(|| host.upload_buffer(u64::from(count.max(victory_shader::emit::HEAP_VIEWS)) * 4))
            .flatten()
            .map(|(buffer, data)| (buffer, data as usize));
        let marks = (!sampler)
            .then(|| {
                let bytes = u64::from(count.max(victory_shader::emit::HEAP_VIEWS)) * 8;
                let (buffer, data) = host.upload_buffer(bytes)?;
                crate::host::name(&buffer, || "view marks".into());
                // Every slot unmarked (all ones) until a view is made there.
                // SAFETY: our mapped buffer of `bytes` bytes.
                unsafe { std::ptr::write_bytes(data, 0xFF, bytes as usize) };
                Some((buffer, data as usize))
            })
            .flatten();
        // The larger dynamic range only where the host takes it on top of
        // every title slot; else the smaller one, and the cap after that.
        // Translated shaders clamp their indices below `HEAP_VIEWS` or
        // `HEAP_SAMPLERS`: slots up to there exist and stay null.
        let (views, samplers) = (victory_shader::emit::HEAP_VIEWS, victory_shader::emit::HEAP_SAMPLERS);
        let (heap, count, extra) = if sampler {
            match make(count.max(samplers)) {
                Ok(heap) => (heap, count, 0),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else if let Ok(heap) = make((count + DYNAMIC).max(views)) {
            (heap, count, DYNAMIC)
        } else {
            // shortcut: a host that caps its heaps below `HEAP_VIEWS` (NVIDIA)
            // faults on an index past them, as before the clamp.
            match make(count + MIN_DYNAMIC) {
                Ok(heap) => (heap, count, MIN_DYNAMIC),
                Err(e) if count + MIN_DYNAMIC > PORTABLE_VIEWS => {
                    // shortcut: descriptors past the host's limit are left
                    // out; a title that reaches them needs remapping.
                    tracing::warn!(error = %e, count, "host view heap capped at {PORTABLE_VIEWS}");
                    (make(PORTABLE_VIEWS).ok()?, PORTABLE_VIEWS - MIN_DYNAMIC, MIN_DYNAMIC)
                }
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let (gpu, total) = unsafe {
            (heap.GetGPUDescriptorHandleForHeapStart().ptr, heap.GetDesc().NumDescriptors)
        };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(total) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let h = HostHeap {
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made: Mutex::new(vec![0; count as usize]),
            failed_buffers: Mutex::new(std::collections::HashMap::new()),
            observed_texture_misses: Mutex::new(std::collections::HashMap::new()),
            #[cfg(feature = "diagnostics")]
            failed_textures: Mutex::new(std::collections::HashMap::new()),
            kept: Mutex::new(vec![None; (count + extra) as usize]),
            next: Mutex::new((0, vec![false; extra as usize])),
            pairs: Mutex::new((count + extra, Vec::new())),
            pairs_end: match victory_shader::emit::NULL_BUFFER_VIEW {
                null if !sampler && null > count + extra && null < total => null - 1,
                _ => total,
            },
            strides,
            marks,
            addresses: std::sync::OnceLock::new(),
            viewed: Default::default(),
        };
        if !sampler {
            crate::bindless::register(&h.viewed);
        }
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, total) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(ms = started.elapsed().as_millis(), "host descriptor heap made null");
        // What shaders read instead of a slot without a buffer view where
        // they take one at a computed index (`victory_shader::emit`, "Buffer
        // views at computed heap indices"): a null raw SRV, and UAV below it.
        let null = victory_shader::emit::NULL_BUFFER_VIEW;
        if !sampler && null > count + extra && null < total {
            h.raw_view_at(null, None, 0, 0, false);
            h.raw_view_at(null - 1, None, 0, 0, true);
        } else if !sampler {
            tracing::warn!(count, total, "no room for the null buffer views: computed indices of slots without a buffer view read what is there");
        }
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: BLOCK,
                    ..Default::default()
                })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device.CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device.CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

/// A sampler heap of our own, for the lists that set a root signature whose
/// sampler heap is indexed directly (`SAMPLER_HEAP_DIRECTLY_INDEXED`) while
/// the title binds no sampler heap: D3D12 requires one to be bound first
/// (the debug layer's id 1419), and breaking that rule removes the device on
/// NVIDIA. Slot 0 holds a point sampler, so an indexed read of it returns
/// something defined rather than whatever the memory held.
pub(crate) fn fallback_samplers() -> Option<&'static HostHeap> {
    static HEAP: std::sync::OnceLock<Option<HostHeap>> = std::sync::OnceLock::new();
    HEAP.get_or_init(|| {
        let heap = HostHeap::new(true, 1)?;
        null_sampler(heap.cpu(0));
        Some(heap)
    })
    .as_ref()
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let n = n as usize;
        // Search from `next` on, then from the start (extending past cursor by
        // n - 1 so contiguous runs crossing the cursor are not missed).
        let cursor = *next as usize;
        let end = self.dynamic as usize;
        let second_end = cursor.saturating_add(n.saturating_sub(1)).min(end);
        let starts = (cursor..end).chain(0..second_end);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(n, "no free dynamic descriptor slots: a table is not made"));
            return None;
        };
        taken[first..first + n].fill(true);
        *next = ((first + n) % self.dynamic as usize) as u32;
        Some(self.count + first as u32)
    }

    /// Two consecutive slots (view heaps only) kept until
    /// [`HostHeap::free_pair`]: past the dynamic slots where the heap has
    /// room (a host copy's views, `crate::memory`, for as long as it
    /// lives), else dynamic ones.
    pub(crate) fn pair(&self) -> Option<u32> {
        if self.sampler {
            return None;
        }
        {
            let mut pairs = self.pairs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(index) = pairs.1.pop() {
                return Some(index);
            }
            if pairs.0 + 2 <= self.pairs_end {
                pairs.0 += 2;
                return Some(pairs.0 - 2);
            }
        }
        self.dynamic(2)
    }

    /// Gives back a pair [`HostHeap::pair`] returned.
    pub(crate) fn free_pair(&self, index: u32) {
        if index >= self.count + self.dynamic {
            self.pairs.lock().unwrap_or_else(|e| e.into_inner()).1.push(index);
        } else {
            self.release(index, 2);
        }
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at + n).min(self.dynamic) as usize;
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            slots.fill(false);
        }
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot.
    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let (kept, mark) = make_marked(view, self.cpu(index));
        if kept.is_none() {
            null_for(view, self.cpu(index));
        }
        self.keep(index, kept.flatten());
        self.set_stride(index, Some(view));
        self.set_mark(index, mark);
        // A table made afresh: shaders reading a view's address there (e.g.
        // the BVH builder's base) index the view addresses at this slot.
        self.note_address(index, Some(view));
    }

    /// Makes slot `index` (a dynamic one) a raw view, shader resource or
    /// unordered access (`uav`), of `bytes` of `buffer` from byte `first`
    /// (both multiples of 4), which it holds; a null one for no buffer, or
    /// for unordered access to a buffer shaders cannot write.
    pub(crate) fn raw_view_at(&self, index: u32, buffer: Option<&ID3D12Resource>, first: u64, bytes: u64, uav: bool) {
        let Some(host) = host() else { return };
        let writable = |b: &ID3D12Resource| {
            // SAFETY: a live buffer.
            unsafe { b.GetDesc() }.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
        };
        let buffer = buffer.filter(|&b| !uav || writable(b));
        // shortcut: a buffer view holds at most 2^27 elements (512 MiB raw).
        let (first, elements) = match buffer {
            Some(_) => (first / 4, (bytes / 4).clamp(1, 1 << 27) as u32),
            // A null view still names its size.
            None => (0, 1),
        };
        // SAFETY: a slot of our heap; the buffer is live (or none: a null view).
        unsafe {
            if uav {
                let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            FirstElement: first,
                            NumElements: elements,
                            Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                };
                host.device
                    .CreateUnorderedAccessView(buffer, None, Some(&desc), self.cpu(index));
            } else {
                let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV {
                            FirstElement: first,
                            NumElements: elements,
                            StructureByteStride: 0,
                            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                        },
                    },
                };
                host.device
                    .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index));
            }
        }
        self.keep(index, buffer.map(|b| (b.clone(), None)));
        self.write_stride(index, 4);
        self.set_mark(index, None);
        // Not a title view: no entry an earlier table left there stays.
        self.note_address(index, None);
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.keep(index, None);
        #[cfg(feature = "diagnostics")]
        self.failed_textures.lock().unwrap_or_else(|e| e.into_inner()).remove(&index);
        self.set_stride(index, None);
        self.set_mark(index, None);
        self.note_address(index, None);
        if index < self.count {
            self.viewed.set(index, None, false);
        }
    }

    /// Enters the byte stride of `view` (0 for none or not a buffer view)
    /// for slot `index` in the view-stride table, with the integer-format
    /// bits of a typed buffer view ([`view_kind`]).
    fn set_stride(&self, index: u32, view: Option<&View>) {
        self.write_stride(index, view.map_or(0, |v| view_stride(v) | view_kind(v)));
    }

    fn write_stride(&self, index: u32, stride: u32) {
        let Some((_, data)) = &self.strides else { return };
        if index >= self.count.max(victory_shader::emit::HEAP_VIEWS) {
            return;
        }
        // SAFETY: a dword of the mapped table, which has one per slot below
        // the larger of the title's count and `HEAP_VIEWS`.
        unsafe { (*data as *mut u32).add(index as usize).write_volatile(stride) };
    }

    /// The view-stride table, for root SRV `VIEW_STRIDES`.
    pub(crate) fn strides(&self) -> Option<&ID3D12Resource> {
        self.strides.as_ref().map(|(buffer, _)| buffer)
    }

    /// Enters slot `index`'s view-mark entry: the mark bit and phase of the
    /// unordered buffer view there ([`crate::shadow::Shadow::mark_entry`]),
    /// none for anything else.
    pub(crate) fn set_mark(&self, index: u32, entry: Option<(u32, u32)>) {
        let Some((_, data)) = &self.marks else { return };
        if index >= self.count.max(victory_shader::emit::HEAP_VIEWS) {
            return;
        }
        let entry = entry.unwrap_or((victory_shader::emit::VIEW_MARK_NONE, 0));
        // SAFETY: two dwords of the mapped table, which has two per slot
        // below the larger of the title's count and `HEAP_VIEWS`.
        unsafe {
            let at = (*data as *mut u32).add(2 * index as usize);
            at.write_volatile(entry.0);
            at.add(1).write_volatile(entry.1);
        }
    }

    /// The view-mark table, for the root SRV of shaders marking their writes.
    pub(crate) fn marks(&self) -> Option<&ID3D12Resource> {
        self.marks.as_ref().map(|(buffer, _)| buffer)
    }

    /// Slot `index` now views `resource` (or nothing of the host's).
    fn keep(&self, index: u32, resource: Kept) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = kept.get_mut(index as usize) {
            let old = std::mem::replace(slot, resource);
            #[cfg(feature = "diagnostics")]
            if let Some((old, _)) = &old
                && !self.sampler
                && slot.as_ref().map(|s| windows::core::Interface::as_raw(&s.0)) != Some(windows::core::Interface::as_raw(old))
                // SAFETY: a live resource.
                && unsafe { old.GetDesc() }.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER
            {
                use windows::core::Interface;
                let left = crate::resource::life::holders(old);
                crate::resource::life::note(old.as_raw() as usize, crate::resource::life::SLOT, u64::from(index), left);
            }
            // Title threads rewrite slots while the GPU runs a submission;
            // one of its shaders may hold the old descriptor (or read the
            // slot at an index from wrong data): what it views is freed
            // only once that work has run.
            if let Some((old, _)) = old {
                crate::host::retire(old);
            }
        }
    }

    /// The host address of the slots' view addresses (see the field), made
    /// on first use from the slots made so far.
    pub(crate) fn view_addresses(&self) -> Option<u64> {
        let made = self.addresses.get_or_init(|| {
            // The view is a root view, which has no bounds: an entry for
            // every slot a table can lie in, the dynamic ones too (a table
            // made afresh on a host whose heap is smaller than the title's
            // read past the title slots' entries: whatever followed the
            // buffer, or a GPU fault).
            let (buffer, data) = host()?.upload_buffer(u64::from(self.address_slots()) * 16)?;
            crate::host::name(&buffer, || "view addresses".into());
            let data = data as usize;
            let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            for (i, &key) in made.iter().enumerate() {
                let view = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten();
                write_address(data, i, view.as_ref());
            }
            // Dynamic slots are entered as they are made ([`Self::make_at`],
            // [`Self::make_null`], raw views).
            for i in self.count..self.address_slots() {
                write_address(data, i as usize, None);
            }
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data, gpu))
        });
        made.as_ref().map(|m| m.2)
    }

    /// The slots the view addresses have an entry for: the title's and the
    /// dynamic ones.
    fn address_slots(&self) -> u32 {
        self.count + self.dynamic
    }

    /// Slot `index` now holds `view` (or no view): its view address.
    fn note_address(&self, index: u32, view: Option<&View>) {
        if index < self.address_slots()
            && let Some(Some((_, data, _))) = self.addresses.get()
        {
            write_address(*data, index as usize, view);
        }
    }

    /// The view address entered for slot `index`; none for a slot past the
    /// view addresses' buffer (tests).
    #[cfg(test)]
    fn address_at(&self, index: u32) -> Option<u64> {
        let (buffer, data, _) = self.addresses.get()?.as_ref()?;
        // SAFETY: a live buffer.
        if 16 * u64::from(index) + 16 > unsafe { buffer.GetDesc() }.Width {
            return None;
        }
        // SAFETY: an entry of the mapped view addresses (checked).
        let [lo, hi] = unsafe { ((data + 16 * index as usize) as *const [u32; 2]).read_unaligned() };
        Some(u64::from(hi) << 32 | u64::from(lo))
    }

    /// Makes slots `index` and `index + 1` (dynamic ones) a raw shader
    /// resource view and a raw unordered access view of the first `bytes`
    /// (a multiple of 4) of `buffer`, which they hold; null views for no
    /// buffer.
    pub(crate) fn raw_pair_at(&self, index: u32, buffer: Option<&ID3D12Resource>, bytes: u64) {
        let Some(host) = host() else { return };
        // shortcut: a buffer view holds at most 2^27 elements (512 MiB raw);
        // a longer copy's rest reads 0 through it.
        let elements = (bytes / 4).min(1 << 27) as u32;
        let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 0,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: elements,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        // SAFETY: slots of our heap; the buffer is live (or none: null views).
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&srv), self.cpu(index));
            host.device
                .CreateUnorderedAccessView(buffer, None, Some(&uav), self.cpu(index + 1));
        }
        self.keep(index, buffer.map(|b| (b.clone(), None)));
        self.keep(index + 1, buffer.map(|b| (b.clone(), None)));
        self.write_stride(index, 4);
        self.write_stride(index + 1, 4);
        // Writes through these are title memory's, marked in its page table.
        self.set_mark(index, None);
        self.set_mark(index + 1, None);
        // Not a title view: no entry an earlier table left there stays.
        self.note_address(index, None);
        self.note_address(index + 1, None);
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart() };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + u64::from(index) * u64::from(self.step),
        }
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// A heap sync of slots `first..last`, whose records are at `start` +
    /// index × `step`: [`HostHeap::mirror`] of each slot whose record
    /// differs from what the slot holds, the rest passed by as `mirror`
    /// would, with one lock of each kind per stack chunk instead of two per
    /// slot (a world frame looks at ~700,000 slots, run perf3: ~75 ms).
    pub(crate) fn sync_slots(&self, first: u32, last: u32, start: u64, step: u64) {
        // Dirty title pages contain up to 2,048 view slots. Checking 1,024
        // at once cuts lock acquisitions eightfold against 128-slot chunks,
        // while keeping the temporary arrays under 20 KiB of stack.
        const CHUNK: usize = 1024;
        let at = |i: u32| start + u64::from(i) * step;
        let mut records = [[0u32; 3]; CHUNK];
        let mut current = [false; CHUNK];
        let mut stale = [0u32; CHUNK];
        let mut next = first;
        while next < last {
            let count = (last - next).min(CHUNK as u32) as usize;
            for (offset, record) in records[..count].iter_mut().enumerate() {
                // SAFETY: records in title heap memory the caller found mapped.
                *record = unsafe { (at(next + offset as u32) as *const [u32; 3]).read_unaligned() };
            }
            // Preserve the original lock order: finish checking view
            // generations before taking `made`, then release it before
            // mirroring any stale slot.
            {
                let views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
                for (&[tag, index, generation], valid) in records[..count].iter().zip(&mut current[..count]) {
                    *valid = tag == VIEW_TAG
                        && views.list.get(index as usize).is_some_and(|(_, live)| *live == generation);
                }
            }
            let stale_count = {
                let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
                let mut found = 0;
                for (offset, (&[_, view, generation], &valid)) in
                    records[..count].iter().zip(&current[..count]).enumerate()
                {
                    let i = next + offset as u32;
                    let dirty = match made.get(i as usize) {
                        Some(&m) if valid => m != u64::from(generation) << 32 | u64::from(view),
                        Some(&m) => m != 0,
                        None => true,
                    };
                    if dirty {
                        stale[found] = i;
                        found += 1;
                    }
                }
                found
            };
            for &i in &stale[..stale_count] {
                self.mirror(i, at(i), false);
            }
            next += count as u32;
        }
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        self.mirror_with(index, handle, for_draw, false)
    }

    /// A shader-statically-used table slot may recover from an ordinary
    /// budget denial through the bounded priority reserve. The second
    /// mirror is forced exactly once after its coherent host copy exists;
    /// the unchanged title descriptor would otherwise remain null for this
    /// admission epoch.
    pub(crate) fn mirror_used_table(&self, index: u32, handle: u64) -> Option<View> {
        let view = self.mirror(index, handle, true)?;
        #[cfg(feature = "diagnostics")]
        if let Some(View::Shader(resource, _)) = placed_texture(&view).or_else(|| Some(view.clone())) {
            crate::list::note_skipped_copy_shader_read(resource as crate::object::This, "table");
        }
        if self.kept.lock().unwrap_or_else(|e| e.into_inner())
            .get(index as usize).is_some_and(Option::is_some)
            || !self.failed_buffers.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&index)
        {
            return Some(view);
        }
        let Some(_copy) = table_priority_copy(&view) else { return Some(view) };
        let remade = self.mirror_with(index, handle, true, true);
        #[cfg(feature = "diagnostics")]
        {
            if self.kept.lock().unwrap_or_else(|e| e.into_inner())
                .get(index as usize).is_some_and(Option::is_some)
            {
                TABLE_PRIORITY_REMADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                TABLE_PRIORITY_REMAKE_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        remade
    }

    fn mirror_with(&self, index: u32, handle: u64, for_draw: bool, force_remake: bool) -> Option<View> {
        if index >= self.count {
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        let Some(view) = view else {
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            self.failed_buffers.lock().unwrap_or_else(|e| e.into_inner()).remove(&index);
            #[cfg(feature = "diagnostics")]
            self.failed_textures.lock().unwrap_or_else(|e| e.into_inner()).remove(&index);
            if made[index as usize] != 0 {
                made[index as usize] = 0;
                self.make_null(index);
                self.note_address(index, None);
            }
            return None;
        };
        if made[index as usize] != key {
            self.failed_buffers.lock().unwrap_or_else(|e| e.into_inner()).remove(&index);
        }
        // A placed texture's view left null below is made when a draw reads
        // the slot through a table; until then the slot is not made again
        // (heap syncs revisit it at every submission). Also re-mirror if a
        // later unordered or renderable view upgraded the memory's HostTexture.
        let needs_remake = || {
            if force_remake {
                return true;
            }
            if self.failed_buffers.lock().unwrap_or_else(|e| e.into_inner())
                .get(&index)
                .is_some_and(|&failed| failed < crate::shadow::admission_epoch())
            {
                return true;
            }
            if !for_draw || !is_texture(&view) {
                return false;
            }
            let held = self.kept.lock().unwrap_or_else(|e| e.into_inner())[index as usize]
                .as_ref()
                .map(|(res, _)| windows::core::Interface::as_raw(res));
            let Some(held) = held else {
                return true;
            };
            is_placed_texture(&view)
                && placed_texture(&view).is_some_and(|p| {
                    let (View::Shader(r, _) | View::Unordered(r, ..)) = p else {
                        return false;
                    };
                    !crate::resource::host_copy(r as crate::object::This).is_some_and(|copy| {
                        windows::core::Interface::as_raw(&copy.texture) == held
                            || copy.twin.get().and_then(Option::as_ref).is_some_and(|t| {
                                t.planes
                                    .iter()
                                    .any(|p| windows::core::Interface::as_raw(&p.texture) == held)
                            })
                    })
                })
        };
        if made[index as usize] == key && !needs_remake() {
            return Some(view);
        }
        // Shaders index the heap itself (`crate::bindless`): buffer views
        // are made when written.
        // shortcut: views of textures the title placed itself are made
        // when a draw reads them through a table: a host texture (and a
        // staging copy) for each of the ~20,000 the title has views of
        // ran the host out of memory (run b2).
        let later = !for_draw && is_placed_texture(&view);
        if !later {
            // What the view needs (the texture it views, the copy of the
            // memory) is made without the slot lock, which title threads
            // binding and writing views wait on (run evict5).
            drop(made);
            prepare(&view);
            made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            // The title may have written another record there meanwhile (and
            // another thread made it): that one is the slot's.
            // SAFETY: as above.
            if unsafe { (handle as *const [u32; 3]).read_unaligned() } != [tag, i, generation] {
                drop(made);
                return self.mirror_with(index, handle, for_draw, force_remake);
            }
            if made[index as usize] == key && !needs_remake() {
                return Some(view);
            }
        }
        {
            self.set_stride(index, Some(&view));
            self.note_address(index, Some(&view));
            if later {
                // Made once a shader reads it (`crate::shadow::take_reads`).
                crate::shadow::forget_read(index);
            }
            made[index as usize] = key;
            let (kept, mark) = if later {
                (None, None)
            } else {
                make_marked(&view, self.cpu(index))
            };
            let range = match &view {
                View::Constant(address, size) => Some((*address, u64::from(*size))),
                _ => buffer_range(&view),
            };
            let retry = kept.is_none() && range.is_some_and(|(address, len)| {
                crate::shadow::retryable_ordinary_view(address, len)
            });
            {
                let mut failed = self.failed_buffers.lock().unwrap_or_else(|e| e.into_inner());
                if retry {
                    failed.insert(index, crate::shadow::admission_epoch());
                } else {
                    failed.remove(&index);
                }
            }
            self.set_mark(index, mark);
            if kept.is_none() && !self.sampler {
                null_for(&view, self.cpu(index));
            }
            if !self.sampler {
                self.viewed.set(index, Some(&view), kept.is_some());
            }
            #[cfg(feature = "diagnostics")]
            {
                let mut failed = self.failed_textures.lock().unwrap_or_else(|e| e.into_inner());
                if kept.is_none() && !later && texture_view_details(&view).is_some() {
                    failed.insert(index, VIEW_MISSING.with(|reason| reason.get()));
                } else {
                    failed.remove(&index);
                }
            }
            self.keep(index, kept.flatten());
        }
        Some(view)
    }

    /// Retry a bounded number of unchanged, formerly budget-denied buffer
    /// slots at submission. Heap-indexing shaders may use such a slot
    /// without a table draw naming it, so waiting for a title rewrite would
    /// leave a permanent null descriptor after memory pressure subsides.
    pub(crate) fn retry_failed_buffers(&self, start: u64, step: u64) {
        self.retry_failed_buffers_with(start, step, crate::shadow::ordinary_room_for_retry);
    }

    fn retry_failed_buffers_with(&self, start: u64, step: u64, room: impl FnOnce() -> bool) {
        const MAX_RETRIES_PER_SYNC: usize = 256;
        let epoch = crate::shadow::admission_epoch();
        let retry: Vec<u32> = self.failed_buffers.lock().unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(&index, &failed)| (failed < epoch).then_some(index))
            .take(MAX_RETRIES_PER_SYNC)
            .collect();
        if retry.is_empty() || !room() {
            return;
        }
        for index in retry {
            if let Some(handle) = u64::from(index).checked_mul(step).and_then(|at| start.checked_add(at)) {
                self.mirror(index, handle, false);
            }
            if crate::shadow::ordinary_admission_denied() {
                break;
            }
        }
    }

    /// An observed heap read tried this slot, but its ordinary sampled
    /// texture was deferred. A GPU read mark is delivered only once until
    /// the slot is explicitly forgotten; keeping the miss here avoids a
    /// permanent null view after a later texture eviction frees space.
    pub(crate) fn note_observed_texture_result(&self, index: u32, view: Option<&View>) {
        if index >= self.count {
            return;
        }
        let key = self.made.lock().unwrap_or_else(|e| e.into_inner())[index as usize];
        let held = self.kept.lock().unwrap_or_else(|e| e.into_inner())[index as usize].is_some();
        let failed = key != 0 && !held && view.is_some_and(missing_streamed_copy);
        if !failed {
            self.observed_texture_misses.lock().unwrap_or_else(|e| e.into_inner()).remove(&index);
            return;
        }
        let state = TextureMiss {
            key,
            reuse_epoch: crate::placement::texture_reuse_epoch_snapshot(),
            room_open: crate::resource::stream_room(),
        };
        self.observed_texture_misses.lock().unwrap_or_else(|e| e.into_inner()).insert(index, state);
    }

    pub(crate) fn has_observed_texture_misses(&self) -> bool {
        !self.observed_texture_misses.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Choose at most `limit` previously observed placed views whose host
    /// copy can be tried again. This runs while the title heap is registered;
    /// the returned placed views own their descriptions and can be prepared
    /// after the heap lock is released.
    pub(crate) fn observed_texture_retry_candidates(
        &self, start: u64, step: u64, epoch: u64, room: bool, limit: usize,
    ) -> Vec<(u32, u64, View)> {
        let retry: Vec<(u32, u64)> = {
            let mut misses = self.observed_texture_misses.lock().unwrap_or_else(|e| e.into_inner());
            misses.iter_mut()
                .filter_map(|(&index, miss)| {
                    if !miss.ready(epoch, room) { return None; }
                    miss.reuse_epoch = epoch;
                    miss.room_open = room;
                    Some((index, miss.key))
                })
                .take(limit)
                .collect()
        };
        let mut candidates = Vec::with_capacity(retry.len());
        for (index, key) in retry {
            let handle = u64::from(index).checked_mul(step).and_then(|at| start.checked_add(at));
            let current = handle.is_some_and(|at| title_record_matches(at, key))
                && self.made.lock().unwrap_or_else(|e| e.into_inner())[index as usize] == key;
            let view = current.then(|| view_at(key as u32, (key >> 32) as u32)).flatten();
            if let Some(view @ View::Placed(false, ..)) = view {
                candidates.push((index, key, view));
            } else {
                self.remove_observed_texture_miss(index, key);
                #[cfg(feature = "diagnostics")]
                OBSERVED_TEXTURE_RETRY[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        candidates
    }

    /// The host copy was prepared outside `SHADER_HEAPS`. Check the record
    /// again before making the descriptor; `mirror` also rechecks after its
    /// own preparation if the title rewrites the slot during this call.
    pub(crate) fn finish_observed_texture_retry(&self, index: u32, key: u64, handle: u64) {
        if !title_record_matches(handle, key)
            || self.made.lock().unwrap_or_else(|e| e.into_inner())[index as usize] != key
        {
            self.remove_observed_texture_miss(index, key);
            #[cfg(feature = "diagnostics")]
            OBSERVED_TEXTURE_RETRY[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        #[cfg(feature = "diagnostics")]
        OBSERVED_TEXTURE_RETRY[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let view = self.mirror(index, handle, true);
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner())[index as usize];
        let held = self.kept.lock().unwrap_or_else(|e| e.into_inner())[index as usize].is_some();
        if made != key || held || !view.is_some_and(|v| missing_streamed_copy(&v)) {
            self.remove_observed_texture_miss(index, key);
            #[cfg(feature = "diagnostics")]
            if made == key && held {
                OBSERVED_TEXTURE_RETRY[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    fn remove_observed_texture_miss(&self, index: u32, key: u64) {
        let mut misses = self.observed_texture_misses.lock().unwrap_or_else(|e| e.into_inner());
        if misses.get(&index).is_some_and(|m| m.key == key) {
            misses.remove(&index);
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn observed_texture_miss_count(&self) -> usize {
        self.observed_texture_misses.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self.made.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize)?;
        // A view the host could not make leaves a null view, its record
        // noted all the same; it keeps no resource.
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize).is_some_and(Option::is_some);
        if key == 0 || (!kept && !self.sampler) {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }

    /// Slots viewing one of the host textures `textures` (raw pointers),
    /// which are given up: each gets a null view of its kind and holds the
    /// texture no more (retired until the running submission has run), its
    /// record stays noted so heap syncs leave it, and the next draw reading
    /// it through a table or shader reading it through the heap (its read
    /// mark, cleared) makes it again.
    /// The slots are found by a walk of the heap, once per pass that gives
    /// textures up: a map of each texture's slots, kept at every slot the
    /// title rewrote, cost ~25 us a slot (a texture viewed in thousands of
    /// slots had its list searched each time; 62,000 slots a frame, run
    /// evict3).
    pub(crate) fn evict(&self, textures: &std::collections::HashSet<usize>) {
        let slots: Vec<u32> = {
            let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            (0..kept.len() as u32)
                .filter(|&i| {
                    kept[i as usize]
                        .as_ref()
                        .is_some_and(|k| k.1.is_none() && textures.contains(&(windows::core::Interface::as_raw(&k.0) as usize)))
                })
                .collect()
        };
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        for index in slots {
            let key = made.get(index as usize).copied().unwrap_or(0);
            let view = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten();
            match &view {
                Some(v) => null_for(v, self.cpu(index)),
                None => null_view(self.cpu(index)),
            }
            self.keep(index, None);
            if index < self.count {
                self.viewed.set(index, view.as_ref(), false);
                crate::shadow::forget_read(index);
            }
        }
    }

    /// Slots viewing one of the host buffers `buffers` (raw pointers), which
    /// are given up ([`HostHeap::evict`] for textures). The record stays, so
    /// a later write of the slot makes the view again.
    pub(crate) fn evict_buffers(&self, buffers: &std::collections::HashSet<usize>) {
        if self.sampler || buffers.is_empty() {
            return;
        }
        let slots: Vec<u32> = {
            let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            (0..kept.len() as u32)
                .filter(|&i| {
                    kept[i as usize].as_ref().is_some_and(|k| {
                        buffers.contains(&(windows::core::Interface::as_raw(&k.0) as usize))
                    })
                })
                .collect()
        };
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        for index in slots {
            let key = made.get(index as usize).copied().unwrap_or(0);
            let view = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten();
            match &view {
                Some(v) => null_for(v, self.cpu(index)),
                None => null_view(self.cpu(index)),
            }
            self.keep(index, None);
            if index < self.count {
                self.viewed.set(index, view.as_ref(), false);
                crate::shadow::forget_read(index);
            }
        }
    }

    /// The host textures (raw pointers) slots `slots` view.
    pub(crate) fn textures_at(&self, slots: &[u32]) -> Vec<usize> {
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .iter()
            .filter_map(|&i| kept.get(i as usize)?.as_ref().filter(|k| k.1.is_none()))
            .map(|k| windows::core::Interface::as_raw(&k.0) as usize)
            .collect()
    }

    /// The view of slot `index` if it is a placed texture's left without a
    /// host view (made when read).
    pub(crate) fn unmade_placed(&self, index: u32) -> Option<View> {
        let key = *self.made.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize)?;
        let view = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten()?;
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize).is_some_and(Option::is_some);
        (is_placed_texture(&view) && !kept).then_some(view)
    }

    /// A shader read slot `index` through the heap: the texture it views
    /// was used now.
    pub(crate) fn stamp_read(&self, index: u32) {
        let key = self.made.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize).copied().unwrap_or(0);
        let Some(view) = (key != 0).then(|| view_at(key as u32, (key >> 32) as u32)).flatten() else {
            return;
        };
        let view = placed_texture(&view).unwrap_or(view);
        if let View::Shader(r, _) = view
            && let Some(t) = crate::resource::made_copy(r as crate::object::This)
        {
            t.stamp();
            #[cfg(feature = "diagnostics")]
            crate::list::note_skipped_copy_shader_read(r as crate::object::This, "observed_heap");
        }
    }

    /// A bounded snapshot for the five-second diagnostic report. Only title
    /// slots count: dynamic slots are owned by command lists, not stale title
    /// descriptors. Clone each distinct COM resource once so it stays live
    /// after the heap locks are released.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn kept_snapshot(&self) -> (Vec<(u64, usize)>, Vec<ID3D12Resource>) {
        use windows::core::Interface;

        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let mut slots = Vec::new();
        let mut resources = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (&key, entry) in made.iter().zip(kept.iter()).take(self.count as usize) {
            let Some((resource, _)) = entry else { continue };
            let ptr = resource.as_raw() as usize;
            slots.push((key, ptr));
            if seen.insert(ptr) {
                resources.push(resource.clone());
            }
        }
        (slots, resources)
    }

    /// Diagnostic only: distinguish title slots from dynamic slots that a
    /// command list still owns and dynamic slots already released for reuse.
    /// A released slot that still pins a Shadow can prevent pressure trim
    /// until another view overwrites that slot.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn shadow_pin_snapshot(&self) -> ([u64; 3], Vec<(usize, u64, u8)>) {
        let taken = self.next.lock().unwrap_or_else(|e| e.into_inner()).1.clone();
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let mut slots = [0u64; 3];
        let mut unique = std::collections::HashMap::<usize, (u64, u8)>::new();
        for (index, entry) in kept.iter().enumerate().take((self.count + self.dynamic) as usize) {
            let Some((_, Some(shadow))) = entry else { continue };
            let group = if index < self.count as usize {
                0
            } else if taken.get(index - self.count as usize).copied().unwrap_or(false) {
                1
            } else {
                2
            };
            slots[group] += 1;
            let ptr = std::sync::Arc::as_ptr(shadow) as usize;
            let pin = unique.entry(ptr).or_insert((shadow.allocation_len(), 0));
            pin.1 |= 1 << group;
        }
        (slots, unique.into_iter().map(|(ptr, (bytes, groups))| (ptr, bytes, groups)).collect())
    }

    /// Whether slot `index` was made from a record (tests).
    #[cfg(test)]
    pub(crate) fn made(&self, index: u32) -> bool {
        self.made.lock().unwrap_or_else(|e| e.into_inner())[index as usize] != 0
    }

    /// Whether slot `index` holds a host view of a resource (tests).
    #[cfg(test)]
    pub(crate) fn kept(&self, index: u32) -> bool {
        self.kept.lock().unwrap_or_else(|e| e.into_inner())[index as usize].is_some()
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn failed_buffer_count(&self) -> usize {
        self.failed_buffers.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Called only for a descriptor the translated shader names through a
    /// table, or a heap slot observed as read. Null texture views are
    /// diagnosed elsewhere; this probe classifies missing buffer views.
    #[cfg(feature = "diagnostics")]
    fn used_null_reason(
        &self,
        index: u32,
        view: Option<&View>,
        buffer_expected_if_empty: bool,
    ) -> Option<(UsedNullReason, Option<(u64, u64)>)> {
        if index >= self.count {
            return None;
        }
        if self.kept.lock().unwrap_or_else(|e| e.into_inner())
            .get(index as usize).is_some_and(Option::is_some)
        {
            return None;
        }
        let range = match view {
            Some(View::Constant(address, size)) => Some((*address, u64::from(*size))),
            Some(view) => buffer_range(view),
            None if buffer_expected_if_empty => return Some((UsedNullReason::InvalidOrEmpty, None)),
            None => return None,
        };
        let buffer_view = range.is_some() || matches!(view, Some(View::Placed(_, resource, _)) if resource[0] == 1);
        if !buffer_view {
            return None;
        }
        if self.failed_buffers.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&index) {
            return Some((UsedNullReason::Budget, range));
        }
        let invalid = range.is_none_or(|(address, len)| {
            address == 0 || len == 0 || address.checked_add(len).is_none()
                || crate::shadow::title_address_is_free(address)
        });
        Some((if invalid { UsedNullReason::InvalidOrEmpty } else { UsedNullReason::Other }, range))
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_used_null(
        &self,
        index: u32,
        view: Option<&View>,
        buffer_expected_if_empty: bool,
        source: UsedNullSource,
        pipeline: usize,
        root_slot: u8,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some((reason, range)) = self.used_null_reason(index, view, buffer_expected_if_empty) else {
            return;
        };
        let source_at = match source { UsedNullSource::Table => 0, UsedNullSource::ObservedHeap => 1 };
        let reason_at = match reason {
            UsedNullReason::Budget => 0,
            UsedNullReason::InvalidOrEmpty => 1,
            UsedNullReason::Other => 2,
        };
        USED_NULL[source_at * 3 + reason_at].fetch_add(1, Relaxed);
        if reason == UsedNullReason::Budget
            && let Some((address, len)) = range
        {
            USED_BUDGET_RANGES.get_or_init(|| Mutex::new(UsedBudgetRanges::default()))
                .lock().unwrap_or_else(|e| e.into_inner())
                .record(UsedBudgetKey { source, address, len }, index, pipeline,
                    root_slot, USED_BUDGET_RANGE_CAP);
        }
        let seen = USED_NULL_WARNED.fetch_add(1, Relaxed) + 1;
        if seen <= 8 || seen.is_power_of_two() {
            tracing::warn!(seen, ?source, ?reason, index, root_slot,
                pipeline = format_args!("{pipeline:#x}"), ?range,
                "shader-used buffer descriptor has a null host view");
        }
    }

    /// A table entry or observed heap slot that the shader actually named.
    /// `mirror` has already tried to create the view, so this probe only
    /// reads the slot and its saved failure reason; it never creates another
    /// texture or queries the driver under memory pressure.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_used_texture_null(
        &self,
        index: u32,
        view: Option<&View>,
        source: UsedNullSource,
        pipeline: usize,
        root_slot: u8,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        if index >= self.count {
            return;
        }
        let Some((kind, address, width, height, format)) = view.and_then(texture_view_details) else {
            return;
        };
        if self.kept.lock().unwrap_or_else(|e| e.into_inner())
            .get(index as usize).is_some_and(Option::is_some)
        {
            return;
        }
        let attempted_reason = self.failed_textures.lock().unwrap_or_else(|e| e.into_inner())
            .get(&index).copied().unwrap_or("not materialized");
        let (reason_at, reason) = used_texture_null_reason(address, attempted_reason);
        let source_at = match source { UsedNullSource::Table => 0, UsedNullSource::ObservedHeap => 1 };
        USED_NULL_TEXTURE[source_at * 5 + reason_at].fetch_add(1, Relaxed);
        let seen = USED_NULL_TEXTURE_WARNED.fetch_add(1, Relaxed) + 1;
        if seen <= 8 || seen.is_power_of_two() {
            tracing::warn!(seen, ?source, reason, kind, index, root_slot,
                pipeline = format_args!("{pipeline:#x}"), address = format_args!("{address:#x}"),
                width, height, format,
                "shader-used texture descriptor has a null host view");
        }
    }
}

/// A zero title address cannot name a placed texture: create_at_address
/// rejects it before host materialization. Keep these null descriptors out
/// of the real texture-miss categories even if their recorded host attempt
/// failed as "placed texture unavailable".
#[cfg(feature = "diagnostics")]
fn used_texture_null_reason(address: u64, attempted_reason: &'static str) -> (usize, &'static str) {
    if address == 0 {
        return (4, "invalid or empty");
    }
    match attempted_reason {
        "host copy unavailable" => (0, attempted_reason),
        "placed texture unavailable" => (1, attempted_reason),
        "format incompatible" => (2, attempted_reason),
        _ => (3, attempted_reason),
    }
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A shader view of a texture whose largest mips were not created
/// ([`crate::resource::shrink_streamed`]). `dropped` is how far into the
/// title's chain our mip 0 sits. A view that asks past the mips we kept
/// is pulled back, so the create does not remove the device.
fn clamp_streamed_mips(desc: &mut D3D12_SHADER_RESOURCE_VIEW_DESC, dropped: u32, mips: u16) {
    let mips = u32::from(mips);
    if mips == 0 {
        return;
    }
    let pull = |most: &mut u32, levels: &mut u32, lod: &mut f32| {
        *most = most.saturating_sub(dropped);
        if *most >= mips {
            *most = mips - 1;
        }
        let room = mips - *most;
        // u32::MAX is "every mip from here down".
        if *levels == u32::MAX || *levels > room {
            *levels = room;
        }
        *lod = (*lod - dropped as f32).max(0.0);
    };
    // SAFETY: the union member matches the view dimension the title set.
    unsafe {
        match desc.ViewDimension {
            D3D12_SRV_DIMENSION_TEXTURE1D => {
                let t = &mut desc.Anonymous.Texture1D;
                pull(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
            }
            D3D12_SRV_DIMENSION_TEXTURE1DARRAY => {
                let t = &mut desc.Anonymous.Texture1DArray;
                pull(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
            }
            D3D12_SRV_DIMENSION_TEXTURE2D => {
                let t = &mut desc.Anonymous.Texture2D;
                pull(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
            }
            D3D12_SRV_DIMENSION_TEXTURE2DARRAY => {
                let t = &mut desc.Anonymous.Texture2DArray;
                pull(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
            }
            D3D12_SRV_DIMENSION_TEXTURE3D => {
                let t = &mut desc.Anonymous.Texture3D;
                pull(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
            }
            D3D12_SRV_DIMENSION_TEXTURECUBE => {
                let t = &mut desc.Anonymous.TextureCube;
                pull(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
            }
            D3D12_SRV_DIMENSION_TEXTURECUBEARRAY => {
                let t = &mut desc.Anonymous.TextureCubeArray;
                pull(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
            }
            _ => {}
        }
    }
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// The byte stride of a buffer view (its element size, at least 1), 0 for
/// other views: what the view-stride table holds for it.
fn view_stride(view: &View) -> u32 {
    let (unordered, words): (bool, &[u32; 10]) = match view {
        // Read as raw buffers (`make`).
        View::Constant(..) => return 4,
        View::Shader(_, Some(desc)) => (false, desc),
        View::Unordered(_, _, Some(desc)) => (true, desc),
        View::Placed(unordered, resource, desc) if resource[0] == 1 => {
            match desc[..10].try_into() {
                Ok(words) => (*unordered, words),
                Err(_) => return 0,
            }
        }
        _ => return 0,
    };
    // The view dimension: buffer (1) for shader and unordered views alike.
    if words[1] != 1 {
        return 0;
    }
    let element = if unordered {
        let desc = uav_desc(words);
        // SAFETY: a buffer view's union member.
        let buffer = unsafe { desc.Anonymous.Buffer };
        if buffer.StructureByteStride > 0 {
            u64::from(buffer.StructureByteStride)
        } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
            4
        } else {
            crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
        }
    } else {
        element_size(&srv_desc(words))
    };
    // Nonzero: shaders taking views at computed indices read the null buffer
    // view where the table holds 0 (`victory_shader::emit`).
    element.max(1) as u32
}

/// The view-stride table's format-kind bits of a buffer view: a typed view
/// (no structure stride, not raw) of an integer format gets
/// `VIEW_INTEGER`, a signed one also `VIEW_SIGNED`; anything else none.
/// Translated shaders pass 16-bit typed data of such views as bits, not as
/// halves (`victory_shader::emit`, "d16").
fn view_kind(view: &View) -> u32 {
    use victory_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
    let (unordered, words): (bool, &[u32; 10]) = match view {
        View::Shader(_, Some(desc)) => (false, desc),
        View::Unordered(_, _, Some(desc)) => (true, desc),
        View::Placed(unordered, resource, desc) if resource[0] == 1 => match desc[..10].try_into() {
            Ok(words) => (*unordered, words),
            Err(_) => return 0,
        },
        _ => return 0,
    };
    // Buffer views only (dimension 1); a structured or raw one is no typed view.
    let (format, typed) = if unordered {
        let desc = uav_desc(words);
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        (desc.Format.0 as u32, b.StructureByteStride == 0 && b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 == 0)
    } else {
        let desc = srv_desc(words);
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        (desc.Format.0 as u32, b.StructureByteStride == 0 && b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 == 0)
    };
    match (words[1] == 1 && typed).then(|| integer_format(format)).flatten() {
        Some(true) => VIEW_INTEGER | VIEW_SIGNED,
        Some(false) => VIEW_INTEGER,
        None => 0,
    }
}

/// Whether a DXGI format is an integer one, and if so whether signed:
/// `Some(false)` for `UINT`, `Some(true)` for `SINT`, `None` otherwise.
fn integer_format(format: u32) -> Option<bool> {
    match format {
        3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62 => Some(false),
        4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64 => Some(true),
        _ => None,
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// Writes slot `index`'s words into the view addresses at `data` (the
/// caller keeps `index` below [`HostHeap::address_slots`]), 16 bytes a
/// slot: a buffer view's title address (dword 0 low, dword 1 bits 0-15
/// high), a texture view's size fields ([`texture_words`]), zeros for
/// anything else.
fn write_address(data: usize, index: usize, view: Option<&View>) {
    let address = view.and_then(buffer_range).map_or(0, |(a, _)| a);
    let words = match view.and_then(texture_words) {
        Some(words) => words,
        None => [address as u32, (address >> 32) as u32 & 0xFFFF, 0, 0],
    };
    // SAFETY: the mapped view addresses hold 16 bytes per title and dynamic
    // slot (below `address_slots`).
    unsafe { ((data + 16 * index) as *mut [u32; 4]).write_unaligned(words) };
}

/// A texture view's console image descriptor dwords 0-3 as far as its size
/// goes: width - 1 and height - 1 of the resource's level 0 (dword 1 bits
/// 30-31, dword 2 bits 0-13 and 14-29), dword 3 an identity swizzle, the
/// view's first and last level (bits 12-15, 16-19) and the image type (bits
/// 28-31). `None` for buffers and samplers.
// shortcut: the address, format and tiling fields read 0 (no translated
// shader reads them: the shader refuses such reads where it can tell),
// and the swizzle is the identity whatever the view's component mapping.
fn texture_words(view: &View) -> Option<[u32; 4]> {
    let (resource, desc, unordered) = match view {
        View::Shader(r, desc) => (crate::resource::texture_desc(*r as crate::object::This)?, *desc, false),
        View::Unordered(r, _, desc) => (crate::resource::texture_desc(*r as crate::object::This)?, *desc, true),
        View::Placed(unordered, resource, desc) if resource[0] != 1 => {
            (*resource, desc[..10].try_into().ok(), *unordered)
        }
        _ => return None,
    };
    Some(image_words(&resource, desc.as_ref(), unordered))
}

/// [`texture_words`] of a texture of resource description `resource` (as
/// words) through a view of description `desc` (none: the resource's own).
fn image_words(resource: &[u32; 14], desc: Option<&[u32; 10]>, unordered: bool) -> [u32; 4] {
    let width = u64::from(resource[4]) | u64::from(resource[5]) << 32;
    let height = resource[6];
    let (depth, mips) = (resource[7] & 0xFFFF, resource[7] >> 16);
    let (samples, dimension) = (resource[9], resource[0]);
    // Mip count 0: the whole chain.
    let deepest = if dimension == 4 { depth } else { 1 };
    let chain = 64 - (width.max(u64::from(height)).max(u64::from(deepest)) | 1).leading_zeros();
    let mips = if mips == 0 { chain } else { mips };
    // Image types: 8 1D, 9 2D, 10 3D, 11 cube, 12 1D array, 13 2D array,
    // 14 2D MSAA, 15 2D MSAA array.
    let (ty, first, count) = match desc {
        // Shader and unordered view dimensions number alike up to 3D (8).
        Some(d) => {
            let ty = [0, 0, 8, 12, 9, 13, 14, 15, 10, 11, 11].get(d[1] as usize).copied().unwrap_or(0);
            match (unordered, d[1]) {
                (_, 6 | 7) => (ty, 0, 1),
                (true, _) => (ty, d[4], 1),
                (false, _) => (ty, d[4], if d[5] == u32::MAX { mips.saturating_sub(d[4]) } else { d[5] }),
            }
        }
        None => {
            let array = depth > 1;
            let ty = match dimension {
                2 if array => 12,
                2 => 8,
                3 if samples > 1 => 14 + u32::from(array),
                3 => 9 + 4 * u32::from(array),
                _ => 10,
            };
            (ty, 0, if unordered { 1 } else { mips })
        }
    };
    let (w, h) = (width.saturating_sub(1) as u32, height.saturating_sub(1));
    let last = first + count.max(1) - 1;
    [
        0,
        (w & 3) << 30,
        (w >> 2 & 0x3FFF) | (h & 0xFFFF) << 14,
        0xFAC | (first & 0xF) << 12 | (last & 0xF) << 16 | ty << 28,
    ]
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The host copy a buffer view's bytes are in (the one its host view
/// reads, [`crate::shadow::for_view`]) and the view's title range.
pub(crate) fn view_copy(view: &View) -> Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)> {
    let (address, len) = buffer_range(view)?;
    let (shadow, _) = crate::shadow::for_view(address, len, u64::from(view_stride(view)))?;
    Some((shadow, address, len))
}

/// [`buffer_range`] of an unordered access view of memory at `base`.
fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The host resource a slot made by [`make`] views, if any, and the exact
/// title-memory copy behind a buffer view ([`crate::shadow::for_view`]).
/// Keeping that copy prevents loose-window trimming from leaving a stable
/// descriptor pointing at a different or missing copy.
type Kept = Option<(ID3D12Resource, Option<std::sync::Arc<crate::shadow::Shadow>>)>;

/// Buffer views refused so far (diagnostics: the first are logged).
#[cfg(feature = "diagnostics")]
static REFUSED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<Kept> {
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    if element == 0 {
        once!(len, "buffer view of elements without a size");
        return None;
    }
    // The copy its own bytes are in, its first element counted from there.
    let start = address + buffer.FirstElement * element;
    let Some((shadow, _own)) =
        crate::shadow::for_view(start, u64::from(buffer.NumElements) * element, element)
    else {
        #[cfg(feature = "diagnostics")]
        if REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::info!(start = format_args!("{start:#x}"), len, element, count = buffer.NumElements, "refused buffer view: no host copy");
        }
        return None;
    };
    buffer.FirstElement = shadow.offset(start) / element;
    let (first, count) = (buffer.FirstElement, buffer.NumElements);
    if !in_buffer(&shadow.buffer, first, count, element) {
        #[cfg(feature = "diagnostics")]
        if REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::info!(start = format_args!("{start:#x}"), first, count, element, "refused buffer view: empty or past its copy");
        }
        once!(first, count, element, "buffer view empty or past its host copy");
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    // A persistent slot must keep the exact copy it views. The loose-copy
    // cache may otherwise trim the copy and leave this unchanged slot null.
    Some(Some((shadow.buffer.clone(), Some(shadow))))
}

/// Whether `count` elements of `element` bytes from element `first` lie in
/// `buffer`. The console takes a view that is empty or runs past its
/// memory; the host removes the device for one.
fn in_buffer(buffer: &ID3D12Resource, first: u64, count: u32, element: u64) -> bool {
    // SAFETY: a live buffer.
    within(unsafe { buffer.GetDesc() }.Width, first, count, element)
}

fn within(width: u64, first: u64, count: u32, element: u64) -> bool {
    count > 0 && first.saturating_add(u64::from(count)).saturating_mul(element) <= width
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
/// shortcut: what shaders write stays in the host copy; title memory (and
/// so the CPU, and copies run on the CPU) does not see it.
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<Kept> {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    if element == 0 {
        once!(len, "unordered buffer view of elements without a size");
        return None;
    }
    let start = address + buffer.FirstElement * element;
    let (shadow, _own) = crate::shadow::for_view(start, u64::from(buffer.NumElements) * element, element)?;
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    buffer.FirstElement = shadow.offset(start) / element;
    buffer.CounterOffsetInBytes = 0;
    let (first, count) = (buffer.FirstElement, buffer.NumElements);
    if !in_buffer(&shadow.buffer, first, count, element) {
        once!(first, count, element, "unordered buffer view empty or past its host copy");
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), at)
    };
    MARK.with(|m| m.set(shadow.mark_entry(start)));
    Some(Some((shadow.buffer.clone(), Some(shadow))))
}

thread_local! {
    /// The view-mark entry of the unordered buffer view [`make`] last made
    /// on this thread ([`make_marked`]).
    static MARK: std::cell::Cell<Option<(u32, u32)>> = const { std::cell::Cell::new(None) };
}

/// [`make`], and the view-mark entry of the view if it is an unordered
/// buffer view whose writes shaders mark (`victory_shader::emit`, "Write
/// marks").
fn make_marked(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> (Option<Kept>, Option<(u32, u32)>) {
    MARK.with(|m| m.set(None));
    let kept = make(view, at);
    (kept, MARK.with(std::cell::Cell::take))
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in a format neither of the
/// resource's family nor among the formats it was made castable to
/// (`castable`, `resource::castable`). A view of a typed texture in another
/// format of its family takes the texture's format unless it is castable.
/// shortcut: such a view (sRGB of UNORM, say, on a texture the host made
/// without castable formats) reads without its conversion.
fn fits(texture: &ID3D12Resource, castable: &[u32], mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let is_typeless = |f: u32| family(f) == f && family(f + 1) == f;
    let typeless = is_typeless(own);
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    // A typed unordered view in a format the host cannot store to
    // (B5G5R5A1_UNORM on NVIDIA) removes the device when made, in the
    // texture's own format too: the slot stays null, its writes dropped.
    // The format checked is the one the view is made in: a typed
    // texture's own unless the requested one is castable (below).
    let typed = if view == 0 || (!typeless && !castable.contains(&view)) { own } else { view };
    if unordered && !(view == 0 && typeless) && !typed_store(typed) {
        once!(view = typed, own, "an unordered view in a format the host cannot store");
        return false;
    }
    if view == 0 {
        return !typeless && (!unordered || typed_store(own));
    }
    if DEPTH_FORMATS.iter().any(|&(typed, _)| typed == view) || is_typeless(view) {
        once!(unordered, view, own, "view the host does not allow");
        return false;
    }
    // R9G9B9E5 on an R32_UINT resource is not a same-family cast. Creating
    // that view removes the device (debug id 28, then RemoveDevice id 232)
    // even when the resource was made with the format in its castable list.
    // The sample goes to the placed alias (`resource::place_shared_exp`).
    let shared_exp_on_uint = view == 67 && own == 42;
    let same_family = family(view) == family(own);
    let cast = castable.contains(&view)
        && !shared_exp_on_uint
        && (same_family || crate::resource::relaxed_casting());
    if !same_family && !cast {
        once!(view, own, "view format of another family");
        return false;
    }
    let effective = if typeless || cast { view } else { own };
    if unordered && !typed_store(effective) {
        once!(
            view = effective,
            own,
            "an unordered view in a format the host cannot store"
        );
        return false;
    }
    if let Some(format) = format
        && !typeless
        && !cast
    {
        *format = d.Format;
    }
    true
}

/// Whether a view without a description of `resource` (a title texture)
/// may take its host texture's defaults: not when the host texture has
/// another format than the title's (typeless, or shared with a title
/// texture of another format of the same memory).
/// shortcut: such views are null; spell the description out (dimension,
/// mips) if the title makes them.
fn default_view(resource: crate::object::This) -> bool {
    let other = crate::resource::view_format(resource);
    if other.is_some() {
        once!(format = other, "a view without a description of a texture whose host texture has another format");
    }
    other.is_none()
}

/// Whether the host stores to typed unordered views of `format`.
fn typed_store(format: u32) -> bool {
    static KNOWN: Mutex<Vec<(u32, bool)>> = Mutex::new(Vec::new());
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, ok)) = known.iter().find(|(f, _)| *f == format) {
        return ok;
    }
    let mut support = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
        Format: DXGI_FORMAT(format as i32),
        ..Default::default()
    };
    let ok = host().is_some_and(|h| {
        // SAFETY: a plain query into a struct of its size.
        unsafe {
            h.device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_SUPPORT,
                (&raw mut support).cast(),
                size_of_val(&support) as u32,
            )
        }
        .is_ok()
            && support.Support1.0 & D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0 != 0
    });
    known.push((format, ok));
    ok
}

/// Makes the host descriptor for `view` at `at` (in any host view heap):
/// the host resource it views, if it can.
pub(crate) fn make_view(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<ID3D12Resource> {
    make(view, at).flatten().map(|k| k.0)
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    #[cfg(feature = "diagnostics")]
    VIEW_MISSING.with(|reason| reason.set("unclassified"));
    let made = make_inner(view, at);
    #[cfg(feature = "diagnostics")]
    note_view_coverage(view, &made);
    made
}

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct BoundLightingBcLog {
    seen: std::collections::HashSet<(usize, Option<[u32; 10]>)>,
}

#[cfg(feature = "diagnostics")]
static BOUND_LIGHTING_BC_LOGGED: std::sync::OnceLock<Mutex<BoundLightingBcLog>> =
    std::sync::OnceLock::new();

/// One attempt to mirror a title view. Coverage is counted by `make` only
/// once for placed textures, after this resolves them to ordinary views.
fn make_inner(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            // Constant buffers in the title's slots, through the heap itself
            // or a table, are read as raw buffers by translated shaders
            // (`victory_shader::emit`, "Constant buffers in the heap"): the
            // console needs 16-byte alignment, a host constant buffer view
            // 256, which most of the title's do not have on the host.
            let shadow = crate::shadow::find(address, u64::from(size))?;
            let bytes = u64::from(size).min(shadow.left(address));
            let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        NumElements: (bytes / 4) as u32,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                        ..Default::default()
                    },
                },
            };
            buffer_srv(address, bytes, desc, at)
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(copy) = crate::resource::host_copy(resource) {
                copy.viewed.store(true, std::sync::atomic::Ordering::Relaxed);
                let texture = &copy.texture;
                #[cfg(feature = "diagnostics")]
                let raw_desc = *desc;
                let mut desc = desc.as_ref().map(srv_desc);
                if let Some(desc) = desc.as_mut()
                    && desc.Format.0 == 0
                    && let Some(format) = crate::resource::view_format(resource)
                {
                    desc.Format = DXGI_FORMAT(format as i32);
                }
                // Sample the placed R9G9B9E5 resource, not the R32_UINT one.
                // A default SRV of a title f67 texture belongs there too.
                if (desc.as_ref().is_some_and(|d| d.Format.0 == 67)
                    || desc.is_none() && crate::resource::view_format(resource) == Some(67))
                    && let Some(alias) = copy.shared_exp_resource()
                {
                    // SAFETY: a live texture and a slot of our heap. The
                    // alias's own format is R9G9B9E5, so the view is legal.
                    unsafe {
                        device.CreateShaderResourceView(
                            &alias,
                            desc.as_ref().map(std::ptr::from_ref),
                            at,
                        )
                    };
                    return Some(Some((alias, None)));
                }
                if desc.is_none() && !default_view(resource) {
                    crate::resource::note_priority_stream_view(resource, "no default view");
                    return view_missing("no default view");
                }
                if !fits(texture, &copy.castable, desc.as_mut().map(|d| &mut d.Format), false) {
                    crate::resource::note_priority_stream_view(resource, "host format incompatible");
                    return view_missing("format incompatible");
                }
                if copy.dropped > 0
                    && let Some(desc) = desc.as_mut()
                {
                    // SAFETY: a live texture.
                    let mips = unsafe { texture.GetDesc() }.MipLevels;
                    clamp_streamed_mips(desc, copy.dropped, mips);
                }
                #[cfg(feature = "diagnostics")]
                if let Some((memory, _)) = crate::resource::texture_memory(resource)
                    && let Some(index) = crate::resource::priority_bound_source(memory as u64)
                {
                    let mut logged = BOUND_LIGHTING_BC_LOGGED.get_or_init(|| Mutex::new(BoundLightingBcLog::default()))
                        .lock().unwrap_or_else(|e| e.into_inner());
                    if logged.seen.len() < 16 && logged.seen.insert((index, raw_desc)) {
                        // SAFETY: the union member is Texture2D only for
                        // the matching view dimension below.
                        let (first, levels, min_lod) = desc.as_ref().filter(|d| d.ViewDimension == D3D12_SRV_DIMENSION_TEXTURE2D)
                            .map(|d| unsafe { (d.Anonymous.Texture2D.MostDetailedMip, d.Anonymous.Texture2D.MipLevels, d.Anonymous.Texture2D.ResourceMinLODClamp) })
                            .unwrap_or((u32::MAX, 0, f32::NAN));
                        // SAFETY: `texture` is a live host COM resource held
                        // by the streamed copy for this descriptor creation.
                        let host_desc = unsafe { texture.GetDesc() };
                        tracing::info!(index, address = format_args!("{:#x}", memory as u64), ?raw_desc,
                            host_format = host_desc.Format.0, host_width = host_desc.Width, host_height = host_desc.Height,
                            host_mips = host_desc.MipLevels, view_format = desc.as_ref().map(|d| d.Format.0),
                            view_dimension = desc.as_ref().map(|d| d.ViewDimension.0),
                            first, levels, min_lod,
                            mapping = desc.as_ref().map(|d| d.Shader4ComponentMapping),
                            "bound lighting BC host SRV descriptor");
                    }
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                crate::resource::note_priority_stream_view(resource, "host SRV created");
                Some(Some((texture.clone(), None)))
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                buffer_srv(data as u64, len, srv_desc(desc), at)
            } else {
                crate::resource::note_priority_stream_view(resource, "host copy unavailable");
                view_missing("host copy unavailable")
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(copy) = crate::resource::host_copy(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at)
                    }
                    _ => view_missing("host copy unavailable"),
                };
            };
            copy.viewed.store(true, std::sync::atomic::Ordering::Relaxed);
            let texture = &copy.texture;
            // A depth texture's unordered views write its twin.
            // SAFETY: a live resource.
            if unsafe { texture.GetDesc() }.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0 {
                return crate::twin::uav(resource, &copy, desc.as_ref(), at)
                    .map(|twin| Some((twin, None)));
            }
            let mut desc = desc.as_ref().map(uav_desc);
            if desc.is_none() && !default_view(resource) {
                return view_missing("no default view");
            }
            if let Some(desc) = desc.as_mut()
                && desc.Format.0 == 0
                && let Some(format) = crate::resource::view_format(resource)
            {
                desc.Format = DXGI_FORMAT(format as i32);
            }
            if !fits(texture, &copy.castable, desc.as_mut().map(|d| &mut d.Format), true) {
                return view_missing("format incompatible");
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some(Some((texture.clone(), None)))
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words, crate::sampler::aniso_point_mip(device));
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make_inner(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                return view_missing("placed texture unavailable");
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at)
            } else {
                buffer_srv(address, len, srv_desc(words), at)
            }
        }
    }
}

#[cfg(feature = "diagnostics")]
thread_local! {
    static VIEW_MISSING: std::cell::Cell<&'static str> = const { std::cell::Cell::new("unclassified") };
}

fn view_missing(reason: &'static str) -> Option<Kept> {
    #[cfg(feature = "diagnostics")]
    VIEW_MISSING.with(|current| current.set(reason));
    #[cfg(not(feature = "diagnostics"))]
    let _ = reason;
    None
}

#[cfg(feature = "diagnostics")]
type ViewCoverageKey = (&'static str, u32, u32, u32, &'static str);

#[cfg(feature = "diagnostics")]
static VIEW_COVERAGE: Mutex<Option<std::collections::HashMap<ViewCoverageKey, u64>>> = Mutex::new(None);

/// One count for every attempted texture SRV/UAV, including failed placed
/// views. The report records title, requested, and actual host DXGI formats.
/// This is intentionally opt-in: mirroring large shader heaps is hot.
#[cfg(feature = "diagnostics")]
fn note_view_coverage(view: &View, made: &Option<Kept>) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| crate::switch::on("VICTORY_COVERAGE")) {
        return;
    }
    let (kind, title, requested) = match view {
        View::Shader(r, desc) | View::Unordered(r, _, desc) => {
            let Some(title) = crate::resource::texture_desc(*r as crate::object::This).map(|d| d[8]) else { return };
            let kind = if matches!(view, View::Shader(..)) { "srv" } else { "uav" };
            (kind, crate::resource::pc_format(title), desc.map_or(0, |d| d[0]))
        }
        View::Placed(unordered, resource, desc) if resource[0] != 1 => {
            (if *unordered { "placed-uav" } else { "placed-srv" }, crate::resource::pc_format(resource[8]), desc[0])
        }
        _ => return,
    };
    let host = made.as_ref().and_then(|kept| kept.as_ref()).map_or(0, |(resource, _)| {
        // SAFETY: the kept view owns this live COM resource for the duration
        // of the descriptor query.
        unsafe { resource.GetDesc() }.Format.0 as u32
    });
    let outcome = if made.is_some() { "made" } else { VIEW_MISSING.with(|reason| reason.get()) };
    if outcome != "made" {
        let location = match view {
            View::Placed(_, resource, desc) if resource[0] != 1 => {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let width = u64::from(resource[4]) | u64::from(resource[5]) << 32;
                Some((address, width, resource[6], resource[7] >> 16))
            }
            View::Shader(resource, _) | View::Unordered(resource, _, _) => {
                let resource = *resource as crate::object::This;
                crate::resource::texture_memory(resource)
                    .zip(crate::resource::texture_desc(resource))
                    .map(|((address, _), words)| {
                        (address as u64, u64::from(words[4]) | u64::from(words[5]) << 32,
                            words[6], words[7] >> 16)
                    })
            }
            _ => None,
        };
        if let Some((address, width, height, mips)) = location {
            // A global address cap hid later failures of a different format:
            // the first 32 failures were small BC textures, while a hot
            // format-10 view failed tens of thousands of times afterward.
            // Keep two distinct addresses per format and outcome instead.
            type SampleKey = (&'static str, u32, &'static str);
            static SAMPLES: std::sync::OnceLock<Mutex<std::collections::HashMap<SampleKey, std::collections::HashSet<u64>>>> = std::sync::OnceLock::new();
            let mut samples = SAMPLES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
            let addresses = samples.entry((kind, title, outcome)).or_default();
            if addresses.len() < 2 && addresses.insert(address) {
                tracing::info!(address = format_args!("{address:#x}"), width, height, mips,
                    kind, title, requested, outcome, video_memory_mb = ?crate::host::video_memory(),
                    "first missing texture view at address");
            }
        }
    }
    *VIEW_COVERAGE.lock().unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .entry((kind, title, requested, host, outcome)).or_default() += 1;
}

/// All format/outcome combinations since the preceding frame report.
#[cfg(feature = "diagnostics")]
pub(crate) fn take_view_coverage() -> Vec<(ViewCoverageKey, u64)> {
    let rows = VIEW_COVERAGE.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut rows: Vec<_> = rows.into_iter().flatten().collect();
    rows.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// Whether `view` is a view of a texture the title placed in memory itself.
fn is_placed_texture(view: &View) -> bool {
    matches!(view, View::Placed(_, resource, _) if resource[0] != 1)
}

/// Makes what a host view of `view` needs, if it is not made yet: the
/// texture it views (made on first use) or the host copy of the buffer
/// memory it views. The slot's view is then quick to make.
pub(crate) fn prepare(view: &View) {
    #[cfg(test)]
    tests::while_preparing();
    match view {
        View::Placed(..) if is_placed_texture(view) => {
            let _ = placed_texture(view);
        }
        View::Shader(r, _) | View::Unordered(r, _, _) if crate::resource::texture_memory(*r as crate::object::This).is_some() => {
            let _ = crate::resource::host_resource(*r as crate::object::This);
        }
        _ => {
            let _ = view_copy(view);
        }
    }
}

/// Whether `view` is a shader view of a texture (one placed in memory, or a
/// resource object's): such a slot left null is made when read.
fn is_texture(view: &View) -> bool {
    is_placed_texture(view)
        || matches!(view, View::Shader(r, _) if crate::resource::texture_memory(*r as crate::object::This).is_some())
}

/// A sampled title texture whose host copy was not admitted. Views with a
/// made copy but an unsupported format do not enter the admission retry
/// queue: freeing heap bytes cannot make that format legal.
fn missing_streamed_copy(view: &View) -> bool {
    let View::Placed(false, ..) = view else { return false };
    let Some(View::Shader(resource, _)) = placed_texture(view) else { return false };
    let resource = resource as crate::object::This;
    // The low three bits are D3D12 render/depth/UAV uses. Title-side
    // metadata flags (for example 0x300000 on streamed BC textures) do not
    // make a sampled-only texture ineligible for admission recovery.
    crate::resource::texture_desc(resource).is_some_and(|desc| desc[12] & 0x7 == 0)
        && crate::resource::made_copy(resource).is_none()
}

/// The texture identity in a title slot, without making its host copy.
#[cfg(feature = "diagnostics")]
fn texture_view_details(view: &View) -> Option<(&'static str, u64, u64, u32, u32)> {
    match view {
        View::Placed(unordered, resource, desc) if resource[0] != 1 => {
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let width = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            Some((if *unordered { "uav" } else { "srv" }, address, width, resource[6], resource[8]))
        }
        View::Shader(r, _) | View::Unordered(r, _, _) => {
            let resource = *r as crate::object::This;
            let (address, _) = crate::resource::texture_memory(resource)?;
            let desc = crate::resource::texture_desc(resource)?;
            Some((if matches!(view, View::Shader(..)) { "srv" } else { "uav" },
                address as u64, u64::from(desc[4]) | u64::from(desc[5]) << 32,
                desc[6], desc[8]))
        }
        _ => None,
    }
}

/// A view of a texture the title placed in memory itself, as a view of the
/// resource made for that memory (on first use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    let View::Placed(unordered, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let texture = crate::resource::texture_at(address, resource)?;
    let view_desc = desc[..10].try_into().ok();
    Some(if *unordered {
        View::Unordered(texture as usize, 0, view_desc)
    } else {
        View::Shader(texture as usize, view_desc)
    })
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        // Read as raw buffers, as made (`make`).
        View::Constant(..) => return null_buffer_view(at),
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "diagnostics")]
    #[test]
    fn budget_null_ranges_aggregate_and_drain_with_a_cap() {
        let mut ranges = UsedBudgetRanges::default();
        let a = UsedBudgetKey { source: UsedNullSource::Table, address: 0x1000, len: 512 };
        let b = UsedBudgetKey { source: UsedNullSource::ObservedHeap, address: 0x2000, len: 256 };
        let c = UsedBudgetKey { source: UsedNullSource::Table, address: 0x3000, len: 128 };
        ranges.record(a, 7, 0x11, 2, 2);
        ranges.record(b, 8, 0x22, 3, 2);
        ranges.record(a, 9, 0x33, 4, 2);
        ranges.record(c, 10, 0x44, 5, 2);
        let report = ranges.drain_top(2);
        assert_eq!((report.total_uses, report.tracked_distinct_ranges, report.overflow_uses),
            (4, 2, 1));
        assert_eq!(report.top[0].0, a);
        assert_eq!(report.top[0].1.uses, 2);
        assert_eq!((report.top[0].1.example_index, report.top[0].1.example_pipeline,
            report.top[0].1.example_root_slot), (7, 0x11, 2));
        assert_eq!(report.top[1].0, b);
        let empty = ranges.drain_top(16);
        assert_eq!((empty.total_uses, empty.tracked_distinct_ranges, empty.overflow_uses),
            (0, 0, 0));
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn zero_address_texture_is_invalid_not_a_host_materialization_miss() {
        assert_eq!(used_texture_null_reason(0, "placed texture unavailable"), (4, "invalid or empty"));
        assert_eq!(used_texture_null_reason(0, "host copy unavailable"), (4, "invalid or empty"));
        assert_eq!(used_texture_null_reason(0x5c3050000, "placed texture unavailable"),
            (1, "placed texture unavailable"));
        assert_eq!(used_texture_null_reason(0x5c3050000, "host copy unavailable"),
            (0, "host copy unavailable"));
    }

    #[test]
    fn buffer_view_keeps_the_exact_shadow_copy() {
        let Some(host) = host() else {
            return;
        };
        let title = vec![0u8; 256 << 10];
        let address = title.as_ptr() as u64;
        let shadow = crate::shadow::find(address, 64).expect("title memory copy");
        let kept = make(&View::Constant(address, 64), host.cpu_view())
            .flatten()
            .expect("constant buffer view");
        assert!(
            kept.1.as_ref().is_some_and(|copy| std::sync::Arc::ptr_eq(copy, &shadow)),
            "a slot must pin the same copy its descriptor views"
        );
    }

    /// A heap slot first made while the loose-copy budget is closed stays
    /// null for that epoch, then recovers from the same unchanged title
    /// record at submission when the budget opens. A permanently invalid
    /// address is not queued for retry.
    #[test]
    fn budget_denied_buffer_slot_recovers_without_a_title_rewrite() {
        if host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let host = title_heap.host().expect("a host view heap");
        let title = vec![0u8; 256 << 10];
        let address = title.as_ptr() as u64;
        let slot = title_heap.start;
        let high = [(1000.0, 8000.0), (3600.0, 16000.0)];
        let low = [(1000.0, 8000.0), (3000.0, 16000.0)];
        crate::shadow::with_test_admission_epoch(100, || {
            crate::shadow::with_test_video_memory(high, || {
                crate::descriptor::create_cbv(null, &[address as u32, (address >> 32) as u32, 256, 0], slot);
                crate::descriptor::create_cbv(null, &[0, 0, 256, 0], slot + 32);
            });
            assert!(host.made(0) && !host.kept(0), "budget-denied view was left null");
            assert_eq!(host.failed_buffers.lock().unwrap().get(&0), Some(&100));
            assert!(!host.failed_buffers.lock().unwrap().contains_key(&1), "free address is not retryable");
            #[cfg(feature = "diagnostics")]
            {
                let denied = crate::descriptor::view(slot).expect("unchanged CBV record");
                let empty = crate::descriptor::view(slot + 32).expect("null-address CBV record");
                assert_eq!(host.used_null_reason(0, Some(&denied), true),
                    Some((UsedNullReason::Budget, Some((address, 256)))));
                assert_eq!(host.used_null_reason(1, Some(&empty), true),
                    Some((UsedNullReason::InvalidOrEmpty, Some((0, 256)))));
            }
            crate::shadow::with_test_video_memory(low, || {
                host.mirror(0, slot, true);
                host.sync_slots(0, 1, slot, 32);
            });
            assert!(!host.kept(0), "same epoch must not retry on every draw or sync");
            crate::shadow::with_test_admission_epoch(101, || {
                crate::shadow::with_test_video_memory(low, || {
                    host.retry_failed_buffers_with(slot, 32, || true);
                });
            });
            assert!(host.kept(0), "unchanged buffer descriptor was rebuilt after recovery");
            assert!(!host.failed_buffers.lock().unwrap().contains_key(&0));
            #[cfg(feature = "diagnostics")]
            assert_eq!(host.used_null_reason(0, crate::descriptor::view(slot).as_ref(), true), None,
                "recovered host view no longer counts as a shader-used null");
        });
    }

    /// Only a live, at-most-two-window table buffer gets the priority reserve. The
    /// unchanged title descriptor must become a real host view immediately,
    /// and the view must pin the same Shadow later refresh/write-back uses.
    #[test]
    fn statically_used_budget_denied_table_buffer_recovers_in_same_epoch() {
        if host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let host = title_heap.host().expect("a host view heap");
        let layout = std::alloc::Layout::from_size_align(
            (TABLE_PRIORITY_WINDOW * 5) as usize, TABLE_PRIORITY_WINDOW as usize,
        ).unwrap();
        // SAFETY: the aligned live title allocation is intentionally kept for
        // the process lifetime because the loose Shadow caches its address.
        let title = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!title.is_null());
        let address = title as u64;
        let slot = title_heap.start;
        crate::shadow::with_test_admission_epoch(900, || {
            crate::shadow::with_test_video_memory([(1000.0, 8000.0), (3600.0, 16000.0)], || {
                crate::descriptor::create_cbv(null, &[address as u32, (address >> 32) as u32, 256, 0], slot);
                crate::descriptor::create_cbv(null, &[0, 0, 256, 0], slot + 32);
                let crossing = address + 2 * TABLE_PRIORITY_WINDOW - 256;
                crate::descriptor::create_cbv(null, &[crossing as u32, (crossing >> 32) as u32, 512, 0], slot + 64);
                let too_wide = address + 3 * TABLE_PRIORITY_WINDOW - 256;
                let long = (TABLE_PRIORITY_WINDOW + 512) as u32;
                crate::descriptor::create_cbv(null, &[too_wide as u32, (too_wide >> 32) as u32, long, 0], slot + 96);
                assert!(host.made(0) && !host.kept(0), "ordinary admission denied");
                let invalid_was_kept = host.kept(1);
                assert!(!host.kept(2) && !host.kept(3));
                assert!(host.mirror_used_table(0, slot).is_some());
                assert!(host.kept(0), "a statically used table slot recovers now");
                let copy = crate::shadow::find(address, 256).expect("priority copy is reusable");
                let kept = host.kept.lock().unwrap_or_else(|e| e.into_inner());
                assert!(kept[0].as_ref().and_then(|(_, shadow)| shadow.as_ref())
                    .is_some_and(|shadow| std::sync::Arc::ptr_eq(shadow, &copy)));
                drop(kept);
                assert!(!host.failed_buffers.lock().unwrap().contains_key(&0));
                assert!(host.mirror_used_table(1, slot + 32).is_some());
                assert!(host.mirror_used_table(2, slot + 64).is_some());
                assert!(host.kept(2), "a view crossing two windows recovers");
                let crossing_copy = crate::shadow::find(crossing, 512).expect("two-window copy is reusable");
                let kept = host.kept.lock().unwrap_or_else(|e| e.into_inner());
                assert!(kept[2].as_ref().and_then(|(_, shadow)| shadow.as_ref())
                    .is_some_and(|shadow| std::sync::Arc::ptr_eq(shadow, &crossing_copy)));
                drop(kept);
                assert!(host.mirror_used_table(3, slot + 96).is_some());
                assert_eq!(host.kept(1), invalid_was_kept,
                    "null address is untouched by priority recovery");
                assert!(!host.failed_buffers.lock().unwrap().contains_key(&1));
                assert!(!host.kept(3), "a range spanning three windows fails closed");
            });
        });
    }

    /// Mirroring the small view first leaves a live one-window copy that a
    /// crossing view cannot alias. Preseeding the crossing view first lets
    /// both host descriptors and refreshes share one coherent two-window
    /// Shadow, even with ordinary admission closed.
    #[test]
    fn table_union_preseed_prevents_small_view_overlap() {
        if host().is_none() { return; }
        let null = std::ptr::null();
        let mut heap = null;
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let host = title_heap.host().expect("a host view heap");
        let layout = std::alloc::Layout::from_size_align(
            (4 * TABLE_PRIORITY_WINDOW) as usize, TABLE_PRIORITY_WINDOW as usize,
        ).unwrap();
        // SAFETY: the global loose cache can retain the title address after
        // the test, so the aligned live allocation is intentionally leaked.
        let title = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!title.is_null());
        let base = title as u64;
        let slot = title_heap.start;
        let small_a = base + TABLE_PRIORITY_WINDOW - 512;
        let large_a = base + TABLE_PRIORITY_WINDOW - 256;
        let small_b = base + 3 * TABLE_PRIORITY_WINDOW - 512;
        let large_b = base + 3 * TABLE_PRIORITY_WINDOW - 256;
        crate::shadow::with_test_admission_epoch(901, || {
            crate::shadow::with_test_video_memory([(1000.0, 8000.0), (3600.0, 16000.0)], || {
                for (index, address, size) in [
                    (0u32, small_a, 256), (1, large_a, 512),
                    (2, small_b, 256), (3, large_b, 512),
                ] {
                    crate::descriptor::create_cbv(null,
                        &[address as u32, (address >> 32) as u32, size, 0],
                        slot + u64::from(index) * 32);
                }
                assert!(host.mirror_used_table(0, slot).is_some() && host.kept(0));
                assert!(host.mirror_used_table(1, slot + 32).is_some());
                assert!(!host.kept(1), "late union must fail instead of aliasing the first copy");

                let large_view = crate::descriptor::view(slot + 96).unwrap();
                assert!(table_union_candidate(&large_view).is_some());
                assert!(preseed_table_union(&large_view));
                assert!(host.mirror_used_table(2, slot + 64).is_some() && host.kept(2));
                assert!(host.mirror_used_table(3, slot + 96).is_some() && host.kept(3));
                let kept = host.kept.lock().unwrap_or_else(|e| e.into_inner());
                let small_copy = kept[2].as_ref().and_then(|(_, shadow)| shadow.as_ref()).unwrap();
                let large_copy = kept[3].as_ref().and_then(|(_, shadow)| shadow.as_ref()).unwrap();
                assert!(std::sync::Arc::ptr_eq(small_copy, large_copy));
            });
        });
    }

    #[test]
    fn registered_table_union_does_not_spend_priority_reserve() {
        if host().is_none() { return; }
        let layout = std::alloc::Layout::from_size_align(
            (4 * TABLE_PRIORITY_WINDOW) as usize, TABLE_PRIORITY_WINDOW as usize,
        ).unwrap();
        // SAFETY: the registered Shadow can outlive this test through global
        // descriptor or command-list references, so keep title memory live.
        let title = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!title.is_null());
        let base = title as u64;
        crate::shadow::register(base as usize, base, 4 * TABLE_PRIORITY_WINDOW);
        let address = base + TABLE_PRIORITY_WINDOW - 256;
        let view = View::Constant(address, 512);
        crate::shadow::with_test_admission_epoch(902, || {
            crate::shadow::with_test_video_memory([(1000.0, 8000.0), (3600.0, 16000.0)], || {
                let copy = table_priority_copy(&view).expect("registered copy covers the union");
                assert_eq!(copy.range(), (base, 4 * TABLE_PRIORITY_WINDOW));
                let key = std::sync::Arc::as_ptr(&copy) as usize;
                let state = TABLE_PRIORITY.get().unwrap().lock().unwrap_or_else(|e| e.into_inner());
                assert!(!state.live.contains_key(&key),
                    "a registered Shadow is governed by registered memory, not this reserve");
            });
        });
        crate::shadow::unregister(base as usize, base);
    }

    /// Typed buffer views of integer formats carry their kind in the
    /// view-stride table; float, normalized, raw and structured ones none.
    #[test]
    fn integer_typed_buffer_views_are_marked_in_the_stride_table() {
        use victory_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
        // Format, dimension (buffer), first element, count, stride, counter, flags.
        let uav = |format, stride, flags| View::Unordered(0, 0, Some([format, 1, 0, 0, 64, stride, 0, 0, flags, 0]));
        assert_eq!(view_kind(&uav(12, 0, 0)), VIEW_INTEGER, "R16G16B16A16_UINT");
        assert_eq!(view_kind(&uav(57, 0, 0)), VIEW_INTEGER, "R16_UINT");
        assert_eq!(view_kind(&uav(59, 0, 0)), VIEW_INTEGER | VIEW_SIGNED, "R16_SINT");
        assert_eq!(view_kind(&uav(64, 0, 0)), VIEW_INTEGER | VIEW_SIGNED, "R8_SINT");
        assert_eq!(view_kind(&uav(10, 0, 0)), 0, "R16G16B16A16_FLOAT");
        assert_eq!(view_kind(&uav(58, 0, 0)), 0, "R16_SNORM");
        assert_eq!(view_kind(&uav(39, 0, 1)), 0, "raw");
        assert_eq!(view_kind(&uav(0, 16, 0)), 0, "structured");
        // Format, dimension, mapping, first element, count, stride, flags.
        let srv = View::Shader(0, Some([62, 1, 5768, 0, 0, 0, 64, 0, 0, 0]));
        assert_eq!(view_kind(&srv), VIEW_INTEGER, "R8_UINT");
        assert_eq!(view_stride(&uav(12, 0, 0)) | view_kind(&uav(12, 0, 0)), 8 | VIEW_INTEGER);
    }

    /// A placed texture's view the title writes into a heap is left null
    /// and noted once: heap syncs (every submission) pass it by, a draw
    /// reading it through a table makes it.
    #[test]
    fn a_placed_texture_slot_left_null_is_made_once_a_draw_reads_it() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        // A 4x1 RGBA8 texture, linear, in memory of its own, and a 2D view.
        let memory = vec![0u8; 1 << 16];
        let address = memory.as_ptr() as u64;
        // Streamed textures carry title metadata in the high flag bits.
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0x300000, 0];
        let mut view = [0u32; 16];
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        crate::descriptor::create_placed_srv(null, &words, &view, title_heap.start);
        let host = title_heap.host().expect("a host view heap");
        for _ in 0..2 {
            assert!(host.mirror(0, title_heap.start, false).is_some());
            assert!(host.made(0), "the record is noted");
            assert!(!host.kept(0), "no host texture for it yet");
        }
        assert!(host.mirror(0, title_heap.start, true).is_some());
        assert!(host.kept(0), "made for the draw");
    }

    #[test]
    fn observed_texture_retry_waits_for_new_space_or_open_admission() {
        let miss = TextureMiss { key: 42, reuse_epoch: 7, room_open: false };
        assert!(!miss.ready(7, false), "a closed cap with no freed range must not spin");
        assert!(miss.ready(8, false), "a newly reusable existing range permits one retry");
        assert!(miss.ready(7, true), "ordinary admission reopening permits one retry");
        assert!(!TextureMiss { room_open: true, ..miss }.ready(7, true));
    }

    /// The read-mark bitmap reports a heap slot once. A failed first read
    /// must still materialize when admission returns, with no title rewrite
    /// and no second observed read.
    #[test]
    fn observed_placed_texture_miss_recovers_without_title_rewrite() {
        if host().is_none() {
            return;
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let memory = vec![0u8; 1 << 18];
        let address = memory.as_ptr() as u64;
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let mut view = [0u32; 16];
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        crate::descriptor::create_placed_srv(null, &words, &view, title_heap.start);
        let host = title_heap.host().expect("a host view heap");
        crate::resource::with_test_stream_room(false, || crate::placement::with_test_no_existing(|| {
            let used = host.mirror(0, title_heap.start, true);
            assert!(used.is_some(), "the title record remains valid");
            assert!(!host.kept(0), "a closed cap leaves a null host view");
            host.note_observed_texture_result(0, used.as_ref());
        }));
        // A second observed miss is then rewritten by the title. Its old
        // queued generation must not materialize over the new descriptor.
        let second_slot = title_heap.start + 32;
        let mut second = view;
        (second[10], second[11]) = ((address + (1 << 16)) as u32, ((address + (1 << 16)) >> 32) as u32);
        crate::descriptor::create_placed_srv(null, &words, &second, second_slot);
        crate::resource::with_test_stream_room(false, || crate::placement::with_test_no_existing(|| {
            let used = host.mirror(1, second_slot, true);
            assert!(!host.kept(1));
            host.note_observed_texture_result(1, used.as_ref());
        }));
        assert_eq!(host.observed_texture_misses.lock().unwrap().len(), 2);
        (second[10], second[11]) = ((address + (2 << 16)) as u32, ((address + (2 << 16)) >> 32) as u32);
        crate::descriptor::create_placed_srv(null, &words, &second, second_slot);
        crate::resource::with_test_stream_room(true, crate::descriptor::retry_observed_textures);
        assert!(host.kept(0), "the same slot is materialized after admission reopens");
        assert!(!host.kept(1), "a rewritten unobserved slot stays deferred");
        assert!(host.observed_texture_misses.lock().unwrap().is_empty());
    }

    /// A slot viewing a texture given up (`crate::resource::evict`) holds it
    /// no more: heap syncs pass it by, a draw reading it through a table
    /// makes it again.
    #[test]
    fn a_slot_viewing_a_texture_given_up_is_made_again_when_read() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        // A 4x1 RGBA8 texture, linear, in memory of its own, and a 2D view.
        let memory = vec![0u8; 1 << 16];
        let address = memory.as_ptr() as u64;
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let mut view = [0u32; 16];
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        crate::descriptor::create_placed_srv(null, &words, &view, title_heap.start);
        let host = title_heap.host().expect("a host view heap");
        assert!(host.mirror(0, title_heap.start, true).is_some());
        assert!(host.kept(0), "made for the draw");
        let texture = crate::resource::texture_at(address, &words).expect("its resource");
        assert!(crate::resource::give_up_now(texture));
        assert!(!host.kept(0), "the slot holds it no more");
        assert!(host.made(0), "its record stays noted");
        assert!(host.mirror(0, title_heap.start, false).is_some());
        assert!(!host.kept(0), "heap syncs pass it by");
        assert!(host.mirror(0, title_heap.start, true).is_some());
        assert!(host.kept(0), "made again for the draw");
        assert!(crate::resource::made_copy(texture).is_some(), "with a host texture again");
    }

    /// Work to run while a view's texture is prepared, once (tests).
    pub(crate) static WHILE_PREPARING: std::sync::Mutex<Option<crate::descriptor::tests::Work>> = std::sync::Mutex::new(None);

    pub(crate) fn while_preparing() {
        let work = WHILE_PREPARING.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(work) = work {
            work();
        }
    }

    /// A record the title writes into a slot while another thread prepares
    /// the slot's old record is the one the slot ends up viewing.
    #[test]
    fn a_slot_rewritten_while_its_view_is_prepared_views_the_new_record() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 1024, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let s = 960u32;
        let at = title_heap.start + u64::from(s) * 32;
        let memory = vec![0u8; 1 << 17];
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let view_of = |address: u64| {
            let mut view = [0u32; 16];
            view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
            (view[10], view[11]) = (address as u32, (address >> 32) as u32);
            view
        };
        let (first, second) = (memory.as_ptr() as u64, memory.as_ptr() as u64 + (1 << 16));
        crate::descriptor::create_placed_srv(null, &words, &view_of(first), at);
        let host = title_heap.host().expect("a host view heap");
        let (done, waited) = std::sync::mpsc::channel();
        let second_view = view_of(second);
        *WHILE_PREPARING.lock().unwrap() = Some(Box::new(move || {
            // The title writes the other record into the slot meanwhile.
            std::thread::spawn(move || {
                crate::descriptor::create_placed_srv(std::ptr::null(), &words, &second_view, at);
                done.send(()).unwrap();
            });
            assert!(waited.recv_timeout(std::time::Duration::from_secs(5)).is_ok(), "the write waits");
        }));
        // A draw reads the slot: its first record is prepared.
        assert!(host.mirror(s, at, true).is_some());
        let second_texture = crate::resource::texture_at(second, &words).expect("its resource");
        let raw = windows::core::Interface::as_raw(&crate::resource::made_copy(second_texture).expect("made").texture) as usize;
        assert_eq!(host.textures_at(&[s]), [raw], "the slot views the record written last");
    }

    /// What a view needs is made without the slot lock: another thread
    /// binding a view of the same heap meanwhile is not held up.
    #[test]
    fn views_bound_while_another_slot_gets_its_texture_do_not_wait() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 1024, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let (first, second) = (950u32, 951u32);
        let memory = vec![0u8; 1 << 17];
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let slot = |i: u32| title_heap.start + u64::from(i) * 32;
        for (i, at) in [(first, 0u64), (second, 1 << 16)] {
            let address = memory.as_ptr() as u64 + at;
            let mut view = [0u32; 16];
            view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
            (view[10], view[11]) = (address as u32, (address >> 32) as u32);
            crate::descriptor::create_placed_srv(null, &words, &view, slot(i));
        }
        let host = title_heap.host().expect("a host view heap");
        let heap_at = title_heap.start;
        let heap = heap as usize;
        let (done, waited) = std::sync::mpsc::channel();
        *WHILE_PREPARING.lock().unwrap() = Some(Box::new(move || {
            std::thread::spawn(move || {
                let h = crate::descriptor::heap(heap as crate::object::This);
                h.host().unwrap().mirror(second, heap_at + u64::from(second) * 32, true);
                done.send(()).unwrap();
            });
            assert!(
                waited.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
                "a view bound while another slot's texture is made waits"
            );
        }));
        assert!(host.mirror(first, slot(first), true).is_some());
        assert!(WHILE_PREPARING.lock().unwrap().is_none(), "the prepare ran");
        assert!(host.kept(first) && host.kept(second), "both made");
    }

    /// The textures of slots shaders read are made without the heaps' lock:
    /// a title thread writing a view meanwhile is not held up by the make.
    #[test]
    fn views_written_while_read_slots_get_their_textures_do_not_wait() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 1024, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let (read, written) = (900u32, 901u32);
        // A 4x1 RGBA8 texture, linear, in memory of its own, and a 2D view.
        let memory = vec![0u8; 1 << 16];
        let address = memory.as_ptr() as u64;
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let mut view = [0u32; 16];
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        let slot = |i: u32| title_heap.start + u64::from(i) * 32;
        crate::descriptor::create_placed_srv(null, &words, &view, slot(read));
        let host = title_heap.host().expect("a host view heap");
        assert!(!host.kept(read), "left null until read");
        let (done, waited) = std::sync::mpsc::channel();
        let at = slot(written);
        *crate::descriptor::tests::WHILE_MAKING.lock().unwrap() = Some((
            read,
            Box::new(move || {
                std::thread::spawn(move || {
                    crate::descriptor::create_placed_srv(std::ptr::null(), &words, &view, at);
                    done.send(()).unwrap();
                });
                assert!(
                    waited.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
                    "a view written while a read slot's texture is made waits"
                );
            }),
        ));
        crate::descriptor::make_read(&[read]);
        assert!(crate::descriptor::tests::WHILE_MAKING.lock().unwrap().is_none(), "the make ran");
        assert!(host.kept(read), "made for the read");
    }

    /// A heap sync makes the slots whose records the title changed as bytes
    /// (a copy of another slot's record, a record cleared) and passes the
    /// others by.
    #[test]
    fn heap_syncs_make_the_slots_whose_records_changed() {
        if host().is_none() {
            return; // no host GPU here
        }
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 2300, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(0x8efb471d, 0x616c, 0x4f49, [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51]);
        assert_eq!(crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap), 0);
        let title_heap = crate::descriptor::heap(heap);
        let start = title_heap.start;
        // A view of no memory: a view of the test's own heap memory would get
        // a host copy whose 4 MiB window the guards may cover (the process
        // heap guarded read-only hung the suite).
        crate::descriptor::create_cbv(null, &[0, 0, 256, 0], start);
        let host = title_heap.host().expect("a host view heap");
        // SAFETY: slots in the heap's memory, 32 bytes each. They cross
        // both 1,024-slot chunk boundaries.
        unsafe {
            for index in [1u64, 1023, 1024, 2049] {
                std::ptr::copy_nonoverlapping(start as *const u8, (start + index * 32) as *mut u8, 32);
            }
        };
        assert!(!host.made(1), "copied as bytes: not seen yet");
        host.sync_slots(0, 2300, start, 32);
        assert!(host.made(0) && !host.made(2));
        for index in [1, 1023, 1024, 2049] {
            assert!(host.made(index), "copied slot {index} was mirrored");
        }
        // SAFETY: as above.
        unsafe {
            for index in [1u64, 1023, 1024, 2049] {
                std::ptr::write_bytes((start + index * 32) as *mut u8, 0, 32);
            }
        };
        host.sync_slots(0, 2300, start, 32);
        assert!(host.made(0));
        for index in [1, 1023, 1024, 2049] {
            assert!(!host.made(index), "cleared slot {index} was released");
        }
    }

    /// The world's 16-byte view of 0 elements removed the device.
    #[test]
    fn empty_and_overlong_buffer_views_are_refused() {
        assert!(within(64, 0, 4, 16));
        assert!(within(64, 3, 1, 16));
        assert!(!within(64, 0, 0, 16));
        assert!(!within(64, 3, 2, 16));
        assert!(!within(64, u64::MAX, 1, 16));
    }

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// A table made afresh lies in dynamic slots, and a shader reading a
    /// view's address (e.g. the BVH builder's base) indexes the view
    /// addresses there: they have an entry for each dynamic slot, entered
    /// whenever the slot is made (a view, null, or a raw view of ours).
    /// Giving a slot back leaves its entry: every table made afresh makes
    /// all its slots. (Hosts whose view heaps are smaller than the title's,
    /// NVIDIA, make such tables; the shader read past the buffer.)
    ///
    /// Any heap size the host takes will do: a title heap of 4 slots leaves
    /// dynamic slots past it either way ([`DYNAMIC`] or [`MIN_DYNAMIC`]).
    #[test]
    fn view_addresses_cover_tables_made_afresh() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let end = heap.count() + heap.dynamic;
        assert!(heap.view_addresses().is_some());
        let first = heap.dynamic(2).unwrap();
        assert!(first >= heap.count() && first + 1 < end);
        // A buffer the title placed at 0xd_6013_e000: 16 elements of 4 bytes.
        let address = 0xd_6013_e000u64;
        let mut resource = [0u32; 14];
        (resource[0], resource[4]) = (1, 64);
        let mut desc = [0u32; 16];
        (desc[1], desc[2], desc[6], desc[7]) = (1, D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, 16, 4);
        (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
        let view = View::Placed(false, resource, desc);
        assert_eq!(buffer_range(&view), Some((address, 64)));
        assert_eq!(heap.address_at(first), Some(0), "not made yet");
        heap.make_at(first, &view);
        assert_eq!(heap.address_at(first), Some(address));
        assert_eq!(heap.address_at(first + 1), Some(0), "not made yet");
        // The last dynamic slot has its entry too.
        heap.make_at(end - 1, &view);
        assert_eq!(heap.address_at(end - 1), Some(address));
        // A slot given back keeps its entry until it is made again; made
        // null, it has its entry cleared.
        heap.release(first, 2);
        assert_eq!(
            heap.address_at(first),
            Some(address),
            "given back, not made"
        );
        heap.make_null(first);
        assert_eq!(heap.address_at(first), Some(0));
        assert_eq!(heap.address_at(end - 1), Some(address), "still made");
        // Raw views of ours (root views, host copies' view pairs) in dynamic
        // slots clear what an earlier table entered there.
        heap.make_at(first, &view);
        heap.raw_view_at(first, None, 0, 0, false);
        assert_eq!(heap.address_at(first), Some(0), "a root view");
        heap.make_at(first, &view);
        heap.make_at(first + 1, &view);
        heap.raw_pair_at(first, None, 0);
        assert_eq!(heap.address_at(first), Some(0), "a view pair");
        assert_eq!(heap.address_at(first + 1), Some(0), "a view pair");
        // Slots past the dynamic ones (host copies' view pairs) have no
        // entry, and nothing is written past the last: a mark there (the
        // buffer's allocation reaches past it) stays.
        assert_eq!(heap.address_at(end), None);
        let host = host().unwrap();
        let (buffer, data, _) = heap.addresses.get().unwrap().as_ref().unwrap();
        // SAFETY: plain queries of a live buffer.
        let (desc, size) = unsafe {
            let desc = buffer.GetDesc();
            let info = host.device.GetResourceAllocationInfo(0, &[desc]);
            (desc, info.SizeInBytes)
        };
        assert_eq!(desc.Width, 16 * u64::from(end));
        assert!(size >= desc.Width + 8, "room for the mark");
        let past = (data + 16 * end as usize) as *mut u64;
        let mark = 0x5a5a_5a5a_5a5a_5a5a;
        // SAFETY: mapped memory of the buffer's allocation (checked).
        unsafe { past.write_unaligned(mark) };
        heap.note_address(end, Some(&view));
        // SAFETY: as above.
        assert_eq!(unsafe { past.read_unaligned() }, mark);
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        // Every index a translated shader can reach exists (clamped below
        // `HEAP_VIEWS`, `HEAP_SAMPLERS`).
        // SAFETY: plain queries of live heaps.
        unsafe {
            assert!(heap.heap.GetDesc().NumDescriptors >= victory_shader::emit::HEAP_VIEWS);
            let samplers = HostHeap::new(true, 128).unwrap();
            assert!(samplers.heap.GetDesc().NumDescriptors >= victory_shader::emit::HEAP_SAMPLERS);
        }
        // A slot that stops viewing a resource leaves it alive until the GPU
        // work in flight has run (a read of a freed host texture otherwise).
        // SAFETY: a plain committed buffer; one reference taken and given back.
        unsafe {
            let host = host().unwrap();
            let mut buffer: Option<ID3D12Resource> = None;
            host.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 256,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .unwrap();
            let buffer = buffer.unwrap();
            let references = |b: &ID3D12Resource| {
                let raw = windows::core::Interface::as_raw(b);
                let unknown = &*(raw as *const *const windows::core::IUnknown_Vtbl);
                ((**unknown).AddRef)(raw);
                ((**unknown).Release)(raw)
            };
            let slot = heap.count + 2;
            heap.raw_view_at(slot, Some(&buffer), 0, 256, false);
            assert_eq!(references(&buffer), 2, "ours and the slot's");
            heap.make_null(slot);
            assert!(references(&buffer) >= 2, "the slot's, retired");
            crate::host::release_retired();
            // Released on a thread of its own.
            let since = std::time::Instant::now();
            while references(&buffer) > 1 && since.elapsed() < std::time::Duration::from_secs(5) {
                std::thread::yield_now();
            }
            assert_eq!(references(&buffer), 1, "ours");
        }
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    #[test]
    fn dynamic_search_can_find_a_run_across_the_cursor() {
        let Some(heap) = HostHeap::new(false, 4) else {
            eprintln!("no host descriptor heap; skipped");
            return;
        };
        let first = heap.dynamic(1).unwrap();
        let left = heap.dynamic(2).unwrap();
        let right = heap.dynamic(2).unwrap();
        let rest = heap.dynamic(heap.dynamic - 5).unwrap();
        assert_eq!((first, left, right, rest), (4, 5, 7, 9));

        // Reallocating the left half puts the cursor back in the middle.
        heap.release(left, 2);
        assert_eq!(heap.dynamic(2), Some(left));
        heap.release(left, 2);
        heap.release(right, 2);
        assert_eq!(heap.dynamic(4), Some(left));
        assert_eq!(heap.dynamic(1), None);

        // A free slot at each physical end does not form a two-slot range.
        heap.release(first, 1);
        let last = first + heap.dynamic - 1;
        heap.release(last, 1);
        assert_eq!(heap.dynamic(2), None);
    }

    /// A texture view's size words are the console image descriptor's
    /// fields: level 0's size of the resource, the view's levels, its type.
    #[test]
    fn a_texture_view_gives_its_image_descriptor_size_fields() {
        // A 1000x600 2D texture of 10 levels, then an array of 6 with the
        // whole chain (mip count 0, 1024 wide: 11 levels).
        let mut texture = [0u32; 14];
        (texture[0], texture[4], texture[6], texture[7], texture[9]) = (3, 1000, 600, 1 | 10 << 16, 1);
        let mut array = texture;
        (array[4], array[7]) = (1024, 6);
        let mut view = [0u32; 10];
        // Levels 2-4 of the texture: width - 1 = 999 (0b11_1110_0111).
        (view[1], view[4], view[5]) = (4, 2, 3);
        let words = image_words(&texture, Some(&view), false);
        assert_eq!(words, [0, 3 << 30, 249 | 599 << 14, 0xFAC | 2 << 12 | 4 << 16 | 9 << 28]);
        // Its whole chain from level 1.
        (view[4], view[5]) = (1, u32::MAX);
        assert_eq!(image_words(&texture, Some(&view), false)[3], 0xFAC | 1 << 12 | 9 << 16 | 9 << 28);
        // An unordered view of level 3: that level alone.
        let uav = [0, 4, 0, 0, 3, 0, 0, 0, 0, 0];
        assert_eq!(image_words(&texture, Some(&uav), true)[3], 0xFAC | 3 << 12 | 3 << 16 | 9 << 28);
        // The array's own view: a 2D array, levels 0-10.
        let words = image_words(&array, None, false);
        assert_eq!(words[1..], [3 << 30, 255 | 599 << 14, 0xFAC | 10 << 16 | 13 << 28]);
    }

    fn placed_memory() -> u64 {
        let layout = std::alloc::Layout::from_size_align(1 << 16, 1 << 16).unwrap();
        // SAFETY: non-zero size and power-of-two alignment; leaked for the test.
        unsafe { std::alloc::alloc_zeroed(layout) as u64 }
    }

    fn placed_tex_desc(width: u64, height: u32, format: u32, flags: u32) -> [u32; 14] {
        [
            3,
            0,
            0,
            0,
            width as u32,
            (width >> 32) as u32,
            height,
            1 | (1 << 16),
            format,
            1,
            0,
            0,
            flags,
            0,
        ]
    }

    fn placed_tex_uav_desc(va: u64, view_format: u32) -> [u32; 16] {
        let mut desc = [0u32; 16];
        desc[0] = view_format;
        desc[1] = 4; // D3D12_UAV_DIMENSION_TEXTURE2D
        desc[10] = va as u32;
        desc[11] = (va >> 32) as u32;
        desc
    }

    fn placed_tex_srv_desc(va: u64, view_format: u32) -> [u32; 16] {
        let mut desc = [0u32; 16];
        desc[0] = view_format;
        desc[1] = 4; // D3D12_SRV_DIMENSION_TEXTURE2D
        desc[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        desc[5] = 1; // MipLevels
        desc[10] = va as u32;
        desc[11] = (va >> 32) as u32;
        desc
    }

    #[test]
    fn placed_uav_textures_create_real_host_uavs() {
        let Some(host) = host() else {
            return;
        };
        let Some(heap) = HostHeap::new(false, 32) else {
            return;
        };
        let _ = host.log_messages();
        let bgrx_uav = typed_store(88);
        let cases = [
            (28u32, 28u32, 4u32, true),     // R8G8B8A8_UNORM
            (27u32, 28u32, 4u32, true),     // R8G8B8A8_TYPELESS -> R8G8B8A8_UNORM
            (29u32, 29u32, 4u32, true),     // R8G8B8A8_UNORM_SRGB -> linear R8G8B8A8_UNORM
            (29u32, 0u32, 4u32, true),      // R8G8B8A8_UNORM_SRGB + UNKNOWN -> linear 28
            (91u32, 91u32, 4u32, true),     // B8G8R8A8_UNORM_SRGB -> linear B8G8R8A8_UNORM
            (91u32, 0u32, 4u32, true),      // B8G8R8A8_UNORM_SRGB + UNKNOWN -> linear 87
            (28u32, 28u32, 0u32, false),    // missing ALLOW_UNORDERED_ACCESS -> null fallback
            (93u32, 93u32, 4u32, bgrx_uav), // B8G8R8X8_UNORM_SRGB -> linear 88 if supported
            (93u32, 0u32, 4u32, bgrx_uav),  // B8G8R8X8_UNORM_SRGB + UNKNOWN -> linear 88
            (92u32, 88u32, 4u32, bgrx_uav), // B8G8R8X8_TYPELESS -> B8G8R8X8_UNORM
            (92u32, 93u32, 4u32, bgrx_uav), // B8G8R8X8_TYPELESS -> B8G8R8X8_UNORM_SRGB
            (40u32, 40u32, 4u32, false),    // D32_FLOAT + UAV flag (no depth flag) -> null fallback
            (55u32, 55u32, 4u32, false),    // D16_UNORM + UAV flag (no depth flag) -> null fallback
            (39u32, 40u32, 4u32, false),    // R32_TYPELESS + D32_FLOAT view -> null fallback
            (53u32, 55u32, 4u32, false),    // R16_TYPELESS + D16_UNORM view -> null fallback
            (27u32, 27u32, 4u32, false),    // R8G8B8A8_TYPELESS + typeless view -> null fallback
        ];
        let mut slots = [[0u32; 8]; 24];
        for (idx, (res_fmt, view_fmt, flags, expect_real)) in cases.into_iter().enumerate() {
            let va = placed_memory();
            let resource = placed_tex_desc(4, 4, res_fmt, flags);
            let desc = placed_tex_uav_desc(va, view_fmt);
            let slot_addr = slots[idx].as_mut_ptr() as u64;
            crate::descriptor::create_placed_uav(std::ptr::null(), &resource, &desc, slot_addr);
            let view = crate::descriptor::view(slot_addr).unwrap();
            assert!(matches!(
                placed_texture(&view),
                Some(View::Unordered(_, 0, Some(_)))
            ));
            assert!(heap.mirror(idx as u32, slot_addr, false).is_some());
            assert!(
                heap.kept.lock().unwrap()[idx].is_none(),
                "case {idx} mirror(false) should defer host creation"
            );
            let mirrored = heap.mirror(idx as u32, slot_addr, true).unwrap();
            let dyn_slot = heap.count + idx as u32;
            heap.make_at(dyn_slot, &mirrored);
            let kept = heap.kept.lock().unwrap();
            assert_eq!(kept[idx].is_some(), expect_real, "case {idx} mirror");
            assert_eq!(
                kept[dyn_slot as usize].is_some(),
                expect_real,
                "case {idx} make_at"
            );
        }
        for (offset, (res_fmt, view_fmt, expect_real)) in [
            (39u32, 40u32, false), // R32_TYPELESS + D32_FLOAT SRV -> null fallback
            (53u32, 55u32, false), // R16_TYPELESS + D16_UNORM SRV -> null fallback
            (29u32, 0u32, true),   // R8G8B8A8_UNORM_SRGB + UAV + UNKNOWN SRV -> 29
            (91u32, 0u32, true),   // B8G8R8A8_UNORM_SRGB + UAV + UNKNOWN SRV -> 91
        ]
        .into_iter()
        .enumerate()
        {
            let idx = cases.len() + offset;
            let va = placed_memory();
            let resource = placed_tex_desc(4, 4, res_fmt, 4);
            let desc = placed_tex_srv_desc(va, view_fmt);
            let slot_addr = slots[idx].as_mut_ptr() as u64;
            crate::descriptor::create_placed_srv(std::ptr::null(), &resource, &desc, slot_addr);
            heap.mirror(idx as u32, slot_addr, true).unwrap();
            assert_eq!(
                heap.kept.lock().unwrap()[idx].is_some(),
                expect_real,
                "srv case {offset}"
            );
        }
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
        assert_eq!(host.log_messages(), None);
    }
}
