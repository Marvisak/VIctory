//! Host textures placed in heaps made ahead instead of committed one by
//! one: a new shot brings hundreds of textures, and the host spent most of
//! a frame making them (300 textures, 430 MB on the RX 6700 XT: committed
//! 255 ms; one heap 32 ms, then placed 3 ms).
//!
//! Textures without render-target or depth use, up to a quarter of a heap,
//! go into heaps of [`HEAP`] bytes, first fit. Unordered-access textures
//! take heaps that allow shader atomics, so each one is not its own
//! committed resource. A texture's range is free again once nothing else
//! holds its resource (heap slots keep views' resources until the running
//! submission has run, lists until they are reset), so no view or list
//! sees its memory reused. Empty heaps past [`SPARE`] are released.
//!
//! Write-back buffers (host copies of title memory, a list's argument
//! buffer) share heaps the same way. One committed buffer each exhausted
//! kernel memory during world load: the device was removed at about
//! 9,500 live heaps.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Bytes of one heap.
const HEAP: u64 = 64 << 20;
/// Empty heaps kept for the next textures.
const SPARE: usize = 2;
/// Heap alignment. Eligible small textures may occupy 4 KB aligned ranges
/// inside these 64 KB aligned heaps.
const ALIGN: u64 = 64 << 10;

/// Where a texture lies: heap, offset and bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    heap: u64,
    offset: u64,
    size: u64,
    /// The range has never been handed out. Its pages are still demand-zero.
    fresh: bool,
    /// Bytes charged to the optional guarded tail allowance until this
    /// resource really releases its placement (or creation is rejected).
    pristine_charge: u64,
}

/// A heap and its free ranges (offset → bytes).
struct Heap {
    heap: ID3D12Heap,
    id: u64,
    free: BTreeMap<u64, u64>,
    /// Bytes from the start that have been handed out. A later range below
    /// this may hold an older buffer.
    touched: u64,
    /// First-touch frontier when the guarded tail path was enabled. Ranges
    /// above this stay charged even if a failed creation gives them back.
    pristine_floor: Option<u64>,
}

#[derive(Default)]
struct Heaps {
    heaps: Vec<Heap>,
    /// Unordered-access textures. Their heaps allow shader atomics, so
    /// they cannot share [`Self::heaps`].
    uav: Vec<Heap>,
    next: u64,
    /// Ranges whose texture may still be held elsewhere, with a holder of
    /// our own.
    pending: Vec<(ID3D12Resource, Placement)>,
    /// Live and pending resources placed in the optional tail zone.
    pristine_active: u64,
}

/// Cleared when this host refuses a shader-atomic texture heap. Later
/// unordered-access textures stay committed.
static UAV_HEAPS: AtomicBool = AtomicBool::new(true);

// SAFETY: D3D12 heaps and resources are free-threaded; used under the lock.
unsafe impl Send for Heaps {}

static HEAPS: Mutex<Option<Heaps>> = Mutex::new(None);

/// Advances only when a previously live regular texture range is available
/// for reuse. A failed CreatePlacedResource does not advance it: retrying a
/// denied stream view on that signal would spin against the same shortage.
static TEXTURE_REUSE_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reap completed texture placements before a bounded retry of shader-used
/// views. The epoch lets a closed-cap retry run only when existing heap
/// capacity actually changed.
pub(crate) fn texture_reuse_epoch() -> u64 {
    if let Some(heaps) = HEAPS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        heaps.reap();
    }
    TEXTURE_REUSE_EPOCH.load(Ordering::Relaxed)
}

/// The current epoch without a placement-pool scan, for many observed
/// texture misses reported together after one submission.
pub(crate) fn texture_reuse_epoch_snapshot() -> u64 {
    TEXTURE_REUSE_EPOCH.load(Ordering::Relaxed)
}

/// The first range of `free` that fits `size` bytes at `align`: its
/// offset, taken out of `free`.
fn take(free: &mut BTreeMap<u64, u64>, size: u64, align: u64) -> Option<u64> {
    take_below(free, size, align, u64::MAX)
}

/// Take a range at or above a fixed frontier in an already allocated heap.
/// The result may include bytes once handed out and then rejected; they still
/// count against the guarded allowance on every later attempt.
fn take_at_or_after(free: &mut BTreeMap<u64, u64>, size: u64, align: u64, floor: u64) -> Option<u64> {
    let (&start, &len, at) = free.iter().find_map(|(s, l)| {
        let at = s.max(&floor).next_multiple_of(align);
        (at.checked_add(size).is_some_and(|after| after <= s.saturating_add(*l)))
            .then_some((s, l, at))
    })?;
    free.remove(&start);
    if at > start {
        free.insert(start, at - start);
    }
    if at + size < start + len {
        free.insert(at + size, start + len - at - size);
    }
    Some(at)
}

/// Like [`take`], but never hand out bytes past the previously used end of
/// a heap. An over-budget root view may only reuse an old write-back range.
fn take_below(free: &mut BTreeMap<u64, u64>, size: u64, align: u64, end: u64) -> Option<u64> {
    let (&start, &len, at) = free.iter().find_map(|(s, l)| {
        let at = s.next_multiple_of(align);
        (at.checked_add(size).is_some_and(|after| after <= s + l && after <= end))
            .then_some((s, l, at))
    })?;
    free.remove(&start);
    if at > start {
        free.insert(start, at - start);
    }
    if at + size < start + len {
        free.insert(at + size, start + len - at - size);
    }
    Some(at)
}

/// Gives `[offset, offset + size)` back to `free`, joined with its
/// neighbours.
fn give(free: &mut BTreeMap<u64, u64>, offset: u64, size: u64) {
    let (mut start, mut end) = (offset, offset + size);
    if let Some((&s, &l)) = free.range(..start).next_back()
        && s + l == start
    {
        free.remove(&s);
        start = s;
    }
    if let Some(&l) = free.get(&end) {
        free.remove(&end);
        end += l;
    }
    free.insert(start, end - start);
}

/// Whether a resource has no holder but the one passed.
fn last_holder(resource: &ID3D12Resource) -> bool {
    // SAFETY: a live COM object; one reference taken and given back.
    unsafe {
        let raw = resource.as_raw();
        let unknown = &*(raw as *const *const windows::core::IUnknown_Vtbl);
        ((**unknown).AddRef)(raw);
        ((**unknown).Release)(raw) == 1
    }
}

impl Heaps {
    /// Frees the pending ranges nothing else holds now, and releases empty
    /// heaps past [`SPARE`].
    fn reap(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            if last_holder(&self.pending[i].0) {
                let (_, p) = self.pending.swap_remove(i);
                self.pristine_active = self.pristine_active.saturating_sub(p.pristine_charge);
                if let Some(h) = self.heaps.iter_mut().find(|h| h.id == p.heap) {
                    give(&mut h.free, p.offset, p.size);
                    TEXTURE_REUSE_EPOCH.fetch_add(1, Ordering::Relaxed);
                } else if let Some(h) = self.uav.iter_mut().find(|h| h.id == p.heap) {
                    give(&mut h.free, p.offset, p.size);
                }
            } else {
                i += 1;
            }
        }
        release_empty(&mut self.heaps);
        release_empty(&mut self.uav);
    }

    /// A range of `size` bytes: in a heap with room, else in a new heap.
    /// `uav` selects the shader-atomic heaps.
    fn allocate(&mut self, device: &ID3D12Device, size: u64, align: u64, uav: bool) -> Option<(ID3D12Heap, Placement)> {
        if uav && !UAV_HEAPS.load(Ordering::Relaxed) {
            return None;
        }
        let existing = {
            let pool = if uav { &mut self.uav } else { &mut self.heaps };
            pool.iter_mut().find_map(|h| {
                take(&mut h.free, size, align).map(|offset| {
                    h.touched = h.touched.max(offset + size);
                    (h.heap.clone(), h.id, offset)
                })
            })
        };
        if let Some((heap, id, offset)) = existing {
            return Some((heap, Placement { heap: id, offset, size, fresh: false, pristine_charge: 0 }));
        }
        let flags = if uav {
            D3D12_HEAP_FLAGS(
                D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES.0 | D3D12_HEAP_FLAG_ALLOW_SHADER_ATOMICS.0,
            )
        } else {
            D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES
        };
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: HEAP,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: ALIGN,
            Flags: flags,
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation, our out pointer.
        if unsafe { device.CreateHeap(&desc, &mut heap) }.is_err() {
            if uav && self.uav.is_empty() {
                UAV_HEAPS.store(false, Ordering::Relaxed);
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| tracing::warn!("no shader-atomic texture heap; unordered-access textures stay committed"));
            }
            return None;
        }
        let heap = heap?;
        if uav {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::info!("unordered-access textures share heaps"));
        }
        crate::host::name(&heap, || if uav { "uav texture heap".into() } else { "texture heap".into() });
        let id = self.next;
        self.next += 1;
        let mut free = BTreeMap::new();
        free.insert(0, HEAP);
        let offset = take(&mut free, size, align)?;
        let pool = if uav { &mut self.uav } else { &mut self.heaps };
        pool.push(Heap { heap: heap.clone(), id, free, touched: offset + size, pristine_floor: None });
        Some((heap, Placement { heap: id, offset, size, fresh: false, pristine_charge: 0 }))
    }

    /// Take only a previously handed-out range of an existing heap. This
    /// avoids touching its pristine tail while the combined memory guard is
    /// shut, even though that tail belongs to an allocated D3D12 heap.
    fn allocate_existing(&mut self, size: u64, align: u64, uav: bool, pristine_limit: u64) -> Option<(ID3D12Heap, Placement)> {
        self.reap();
        let pool = if uav { &mut self.uav } else { &mut self.heaps };
        // Once a tail allowance starts, remember where each existing heap's
        // already touched bytes ended. A failed creation refunded below must
        // still pass through the allowance, not masquerade as ordinary reuse.
        if pristine_limit > 0 {
            for h in pool.iter_mut() {
                h.pristine_floor.get_or_insert(h.touched);
            }
        }
        if let Some((heap, p)) = pool.iter_mut().find_map(|h| {
            let end = h.pristine_floor.unwrap_or(h.touched);
            take_below(&mut h.free, size, align, end).map(|offset| {
                (h.heap.clone(), Placement { heap: h.id, offset, size, fresh: false, pristine_charge: 0 })
            })
        }) {
            return Some((heap, p));
        }
        if pristine_limit == 0 || self.pristine_active.checked_add(size)? > pristine_limit {
            return None;
        }
        let (heap, p) = pool.iter_mut().find_map(|h| {
            let floor = h.pristine_floor?;
            take_at_or_after(&mut h.free, size, align, floor).map(|offset| {
                h.touched = h.touched.max(offset + size);
                (h.heap.clone(), Placement { heap: h.id, offset, size, fresh: false, pristine_charge: size })
            })
        })?;
        self.pristine_active += size;
        Some((heap, p))
    }

    /// A placed-resource creation failed; return its bytes and its temporary
    /// tail charge. The fixed floor keeps a later retry correctly charged.
    fn reject(&mut self, p: Placement) {
        self.pristine_active = self.pristine_active.saturating_sub(p.pristine_charge);
        if let Some(h) = self.heaps.iter_mut().find(|h| h.id == p.heap) {
            give(&mut h.free, p.offset, p.size);
        } else if let Some(h) = self.uav.iter_mut().find(|h| h.id == p.heap) {
            give(&mut h.free, p.offset, p.size);
        }
    }
}

/// Drops empty heaps past [`SPARE`].
fn release_empty(heaps: &mut Vec<Heap>) {
    let empty = |h: &Heap| h.free.get(&0) == Some(&HEAP);
    let mut spare = 0;
    heaps.retain(|h| {
        if !empty(h) {
            return true;
        }
        spare += 1;
        spare <= SPARE
    });
}

/// Whether a texture of `desc` goes into a heap: no render-target or depth
/// use, and no bigger than a heap. Unordered access is allowed (its own
/// heaps). A texture over a quarter of a heap used to stay committed, one
/// heap of its own; it now shares, and the bytes beside it stay available.
pub(crate) fn fits(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> bool {
    const USES: i32 = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0
        | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0
        | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS.0;
    if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER || desc.Flags.0 & USES != 0 || desc.SampleDesc.Count > 1 {
        return false;
    }
    if desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0 && !UAV_HEAPS.load(Ordering::Relaxed) {
        return false;
    }
    // SAFETY: a plain query.
    let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
    if info.SizeInBytes == 0 || info.SizeInBytes == u64::MAX || info.Alignment < 4096 || !info.Alignment.is_power_of_two() {
        return false;
    }
    info.SizeInBytes <= HEAP && info.Alignment <= ALIGN
}

/// Room for a texture of `desc` in a heap ([`fits`] first): the heap and
/// the placement, which the texture gives back with [`free`].
pub(crate) fn place(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> Option<(ID3D12Heap, Placement)> {
    // SAFETY: a plain query.
    let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
    if info.SizeInBytes == 0 || info.SizeInBytes == u64::MAX || info.Alignment < 4096 || !info.Alignment.is_power_of_two() {
        return None;
    }
    let size = info.SizeInBytes.checked_next_multiple_of(info.Alignment)?;
    let uav = desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0;
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    let heaps = heaps.get_or_insert_with(Heaps::default);
    heaps.reap();
    heaps.allocate(device, size, info.Alignment, uav)
}

/// Place a sampled texture in a range of an existing heap. This deliberately
/// has no `CreateHeap` or committed-resource fallback: it is for streaming
/// while the combined DXGI usage limit is closed. Caller must check `fits`.
pub(crate) fn place_existing(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC, pristine_limit: u64) -> Option<(ID3D12Heap, Placement)> {
    #[cfg(test)]
    if TEST_NO_EXISTING.with(|value| value.get()) {
        return None;
    }
    if !fits(device, desc) {
        return None;
    }
    // SAFETY: a plain allocation-info query on a supported description.
    let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
    let size = info.SizeInBytes.checked_next_multiple_of(info.Alignment)?;
    let uav = desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0;
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    heaps.as_mut()?.allocate_existing(size, info.Alignment, uav, pristine_limit)
}

#[cfg(test)]
thread_local! {
    static TEST_NO_EXISTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn with_test_no_existing<T>(f: impl FnOnce() -> T) -> T {
    TEST_NO_EXISTING.with(|value| {
        let before = value.replace(true);
        let result = f();
        value.set(before);
        result
    })
}

/// A range [`place`] gave whose texture could not be made there.
pub(crate) fn unplace(p: Placement) {
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hs) = heaps.as_mut() {
        hs.reject(p);
    }
}

/// The texture at `p` is let go of: its range is free again once nothing
/// else holds `resource` (checked at the next placements and passes).
pub(crate) fn free(resource: ID3D12Resource, p: Placement) {
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = heaps.as_mut() {
        h.pending.push((resource, p));
        h.reap();
    }
}

/// Heaps of textures held, and their used and free bytes (after freeing
/// what nothing holds now), for the statistics line.
pub(crate) fn stats() -> (usize, u64, u64) {
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(h) = heaps.as_mut() else { return (0, 0, 0) };
    h.reap();
    let n = h.heaps.len() + h.uav.len();
    let free: u64 = h.heaps.iter().chain(&h.uav).flat_map(|h| h.free.values()).sum();
    (n, n as u64 * HEAP - free, free)
}

impl Placement {
    /// Its offset in its heap.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// Never handed out before. The caller can leave its bytes untouched.
    pub(crate) fn fresh(&self) -> bool {
        self.fresh
    }
}

/// Write-back buffer heaps. Separate from texture heaps: different memory
/// and heap flags.
static BUFFERS: Mutex<Option<BufferHeaps>> = Mutex::new(None);

#[derive(Default)]
struct BufferHeaps {
    heaps: Vec<Heap>,
    next: u64,
    pending: Vec<(ID3D12Resource, Placement)>,
    /// Where the last partial reap stopped, so live buffers at the front
    /// do not hide ones released later.
    cursor: usize,
    /// Shader-atomic heaps, once the first create has chosen.
    atomics: Option<bool>,
    /// Set when a write-back heap cannot be created. Later buffers stay
    /// committed. A placed resource that fails to map does not set this:
    /// its range is given back and the next buffer tries again.
    disabled: bool,
}

impl BufferHeaps {
    /// Frees up to `limit` ranges nothing else holds, and releases empty
    /// heaps past [`SPARE`]. One pass: a live buffer must not make the
    /// cursor wrap and walk the list until `limit` when `limit` is larger
    /// than the list. [`buffer_stats`] used to ask for every range that
    /// way, on the present thread, and the title's main thread blocked on
    /// this lock until its watchdog wrote address 0.
    fn reap(&mut self, limit: usize) {
        let mut checked = 0;
        let pass = limit.min(self.pending.len());
        while checked < pass && !self.pending.is_empty() {
            if self.cursor >= self.pending.len() {
                self.cursor = 0;
            }
            checked += 1;
            if last_holder(&self.pending[self.cursor].0) {
                let (_, p) = self.pending.swap_remove(self.cursor);
                if let Some(h) = self.heaps.iter_mut().find(|h| h.id == p.heap) {
                    give(&mut h.free, p.offset, p.size);
                }
            } else {
                self.cursor += 1;
            }
        }
        release_empty(&mut self.heaps);
    }

    /// Frees every range nothing else holds. One walk, by index.
    fn reap_all(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            if last_holder(&self.pending[i].0) {
                let (_, p) = self.pending.swap_remove(i);
                if let Some(h) = self.heaps.iter_mut().find(|h| h.id == p.heap) {
                    give(&mut h.free, p.offset, p.size);
                }
            } else {
                i += 1;
            }
        }
        if self.cursor >= self.pending.len() {
            self.cursor = 0;
        }
        release_empty(&mut self.heaps);
    }

    fn try_heap(&self, device: &ID3D12Device, atomics: bool) -> Option<ID3D12Heap> {
        let mut bits = D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS.0;
        if atomics {
            bits |= D3D12_HEAP_FLAG_ALLOW_SHADER_ATOMICS.0;
        }
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: HEAP,
            Properties: D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            },
            Alignment: ALIGN,
            Flags: D3D12_HEAP_FLAGS(bits),
        };
        let mut heap = None;
        // SAFETY: plain creation, our out pointer.
        unsafe { device.CreateHeap(&desc, &mut heap) }.ok()?;
        heap
    }

    /// A range of `size` bytes, already within a quarter of a heap.
    fn allocate(&mut self, device: &ID3D12Device, size: u64, align: u64) -> Option<(ID3D12Heap, Placement)> {
        if self.disabled || size == 0 || size > HEAP / 4 || align > ALIGN {
            return None;
        }
        self.reap(256);
        for h in &mut self.heaps {
            if let Some(offset) = take(&mut h.free, size, align) {
                let fresh = offset >= h.touched;
                h.touched = h.touched.max(offset.saturating_add(size));
                return Some((
                    h.heap.clone(),
                    Placement { heap: h.id, offset, size, fresh, pristine_charge: 0 },
                ));
            }
        }
        let (heap, atomics) = if let Some(atomics) = self.atomics {
            (self.try_heap(device, atomics)?, atomics)
        } else if let Some(heap) = self.try_heap(device, true) {
            (heap, true)
        } else if let Some(heap) = self.try_heap(device, false) {
            (heap, false)
        } else {
            self.disabled = true;
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("no write-back heap; each buffer stays committed"));
            return None;
        };
        if self.atomics.is_none() {
            self.atomics = Some(atomics);
            tracing::info!(atomics, "write-back buffers share heaps");
        }
        crate::host::name(&heap, || "write-back buffer heap".into());
        let id = self.next;
        self.next += 1;
        let mut free = BTreeMap::new();
        free.insert(0, HEAP);
        let offset = take(&mut free, size, align)?;
        self.heaps.push(Heap {
            heap: heap.clone(),
            id,
            free,
            touched: offset.saturating_add(size),
            pristine_floor: None,
        });
        Some((heap, Placement { heap: id, offset, size, fresh: true, pristine_charge: 0 }))
    }

    /// An already used free range in an existing heap. No heap creation and
    /// no committed-resource fallback are permitted at this call site.
    fn allocate_reused(&mut self, size: u64, align: u64) -> Option<(ID3D12Heap, Placement)> {
        if size == 0 || size > HEAP / 4 || align > ALIGN {
            return None;
        }
        for h in &mut self.heaps {
            if let Some(offset) = take_below(&mut h.free, size, align, h.touched) {
                return Some((
                    h.heap.clone(),
                    Placement { heap: h.id, offset, size, fresh: false, pristine_charge: 0 },
                ));
            }
        }
        None
    }
}

fn buffers() -> std::sync::MutexGuard<'static, Option<BufferHeaps>> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Room for a write-back buffer of `size` bytes (its allocation size) at
/// `align`, or `None` when it is too big or this host has no such heap.
/// The caller places the buffer and then [`note_buffer`] or [`reject_buffer`].
pub(crate) fn place_buffer(device: &ID3D12Device, size: u64, align: u64) -> Option<(ID3D12Heap, Placement)> {
    if size == 0 || size == u64::MAX || align > ALIGN {
        return None;
    }
    let size = size.next_multiple_of(ALIGN.max(align));
    let mut heaps = buffers();
    let heaps = heaps.get_or_insert_with(BufferHeaps::default);
    heaps.allocate(device, size, ALIGN.max(align))
}

/// Places a write-back buffer only in a range that was handed out before.
/// The caller must fail closed if this has no room.
pub(crate) fn place_buffer_reused(size: u64, align: u64) -> Option<(ID3D12Heap, Placement)> {
    if size == 0 || size == u64::MAX || align > ALIGN {
        return None;
    }
    let size = size.next_multiple_of(ALIGN.max(align));
    buffers().as_mut()?.allocate_reused(size, ALIGN.max(align))
}

/// Largest contiguous free span below each existing write-back heap's
/// touched frontier. This is the capacity `place_buffer_reused` can use
/// without creating another heap; aggregate free bytes can be misleading
/// when the old ranges are fragmented.
#[cfg(feature = "diagnostics")]
pub(crate) fn reused_buffer_room(size: u64) -> (u64, bool) {
    let requested = size.checked_next_multiple_of(ALIGN).unwrap_or(u64::MAX);
    let heaps = buffers();
    let Some(heaps) = heaps.as_ref() else { return (0, false) };
    let largest = heaps.heaps.iter().flat_map(|heap| {
        heap.free.iter().map(move |(&start, &len)| {
            let at = start.next_multiple_of(ALIGN);
            start.saturating_add(len).min(heap.touched).saturating_sub(at)
        })
    }).max().unwrap_or(0);
    (largest, requested <= largest)
}

/// The placed buffer could not be made or mapped. The range is free.
pub(crate) fn reject_buffer(p: Placement) {
    let mut heaps = buffers();
    let Some(h) = heaps.as_mut() else { return };
    if let Some(heap) = h.heaps.iter_mut().find(|heap| heap.id == p.heap) {
        give(&mut heap.free, p.offset, p.size);
    }
}

/// `resource` is mapped. Its range is free again once nothing else holds it.
pub(crate) fn note_buffer(resource: ID3D12Resource, p: Placement) {
    let mut heaps = buffers();
    if let Some(h) = heaps.as_mut() {
        h.pending.push((resource, p));
    }
}

/// Frees write-back ranges nothing else holds, and releases empty heaps.
pub(crate) fn reap_writeback() {
    let mut heaps = buffers();
    if let Some(h) = heaps.as_mut() {
        h.reap_all();
    }
}

/// Heaps of write-back buffers, and their used and free bytes, for the
/// statistics line. System memory, not the card's budget.
pub(crate) fn buffer_stats() -> (usize, u64, u64) {
    let mut heaps = buffers();
    let Some(h) = heaps.as_mut() else { return (0, 0, 0) };
    h.reap_all();
    let free: u64 = h.heaps.iter().flat_map(|heap| heap.free.values()).sum();
    let n = h.heaps.len();
    (n, n as u64 * HEAP - free, free)
}

/// Default-heap buffers (mesh fallback outputs, alias-copy staging) and
/// upload/readback buffers. Each committed one was a heap of its own:
/// world load had already removed the device once about 9,500 heaps were
/// live, and a mesh list's three outputs added another heap each.
#[derive(Default)]
struct RangePool {
    heaps: Vec<Heap>,
    next: u64,
    pending: Vec<(ID3D12Resource, Placement)>,
    disabled: bool,
    /// Shader-atomic default heaps, once the first create has chosen.
    /// Upload and readback pools leave this unset.
    atomics: Option<bool>,
}

impl RangePool {
    fn reap_all(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            if last_holder(&self.pending[i].0) {
                let (_, p) = self.pending.swap_remove(i);
                if let Some(h) = self.heaps.iter_mut().find(|h| h.id == p.heap) {
                    give(&mut h.free, p.offset, p.size);
                }
            } else {
                i += 1;
            }
        }
        release_empty(&mut self.heaps);
    }

    #[cfg(feature = "diagnostics")]
    fn stats(&mut self) -> (usize, u64, u64) {
        self.reap_all();
        let free: u64 = self.heaps.iter().flat_map(|h| h.free.values()).sum();
        let n = self.heaps.len();
        (n, n as u64 * HEAP - free, free)
    }

    /// A range of `size` bytes, already within a heap. `make` creates the
    /// next heap; `None` disables the pool.
    fn allocate(
        &mut self,
        device: &ID3D12Device,
        size: u64,
        align: u64,
        make: impl FnOnce(&mut Self, &ID3D12Device) -> Option<ID3D12Heap>,
    ) -> Option<(ID3D12Heap, Placement)> {
        if self.disabled || size == 0 || size > HEAP || align > ALIGN {
            return None;
        }
        self.reap_all();
        for h in &mut self.heaps {
            if let Some(offset) = take(&mut h.free, size, align) {
                return Some((h.heap.clone(), Placement { heap: h.id, offset, size, fresh: false, pristine_charge: 0 }));
            }
        }
        let heap = make(self, device)?;
        let id = self.next;
        self.next += 1;
        let mut free = BTreeMap::new();
        free.insert(0, HEAP);
        let offset = take(&mut free, size, align)?;
        self.heaps.push(Heap { heap: heap.clone(), id, free, touched: 0, pristine_floor: None });
        Some((heap, Placement { heap: id, offset, size, fresh: false, pristine_charge: 0 }))
    }

    fn reject(&mut self, p: Placement) {
        if let Some(h) = self.heaps.iter_mut().find(|h| h.id == p.heap) {
            give(&mut h.free, p.offset, p.size);
        }
    }
}

fn pool<'a>(
    slot: &'a Mutex<Option<RangePool>>,
) -> std::sync::MutexGuard<'a, Option<RangePool>> {
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

fn place_in(
    slot: &Mutex<Option<RangePool>>,
    device: &ID3D12Device,
    size: u64,
    align: u64,
    make: impl FnOnce(&mut RangePool, &ID3D12Device) -> Option<ID3D12Heap>,
) -> Option<(ID3D12Heap, Placement)> {
    if size == 0 || size == u64::MAX || align > ALIGN {
        return None;
    }
    let size = size.next_multiple_of(ALIGN.max(align));
    if size > HEAP {
        return None;
    }
    let mut guard = pool(slot);
    let heaps = guard.get_or_insert_with(RangePool::default);
    heaps.allocate(device, size, ALIGN.max(align), make)
}

static GPU_BUFFERS: Mutex<Option<RangePool>> = Mutex::new(None);
static UPLOADS: Mutex<Option<RangePool>> = Mutex::new(None);
static READBACKS: Mutex<Option<RangePool>> = Mutex::new(None);

fn gpu_heap(pool: &mut RangePool, device: &ID3D12Device) -> Option<ID3D12Heap> {
    let make = |atomics: bool| {
        let mut bits = D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS.0;
        if atomics {
            bits |= D3D12_HEAP_FLAG_ALLOW_SHADER_ATOMICS.0;
        }
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: HEAP,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: ALIGN,
            Flags: D3D12_HEAP_FLAGS(bits),
        };
        let mut heap = None;
        // SAFETY: plain creation, our out pointer.
        if unsafe { device.CreateHeap(&desc, &mut heap) }.is_err() {
            return None;
        }
        heap
    };
    let (heap, atomics) = if let Some(atomics) = pool.atomics {
        (make(atomics)?, atomics)
    } else if let Some(heap) = make(true) {
        (heap, true)
    } else if let Some(heap) = make(false) {
        (heap, false)
    } else {
        pool.disabled = true;
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("no default buffer heap; each one stays committed"));
        return None;
    };
    if pool.atomics.is_none() {
        pool.atomics = Some(atomics);
        tracing::info!(atomics, "default-heap buffers share heaps");
    }
    crate::host::name(&heap, || "default buffer heap".into());
    Some(heap)
}

fn mapped_heap(pool: &mut RangePool, device: &ID3D12Device, readback: bool) -> Option<ID3D12Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: HEAP,
        Properties: D3D12_HEAP_PROPERTIES {
            Type: if readback { D3D12_HEAP_TYPE_READBACK } else { D3D12_HEAP_TYPE_UPLOAD },
            ..Default::default()
        },
        Alignment: ALIGN,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
    };
    let mut heap = None;
    // SAFETY: plain creation, our out pointer.
    let made = unsafe { device.CreateHeap(&desc, &mut heap) };
    if made.is_err() || heap.is_none() {
        pool.disabled = true;
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(readback, "no upload heap; each mapped buffer stays committed"));
        return None;
    }
    let heap = heap?;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| tracing::info!(readback, "mapped buffers share heaps"));
    crate::host::name(&heap, || if readback { "readback heap".into() } else { "upload heap".into() });
    Some(heap)
}

/// Room for a default-heap buffer of `size` bytes (its allocation size) at
/// `align`. The caller places it and then [`note_gpu_buffer`] or
/// [`reject_gpu_buffer`].
pub(crate) fn place_gpu_buffer(
    device: &ID3D12Device,
    size: u64,
    align: u64,
) -> Option<(ID3D12Heap, Placement)> {
    place_in(&GPU_BUFFERS, device, size, align, gpu_heap)
}

/// The placed buffer could not be made. The range is free.
pub(crate) fn reject_gpu_buffer(p: Placement) {
    if let Some(h) = pool(&GPU_BUFFERS).as_mut() {
        h.reject(p);
    }
}

/// `resource` is live. Its range is free again once nothing else holds it.
pub(crate) fn note_gpu_buffer(resource: ID3D12Resource, p: Placement) {
    if let Some(h) = pool(&GPU_BUFFERS).as_mut() {
        h.pending.push((resource, p));
    }
}

/// Default-heap buffer heaps, and their used and free bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn gpu_buffer_stats() -> (usize, u64, u64) {
    pool(&GPU_BUFFERS).as_mut().map_or((0, 0, 0), RangePool::stats)
}

/// Room for an upload or (`readback`) readback buffer. Same contract as
/// [`place_gpu_buffer`].
pub(crate) fn place_mapped(
    device: &ID3D12Device,
    size: u64,
    align: u64,
    readback: bool,
) -> Option<(ID3D12Heap, Placement)> {
    let slot = if readback { &READBACKS } else { &UPLOADS };
    place_in(slot, device, size, align, |pool, device| mapped_heap(pool, device, readback))
}

/// The placed mapped buffer could not be made or mapped. The range is free.
pub(crate) fn reject_mapped(p: Placement, readback: bool) {
    let slot = if readback { &READBACKS } else { &UPLOADS };
    if let Some(h) = pool(slot).as_mut() {
        h.reject(p);
    }
}

/// `resource` is mapped. Its range is free again once nothing else holds it.
pub(crate) fn note_mapped(resource: ID3D12Resource, p: Placement, readback: bool) {
    let slot = if readback { &READBACKS } else { &UPLOADS };
    if let Some(h) = pool(slot).as_mut() {
        h.pending.push((resource, p));
    }
}

/// Upload heaps, then readback heaps: count, used bytes, free bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn mapped_stats() -> [(usize, u64, u64); 2] {
    let one = |slot: &Mutex<Option<RangePool>>| pool(slot).as_mut().map_or((0, 0, 0), RangePool::stats);
    [one(&UPLOADS), one(&READBACKS)]
}

/// A snapshot of resources still holding a pool range, and heaps that have
/// room for another allocation. This deliberately does not reap or query COM
/// ownership while the five-second diagnostics report is being written.
#[cfg(feature = "diagnostics")]
fn pressure(heaps: &[Heap], pending: &[(ID3D12Resource, Placement)]) -> (usize, u64, usize, usize) {
    let pending_bytes = pending.iter().map(|(_, p)| p.size).sum();
    let reusable = heaps.iter().filter(|h| !h.free.is_empty()).count();
    let empty = heaps.iter().filter(|h| h.free.get(&0) == Some(&HEAP)).count();
    (pending.len(), pending_bytes, reusable, empty)
}

/// Free ranges below `touched` can be reused while the combined-memory cap
/// is closed. The untouched tail is already in a D3D12 heap, but the guarded
/// allocator deliberately does not commit new texture pages there.
#[cfg(feature = "diagnostics")]
fn reusable_ranges(free: &BTreeMap<u64, u64>, touched: u64) -> (u64, u64, u64, usize) {
    let mut reusable = 0;
    let mut pristine = 0;
    let mut largest = 0;
    let mut ranges = 0;
    for (&start, &len) in free {
        let end = start.saturating_add(len);
        let below = end.min(touched).saturating_sub(start);
        let above = end.saturating_sub(start.max(touched));
        reusable += below;
        pristine += above;
        if below > 0 {
            ranges += 1;
            largest = largest.max(below);
        }
    }
    (reusable, pristine, largest, ranges)
}

/// Bytes reserved in fixed-size D3D12 heaps and bytes available for another
/// placement. The difference includes both live resources and ranges still
/// held by an in-flight command list.
#[cfg(feature = "diagnostics")]
fn occupancy(heaps: &[Heap]) -> (usize, u64, u64, u64, u64, u64, usize) {
    let (mut free, mut reusable, mut pristine, mut largest, mut ranges) = (0, 0, 0, 0, 0);
    for heap in heaps {
        let (used_again, never_used, biggest, count) = reusable_ranges(&heap.free, heap.touched);
        free += used_again + never_used;
        reusable += used_again;
        pristine += never_used;
        largest = largest.max(biggest);
        ranges += count;
    }
    (heaps.len(), heaps.len() as u64 * HEAP, free, reusable, pristine, largest, ranges)
}

/// Pool occupancy missing from the ordinary used/free byte counters. A
/// growing pending count can keep resources and their backing heaps alive
/// after their title objects have been given up.
#[cfg(feature = "diagnostics")]
pub(crate) fn log_heap_pressure() {
    let (texture, texture_occupancy, pristine_active) = {
        let guard = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map_or(((0, 0, 0, 0), ((0, 0, 0, 0, 0, 0, 0), (0, 0, 0, 0, 0, 0, 0)), 0), |h| {
            let (pending, bytes, regular_room, regular_empty) = pressure(&h.heaps, &h.pending);
            let (_, _, atomic_room, atomic_empty) = pressure(&h.uav, &[]);
            (
                (pending, bytes, regular_room + atomic_room, regular_empty + atomic_empty),
                (occupancy(&h.heaps), occupancy(&h.uav)),
                h.pristine_active,
            )
        })
    };
    let writeback = buffers().as_ref().map_or((0, 0, 0, 0), |h| pressure(&h.heaps, &h.pending));
    let one = |slot: &Mutex<Option<RangePool>>| {
        pool(slot).as_ref().map_or((0, 0, 0, 0), |h| pressure(&h.heaps, &h.pending))
    };
    let gpu = one(&GPU_BUFFERS);
    let upload = one(&UPLOADS);
    let readback = one(&READBACKS);
    tracing::info!(
        "heap pressure (pending resources, pending MB, heaps with room, fully empty heaps): \
         texture {} {:.1} {} {}; write-back {} {:.1} {} {}; \
         gpu-buffer {} {:.1} {} {}; upload {} {:.1} {} {}; readback {} {:.1} {} {}",
        texture.0, texture.1 as f64 / 1e6, texture.2, texture.3,
        writeback.0, writeback.1 as f64 / 1e6, writeback.2, writeback.3,
        gpu.0, gpu.1 as f64 / 1e6, gpu.2, gpu.3,
        upload.0, upload.1 as f64 / 1e6, upload.2, upload.3,
        readback.0, readback.1 as f64 / 1e6, readback.2, readback.3,
    );
    let (regular, atomic) = texture_occupancy;
    tracing::info!(
        regular_heaps = regular.0,
        regular_reserved_mb = regular.1 as f64 / 1e6,
        regular_placed_mb = (regular.1 - regular.2) as f64 / 1e6,
        regular_free_mb = regular.2 as f64 / 1e6,
        regular_reusable_mb = regular.3 as f64 / 1e6,
        regular_pristine_mb = regular.4 as f64 / 1e6,
        regular_largest_reusable_mb = regular.5 as f64 / 1e6,
        regular_reusable_ranges = regular.6,
        guarded_tail_active_mb = pristine_active as f64 / 1e6,
        atomic_heaps = atomic.0,
        atomic_reserved_mb = atomic.1 as f64 / 1e6,
        atomic_placed_mb = (atomic.1 - atomic.2) as f64 / 1e6,
        atomic_free_mb = atomic.2 as f64 / 1e6,
        atomic_reusable_mb = atomic.3 as f64 / 1e6,
        atomic_pristine_mb = atomic.4 as f64 / 1e6,
        atomic_largest_reusable_mb = atomic.5 as f64 / 1e6,
        atomic_reusable_ranges = atomic.6,
        "texture heap occupancy (shared heaps only; committed textures are reported separately)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "diagnostics")]
    #[test]
    fn reusable_range_report_splits_at_touched_frontier() {
        let free = BTreeMap::from([(0, 64), (128, 128), (384, 128)]);
        // The second range straddles the frontier; the third is pristine.
        let (reusable, pristine, largest, ranges) = reusable_ranges(&free, 192);
        assert_eq!((reusable, pristine, largest, ranges), (128, 192, 64, 2));
        assert_eq!(reusable + pristine, free.values().sum());
    }

    #[test]
    fn eligible_small_textures_share_four_kilobyte_ranges() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 4096,
            Width: 16,
            Height: 10,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: `desc` is fully initialized and the live device reads it
        // only for this allocation-info query.
        let info = unsafe { host.device.GetResourceAllocationInfo(0, &[desc]) };
        if info.Alignment != 4096 || info.SizeInBytes >= ALIGN {
            return; // The host will use ordinary 64 KB placement instead.
        }
        assert!(fits(&host.device, &desc));
        let mut heaps = Heaps::default();
        let (heap, first) = heaps.allocate(&host.device, info.SizeInBytes, info.Alignment, false).unwrap();
        let (_, second) = heaps.allocate(&host.device, info.SizeInBytes, info.Alignment, false).unwrap();
        assert_eq!(second.offset - first.offset, info.SizeInBytes);
        assert!(second.offset < ALIGN);
        for offset in [first.offset, second.offset] {
            let mut texture: Option<ID3D12Resource> = None;
            // SAFETY: both aligned offsets came from this live heap's
            // allocator, each reserves `desc`'s full allocation size, and
            // `texture` is a valid output slot.
            unsafe {
                host.device.CreatePlacedResource(&heap, offset, &desc,
                    D3D12_RESOURCE_STATE_COMMON, None, &mut texture)
            }.unwrap();
            assert!(texture.is_some());
        }
    }

    /// A texture's range is not handed out again while anything else holds
    /// its resource (a view's slot, a list), and is once nothing does.
    #[test]
    fn a_range_is_reused_only_once_nothing_holds_its_texture() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 9,
            Format: DXGI_FORMAT_BC1_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        assert!(fits(&host.device, &desc));
        let mut heaps = Heaps::default();
        let (heap, a) = heaps.allocate(&host.device, ALIGN, ALIGN, false).unwrap();
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: a range of the heap, our out pointer.
        unsafe { host.device.CreatePlacedResource(&heap, a.offset(), &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut texture) }.unwrap();
        let texture = texture.unwrap();
        let held = texture.clone();
        heaps.pending.push((texture, a));
        heaps.reap();
        let (_, b) = heaps.allocate(&host.device, ALIGN, ALIGN, false).unwrap();
        assert_ne!(b, a, "still held: not handed out");
        drop(held);
        heaps.reap();
        let (_, c) = heaps.allocate(&host.device, ALIGN, ALIGN, false).unwrap();
        assert_eq!(c, a, "nothing holds it: free again");
    }

    #[test]
    fn ranges_are_taken_first_fit_and_joined_when_given_back() {
        let k = 64 << 10;
        let mut free = BTreeMap::from([(0, 10 * k)]);
        let a = take(&mut free, 3 * k, k).unwrap();
        let b = take(&mut free, 2 * k, k).unwrap();
        let c = take(&mut free, 4 * k, k).unwrap();
        assert_eq!((a, b, c), (0, 3 * k, 5 * k));
        assert_eq!(take(&mut free, 2 * k, k), None, "one 64 KiB range left");
        give(&mut free, b, 2 * k);
        assert_eq!(take(&mut free, 3 * k, k), None, "two ranges of 2 and 1, not joined");
        give(&mut free, c, 4 * k);
        assert_eq!(free, BTreeMap::from([(3 * k, 7 * k)]), "joined with both neighbours");
        give(&mut free, a, 3 * k);
        assert_eq!(free, BTreeMap::from([(0, 10 * k)]));
        // An aligned range inside a free one keeps the padding before it.
        let mut free = BTreeMap::from([(k, 10 * k)]);
        assert_eq!(take(&mut free, k, 4 * k), Some(4 * k));
        assert_eq!(free, BTreeMap::from([(k, 3 * k), (5 * k, 6 * k)]));
    }

    #[test]
    fn reuse_only_buffer_placement_never_takes_new_heap_bytes() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let mut heaps = BufferHeaps::default();
        assert!(heaps.allocate_reused(ALIGN, ALIGN).is_none(), "no heap exists");
        let (_, first) = heaps.allocate(&host.device, ALIGN, ALIGN).unwrap();
        assert!(heaps.allocate_reused(ALIGN, ALIGN).is_none(), "only untouched space is free");
        give(&mut heaps.heaps[0].free, first.offset, first.size);
        let (_, reused) = heaps.allocate_reused(ALIGN, ALIGN).expect("old range fits");
        assert_eq!((reused.heap, reused.offset, reused.size), (first.heap, first.offset, first.size));
        assert!(!reused.fresh);
        assert_eq!(heaps.heaps.len(), 1, "no new heap");
        assert_eq!(heaps.heaps[0].touched, ALIGN, "no newly handed bytes");
    }

    #[test]
    fn reuse_only_texture_placement_takes_only_previously_used_space() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let mut heaps = Heaps::default();
        assert!(heaps.allocate_existing(ALIGN, ALIGN, false, 0).is_none(), "no heap exists");
        let (_, first) = heaps.allocate(&host.device, ALIGN, ALIGN, false).expect("one heap");
        assert!(heaps.allocate_existing(ALIGN, ALIGN, false, 0).is_none(), "its untouched tail is excluded");
        give(&mut heaps.heaps[0].free, first.offset, first.size);
        let (_, reused) = heaps.allocate_existing(ALIGN, ALIGN, false, 0).expect("old range fits");
        assert_eq!((reused.heap, reused.offset, reused.size), (first.heap, first.offset, first.size));
        assert!(!reused.fresh);
        assert_eq!(heaps.heaps.len(), 1, "no new heap");
        assert_eq!(heaps.heaps[0].touched, ALIGN, "no newly handed bytes");
    }

    #[test]
    fn guarded_texture_tail_stays_in_existing_heap_and_refunds_failed_creation() {
        let Some(host) = crate::host::host() else { return };
        let mut heaps = Heaps::default();
        let (_, first) = heaps.allocate(&host.device, ALIGN, ALIGN, false).expect("one heap");
        let (_, tail) = heaps.allocate_existing(ALIGN, ALIGN, false, 2 * ALIGN).expect("tail admission");
        assert_eq!(tail.offset, first.offset + first.size);
        assert_eq!(heaps.pristine_active, ALIGN);
        assert_eq!(heaps.heaps.len(), 1);
        heaps.reject(tail);
        assert_eq!(heaps.pristine_active, 0, "failed creation refunds the allowance");
        let (_, retried) = heaps.allocate_existing(ALIGN, ALIGN, false, 2 * ALIGN).expect("retry failed range");
        assert_eq!(retried.offset, tail.offset);
        assert_eq!(heaps.pristine_active, ALIGN, "a failed tail range cannot become free reuse");
        let (_, second) = heaps.allocate_existing(ALIGN, ALIGN, false, 2 * ALIGN).expect("second tail admission");
        assert_eq!(heaps.pristine_active, 2 * ALIGN);
        assert!(heaps.allocate_existing(ALIGN, ALIGN, false, 2 * ALIGN).is_none(), "the cap is exact");
        assert_eq!(heaps.heaps.len(), 1, "no new heap was made");
        heaps.reject(retried);
        heaps.reject(second);
        assert_eq!(heaps.pristine_active, 0);
    }

    /// Two buffers in one write-back heap, each with its own bytes. A
    /// buffer bigger than a quarter of a heap is not placed.
    #[test]
    fn two_write_back_buffers_share_one_heap_and_keep_their_bytes() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::core::Interface;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let mut heaps = BufferHeaps::default();
        let (heap_a, a) = heaps.allocate(&host.device, ALIGN, ALIGN).unwrap();
        let (heap_b, b) = heaps.allocate(&host.device, ALIGN, ALIGN).unwrap();
        assert_eq!(heap_a.as_raw(), heap_b.as_raw(), "one heap");
        assert_eq!(b.offset(), a.offset() + ALIGN);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: ALIGN,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let map = |offset: u64| {
            let mut buffer: Option<ID3D12Resource> = None;
            // SAFETY: a range of the heap, our out pointer.
            unsafe {
                host.device
                    .CreatePlacedResource(&heap_a, offset, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut buffer)
                    .unwrap();
            }
            let buffer = buffer.unwrap();
            let mut data = std::ptr::null_mut();
            // SAFETY: mapping a live write-back buffer.
            unsafe { buffer.Map(0, None, Some(&mut data)).unwrap() };
            (buffer, data.cast::<u8>())
        };
        let (_left, left) = map(a.offset());
        let (_right, right) = map(b.offset());
        // SAFETY: the first byte of each mapping.
        unsafe {
            left.write(0x11);
            right.write(0x22);
            assert_eq!(*left, 0x11);
            assert_eq!(*right, 0x22);
        }
        assert!(heaps.allocate(&host.device, HEAP / 4 + ALIGN, ALIGN).is_none());
    }

    /// Unordered-access textures share a heap that allows shader atomics.
    /// Render targets stay committed.
    #[test]
    fn an_unordered_texture_shares_a_shader_atomic_heap() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 64,
            Height: 64,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        assert!(fits(&host.device, &desc));
        let (heap, p) = place(&host.device, &desc).expect("a heap");
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: a range of the heap, our out pointer.
        unsafe {
            host.device
                .CreatePlacedResource(&heap, p.offset(), &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut texture)
                .expect("placed unordered-access texture");
        }
        drop(texture);
        // SAFETY: plain queries.
        let flags = unsafe { heap.GetDesc() }.Flags.0;
        // SAFETY: the device remains live through the query; this method
        // reads no caller memory and reports its removal status.
        let removed = unsafe { host.device.GetDeviceRemovedReason() };
        unplace(p);
        assert!(removed.is_ok(), "device removed: {removed:?}");
        assert_ne!(flags & D3D12_HEAP_FLAG_ALLOW_SHADER_ATOMICS.0, 0, "atomics");
        assert_eq!(flags & D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES.0, D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES.0);
        let target = D3D12_RESOURCE_DESC {
            Flags: D3D12_RESOURCE_FLAGS(
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0,
            ),
            ..desc
        };
        assert!(!fits(&host.device, &target), "a render target stays committed");
    }

    /// Live write-back buffers stay in the pending list. Counting them must
    /// walk that list once: wrapping until a huge limit is reached holds
    /// the lock on the present thread and the title's watchdog fires.
    #[test]
    fn buffer_stats_finishes_while_buffers_are_held() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let held: Vec<_> = (0..8).filter_map(|_| host.shared_buffer(ALIGN)).collect();
        assert_eq!(held.len(), 8);
        let started = std::time::Instant::now();
        let (heaps, used, _) = buffer_stats();
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "stats walked live buffers without bound");
        assert!(heaps >= 1);
        assert!(used >= ALIGN * held.len() as u64);
        drop(held);
        let _ = buffer_stats();
    }

    /// A texture bigger than a quarter of a heap shares one instead of
    /// taking a committed heap of its own.
    #[test]
    fn a_large_texture_shares_a_heap() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4096,
            Height: 2048,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        assert!(fits(&host.device, &desc), "32 MB fits in a 64 MB heap");
        let (heap, p) = place(&host.device, &desc).expect("placed");
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: a range of the heap, our out pointer.
        unsafe {
            host.device
                .CreatePlacedResource(&heap, p.offset(), &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut texture)
                .expect("placed large texture");
        }
        drop(texture);
        unplace(p);
        // SAFETY: a plain query.
        let removed = unsafe { host.device.GetDeviceRemovedReason() };
        assert!(removed.is_ok(), "device removed: {removed:?}");
    }

    /// Two default-heap buffers share one heap. The device stays up.
    #[test]
    fn two_default_buffers_share_one_heap() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::core::Interface;
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let (heap_a, a) = place_gpu_buffer(&host.device, ALIGN, ALIGN).expect("heap");
        let (heap_b, b) = place_gpu_buffer(&host.device, ALIGN, ALIGN).expect("same heap");
        assert_eq!(heap_a.as_raw(), heap_b.as_raw());
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: ALIGN,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: a range of the heap, our out pointer.
        unsafe {
            host.device
                .CreatePlacedResource(&heap_a, a.offset(), &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut made)
                .expect("placed buffer");
        }
        drop(made);
        reject_gpu_buffer(a);
        reject_gpu_buffer(b);
        // SAFETY: a plain query.
        let removed = unsafe { host.device.GetDeviceRemovedReason() };
        assert!(removed.is_ok(), "device removed: {removed:?}");
    }
}
