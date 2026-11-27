//! Host textures placed in heaps made ahead instead of committed one by
//! one: a new shot brings hundreds of textures, and the host spent most of
//! a frame making them (300 textures, 430 MB on the RX 6700 XT: committed
//! 255 ms; one heap 32 ms, then placed 3 ms).
//!
//! Textures without render-target, depth or unordered-access use, up to a
//! quarter of a heap, go into heaps of [`HEAP`] bytes, first fit. A
//! texture's range is free again once nothing else holds its resource
//! (heap slots keep views' resources until the running submission has run,
//! lists until they are reset), so no view or list sees its memory reused.
//! Empty heaps past [`SPARE`] are released.

use std::collections::BTreeMap;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Bytes of one heap.
const HEAP: u64 = 64 << 20;
/// Empty heaps kept for the next textures.
const SPARE: usize = 2;
/// Placement alignment of textures that are not render targets.
const ALIGN: u64 = 64 << 10;

/// Where a texture lies: heap, offset and bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    heap: u64,
    offset: u64,
    size: u64,
}

/// A heap and its free ranges (offset → bytes).
struct Heap {
    heap: ID3D12Heap,
    id: u64,
    free: BTreeMap<u64, u64>,
}

#[derive(Default)]
struct Heaps {
    heaps: Vec<Heap>,
    next: u64,
    /// Ranges whose texture may still be held elsewhere, with a holder of
    /// our own.
    pending: Vec<(ID3D12Resource, Placement)>,
    /// Ranges added since the last reap: a reap walks every pending range
    /// (an `AddRef`/`Release` pair each), so it waits for [`REAP_EVERY`]
    /// of them instead of running per texture.
    since_reap: usize,
    /// Reaps run. Test-only, and it earns its place in a production struct:
    /// [`place`] works on the global [`HEAPS`], so a test that drives that
    /// entry cannot hold a `Heaps` of its own — this count is the only way
    /// to see whether the entry reaped. Compiled out of production builds,
    /// where nothing reads it.
    #[cfg(test)]
    reaps: u64,
}

/// Ranges given back before a reap walks the pending list: bounds both the
/// walk's share of a placement and how much free space sits unreclaimed.
const REAP_EVERY: usize = 64;

// SAFETY: D3D12 heaps and resources are free-threaded; used under the lock.
unsafe impl Send for Heaps {}

static HEAPS: Mutex<Option<Heaps>> = Mutex::new(None);

/// The first range of `free` that fits `size` bytes at `align`: its
/// offset, taken out of `free`.
fn take(free: &mut BTreeMap<u64, u64>, size: u64, align: u64) -> Option<u64> {
    let (&start, &len, at) = free.iter().find_map(|(s, l)| {
        let at = s.next_multiple_of(align);
        (at + size <= s + l).then_some((s, l, at))
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
        self.since_reap = 0;
        #[cfg(test)]
        {
            self.reaps += 1;
        }
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
        let empty = |h: &Heap| h.free.get(&0) == Some(&HEAP);
        let mut spare = 0;
        self.heaps.retain(|h| {
            if !empty(h) {
                return true;
            }
            spare += 1;
            spare <= SPARE
        });
    }

    /// A range of `size` bytes in a heap that holds one now, if any does:
    /// nothing is reaped for it.
    fn take_room(&mut self, size: u64, align: u64) -> Option<(ID3D12Heap, Placement)> {
        for h in &mut self.heaps {
            if let Some(offset) = take(&mut h.free, size, align) {
                return Some((
                    h.heap.clone(),
                    Placement {
                        heap: h.id,
                        offset,
                        size,
                    },
                ));
            }
        }
        None
    }

    /// A range of `size` bytes: in a heap with room, else in a new heap.
    /// The pending ranges are reaped only when no heap holds the range, so
    /// placing into a heap does not walk them.
    fn allocate(&mut self, device: &ID3D12Device, size: u64, align: u64) -> Option<(ID3D12Heap, Placement)> {
        if let Some(room) = self.reserved(size, align) {
            return Some(room);
        }
        self.new_heap(device, size, align)
    }

    /// The range [`Heaps::allocate`] takes from a heap that has room, reaping
    /// the pending ranges once when none does: what a placement pays before
    /// it needs a new heap, and the part of it that needs no device.
    fn reserved(&mut self, size: u64, align: u64) -> Option<(ID3D12Heap, Placement)> {
        if let Some(room) = self.take_room(size, align) {
            return Some(room);
        }
        self.reap();
        self.take_room(size, align)
    }

    /// A new heap with a range of `size` bytes taken from it.
    fn new_heap(&mut self, device: &ID3D12Device, size: u64, align: u64) -> Option<(ID3D12Heap, Placement)> {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: HEAP,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: ALIGN,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation, our out pointer.
        unsafe { device.CreateHeap(&desc, &mut heap) }.ok()?;
        let heap = heap?;
        crate::host::name(&heap, || "texture heap".into());
        let id = self.next;
        self.next += 1;
        let mut free = BTreeMap::new();
        free.insert(0, HEAP);
        let offset = take(&mut free, size, align)?;
        self.heaps.push(Heap { heap: heap.clone(), id, free });
        Some((heap, Placement { heap: id, offset, size }))
    }
}

/// Whether a texture of `desc` goes into a heap: no render-target, depth or
/// unordered-access use, and at most a quarter of a heap.
pub(crate) fn fits(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> bool {
    const USES: i32 = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0
        | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0
        | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0
        | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS.0;
    if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER || desc.Flags.0 & USES != 0 || desc.SampleDesc.Count > 1 {
        return false;
    }
    // SAFETY: a plain query.
    let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
    info.SizeInBytes != u64::MAX && info.SizeInBytes <= HEAP / 4 && info.Alignment <= ALIGN
}

/// Room for a texture of `desc` in a heap ([`fits`] first): the heap and
/// the placement, which the texture gives back with [`free`].
pub(crate) fn place(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> Option<(ID3D12Heap, Placement)> {
    // SAFETY: a plain query.
    let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
    let size = info.SizeInBytes.next_multiple_of(ALIGN);
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    let heaps = heaps.get_or_insert_with(Heaps::default);
    heaps.allocate(device, size, info.Alignment.max(ALIGN))
}

/// A range [`place`] gave whose texture could not be made there.
pub(crate) fn unplace(p: Placement) {
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = heaps.as_mut().and_then(|hs| hs.heaps.iter_mut().find(|h| h.id == p.heap)) {
        give(&mut h.free, p.offset, p.size);
    }
}

/// The texture at `p` is let go of: its range is free again once nothing
/// else holds `resource`, which a reap finds out. Ranges given back wait
/// for [`REAP_EVERY`] of them instead of taking a reap each; [`free_bytes`]
/// and [`stats`] still reap every time they are asked.
pub(crate) fn free(resource: ID3D12Resource, p: Placement) {
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = heaps.as_mut() {
        h.pending.push((resource, p));
        h.since_reap += 1;
        // A reap per texture walked every pending range for each one given
        // back: wait for a batch of them.
        if h.since_reap >= REAP_EVERY {
            h.reap();
        }
    }
}

/// Bytes of heap space free for textures (after freeing what nothing
/// holds now): the card's memory they take is not in use.
pub(crate) fn free_bytes() -> u64 {
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(h) = heaps.as_mut() else { return 0 };
    h.reap();
    h.heaps.iter().flat_map(|h| h.free.values()).sum()
}

/// Heaps of textures held, and their used and free bytes (after freeing
/// what nothing holds now), for the statistics line.
pub(crate) fn stats() -> (usize, u64, u64) {
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(h) = heaps.as_mut() else { return (0, 0, 0) };
    h.reap();
    let free: u64 = h.heaps.iter().flat_map(|h| h.free.values()).sum();
    (h.heaps.len(), h.heaps.len() as u64 * HEAP - free, free)
}

impl Placement {
    /// Its offset in its heap.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (heap, a) = heaps.allocate(&host.device, ALIGN, ALIGN).unwrap();
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: a range of the heap, our out pointer.
        unsafe { host.device.CreatePlacedResource(&heap, a.offset(), &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut texture) }.unwrap();
        let texture = texture.unwrap();
        let held = texture.clone();
        heaps.pending.push((texture, a));
        heaps.reap();
        let (_, b) = heaps.allocate(&host.device, ALIGN, ALIGN).unwrap();
        assert_ne!(b, a, "still held: not handed out");
        drop(held);
        heaps.reap();
        let (_, c) = heaps.allocate(&host.device, ALIGN, ALIGN).unwrap();
        assert_eq!(c, a, "nothing holds it: free again");
    }

    /// A reap walks every pending range (an `AddRef`/`Release` pair each),
    /// so it is not what a placement with room in a heap pays: measured
    /// here against the reap-per-placement it replaces.
    #[test]
    fn a_placement_with_room_does_not_walk_the_pending_ranges() {
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
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let mut heaps = Heaps::default();
        let (heap, _) = heaps.new_heap(&host.device, ALIGN, ALIGN).unwrap();
        // 512 ranges given back whose textures are still held: the backlog a
        // new shot leaves behind (a heap has room for 1,024 of these).
        let mut textures = Vec::new();
        for _ in 0..512 {
            let (_, p) = heaps.take_room(ALIGN, ALIGN).expect("room in the heap");
            let mut texture: Option<ID3D12Resource> = None;
            // SAFETY: a range of the heap, our out pointer.
            unsafe {
                host.device.CreatePlacedResource(
                    &heap,
                    p.offset(),
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut texture,
                )
            }
            .expect("a placed texture");
            let texture = texture.unwrap();
            heaps.pending.push((texture.clone(), p));
            textures.push(texture);
        }
        assert_eq!(heaps.pending.len(), 512, "the backlog the test measures");
        // What one reap costs with that backlog, and what a placement costs
        // when it does not pay one. The timings are printed, not asserted:
        // the test is about the reap, and a wall-clock ratio flaps on a
        // loaded machine (the margin is tens of times, but still).
        let t0 = std::time::Instant::now();
        for _ in 0..100 {
            heaps.reap();
            std::hint::black_box(heaps.take_room(ALIGN, ALIGN));
        }
        let before = t0.elapsed() / 100;
        let reaps = heaps.reaps;
        let t0 = std::time::Instant::now();
        for _ in 0..100 {
            std::hint::black_box(heaps.take_room(ALIGN, ALIGN));
        }
        let after = t0.elapsed() / 100;
        assert_eq!(
            heaps.reaps, reaps,
            "placing into a heap with room reaps nothing"
        );
        // With no room left, the reap is what the next placement pays — once.
        while heaps.take_room(ALIGN, ALIGN).is_some() {}
        std::hint::black_box(heaps.allocate(&host.device, ALIGN, ALIGN));
        assert_eq!(heaps.reaps, reaps + 1, "out of room: the reap runs, once");
        eprintln!(
            "512 pending ranges: placement with a reap {before:?}, without {after:?} (one reap {reaps:?}/100 placements)"
        );
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

    /// [`Heaps::reserved`] reaps the pending ranges only when no heap has
    /// room, so a placement into a heap with room does not walk them.
    #[test]
    fn a_placement_into_a_heap_with_room_does_not_reap() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let mut heaps = Heaps::default();
        let _ = heaps.allocate(&host.device, ALIGN, ALIGN).unwrap();
        let reaps = heaps.reaps;
        let (_, a) = heaps.reserved(ALIGN, ALIGN).unwrap();
        assert_eq!(a.offset, ALIGN, "the range after the first");
        assert_eq!(heaps.reaps, reaps, "room in a heap: nothing reaped");
        heaps.heaps[0].free.clear();
        assert!(heaps.reserved(ALIGN, ALIGN).is_none(), "no room left");
        assert_eq!(heaps.reaps, reaps + 1, "no room: one reap, then a new heap");
    }

    /// Reaps run on the global [`HEAPS`], which [`place`] and its tests
    /// share.
    fn global_reaps() -> u64 {
        HEAPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, |h| h.reaps)
    }

    /// A placement into a heap with room reaps nothing, through [`place`]:
    /// the entry a texture goes through, so a reap put back in there counts
    /// (the tests above drive the helpers and would not see it).
    #[test]
    fn the_place_entry_into_a_heap_with_room_does_not_reap() {
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
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        assert!(fits(&host.device, &desc), "a texture that goes into a heap");
        // The first placement reaps, no heap holding a range yet, and makes
        // the heap the next one takes room in.
        let (_, a) = place(&host.device, &desc).expect("a placement");
        // Other tests place textures on the same global heaps, and a reap of
        // theirs can land between these two reads as well: only a count that
        // moved every time is the one `place` paid.
        for _ in 0..3 {
            let reaps = global_reaps();
            let (_, b) = place(&host.device, &desc).expect("a placement");
            assert_ne!(b, a, "a range of its own, not the first one again");
            if global_reaps() == reaps {
                return;
            }
        }
        panic!("a placement into a heap with room reaped the pending ranges");
    }
}
