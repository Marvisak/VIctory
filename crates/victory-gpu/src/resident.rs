//! Title buffers in place kept in video memory (`VICTORY_IN_PLACE_VRAM=1`,
//! with `VICTORY_IN_PLACE=1`): a title buffer opened in place
//! (`crate::shadow`, a buffer placed in a heap over the title's own
//! allocation) gets a "resident", a buffer in the card's own memory, `R`,
//! that its shaders read and write instead of the title's pages across the
//! bus. `R` is kept in step with title memory by the card alone, on the
//! host queue, in the same submission:
//!
//! - **What the title wrote goes up.** At submission the 64 KiB pages of the
//!   ranges the submission brings up to date that the title wrote since
//!   they last went up (write watching: the runtime's pools are
//!   `MEM_WRITE_WATCH`) are copied from the buffer in place into `R` with
//!   `CopyBufferRegion`, in a list of ours run before the submission's
//!   lists ([`prepare`]). Nothing is copied by the CPU, staged or hashed.
//! - **What shaders wrote goes back.** Every resident has a twin `B` in the
//!   card's memory: `R` as the submission's shaders found it (an upload
//!   writes `R`, then `B` from `R`). After the submission's lists a compute
//!   pass of ours ([`MERGE`]) compares `R` with `B` per dword over the
//!   ranges declared written and stores each dword that differs into the
//!   title's memory (through the buffer in place) and into `B`: a dword
//!   changed whole as a store, one changed in part with `InterlockedAnd` and
//!   `InterlockedOr`, so bytes the CPU wrote meanwhile in the same dword
//!   stay. It runs before the submission's fence signal, so once the host
//!   has waited the title's memory holds what the shaders wrote: no
//!   snapshot, no compare on the CPU, no copy-back. Pages shaders wrote
//!   through the memory page table (`crate::memory`), which are known only
//!   after the submission ran, are merged by one more list of ours run at
//!   once ([`merge_pages`]).
//!
//! Lists of ours run in `ExecuteCommandLists` of their own before and after
//! the submission's: buffers fall back to the common state between them.
//!
//! What another copy of the same title memory writes (a buffer in place over
//! the same pages, shaders writing title memory through the page table into
//! a buffer in place) is in the title's memory and not in `R`: such pages go
//! up again before the next submission ([`written_beside`]). A write of the
//! card into the title's pages is not seen by write watching.
//!
//! shortcut: as with host copies, shaders are taken to write only the
//! ranges declared written (views and the page table); a write outside
//! them stays in `R` until a page the title wrote goes up over it.
//! shortcut: a dword the card changed in part takes two atomics; whether a
//! shader stores whole dwords only (then one store would do, much faster
//! where partial dwords are many) is not known here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::s;

use crate::host::{host, transition};

/// The unit that goes up: the console's page, the unit of the trackers.
const PAGE: u64 = crate::watch::PAGE as u64;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether title buffers in place get a resident (`VICTORY_IN_PLACE_VRAM=1`;
/// off by default, and only with `VICTORY_IN_PLACE=1`).
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if tests::FORCED.load(Ordering::Relaxed) {
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::shadow::in_place_enabled() && std::env::var_os("VICTORY_IN_PLACE_VRAM").is_some_and(|v| v == "1")
    })
}

/// The smallest title buffer in place that gets a resident
/// (`VICTORY_IN_PLACE_VRAM_MIN_MB`, 1).
fn min_bytes() -> u64 {
    static MIN: OnceLock<u64> = OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("VICTORY_IN_PLACE_VRAM_MIN_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(1) << 20
    })
}

/// The most bytes of residents, `R` and `B` together
/// (`VICTORY_IN_PLACE_VRAM_MB`; by default 60% of the card's budget for
/// this process when first asked, else 4 GiB). Each is also made only while
/// the card's memory in use stays under 80% of that budget with it.
fn most() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_IN_PLACE_VRAM_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|mb| mb << 20)
            .or_else(|| crate::host::video_memory().map(|[(_, budget), _]| (budget * 1e6 * 0.6) as u64))
            .unwrap_or(4 << 30)
    })
}

/// A title range in the card's memory, fed from the buffer in place over it.
pub(crate) struct Resident {
    /// Title memory `[start, start + len)`: the buffer in place's.
    start: u64,
    len: u64,
    /// `R`: what shaders read and write.
    pub(crate) buffer: ID3D12Resource,
    pub(crate) gpu: u64,
    /// `B`: `R`'s bytes as the submission's shaders found them.
    twin: ID3D12Resource,
    /// The buffer in place (over the title's memory) it goes up from and
    /// is merged into.
    source: ID3D12Resource,
    source_gpu: u64,
    /// Offsets of `R` to go up before the next submission's lists, in the
    /// order noted, joined where they touch the last.
    dirty: Mutex<Vec<(u64, u64)>>,
}

// SAFETY: D3D12 objects are free-threaded; the rest is behind locks.
unsafe impl Send for Resident {}
// SAFETY: as above.
unsafe impl Sync for Resident {}

impl Drop for Resident {
    fn drop(&mut self) {
        BYTES.fetch_sub(2 * self.len, Ordering::Relaxed);
    }
}

/// Every resident alive (also those only a list still holds).
static ALL: Mutex<Vec<Weak<Resident>>> = Mutex::new(Vec::new());

/// Bytes of `R` and `B` of the residents alive.
static BYTES: AtomicU64 = AtomicU64::new(0);

/// Since the last line: residents made, turned down (budget, overlap, memory
/// not watched), bytes gone up, bytes compared, dispatches, merges of pages
/// written through the page table, the pages marked by other copies' writes.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

impl Resident {
    /// Makes the resident of the title buffer in place `source` (at GPU
    /// address `source_gpu`) over title memory `[start, start + len)`, if
    /// residents are on, the buffer is big enough, no resident holds any of
    /// its memory, the memory is write-watched and the budget allows.
    pub(crate) fn make(source: &ID3D12Resource, source_gpu: u64, start: u64, len: u64) -> Option<Arc<Resident>> {
        if !on() || len < min_bytes() {
            return None;
        }
        let host = host()?;
        let refused = || {
            STATS[1].fetch_add(1, Ordering::Relaxed);
            None
        };
        if overlapping(start, start + len) || !watched(start, len) {
            return refused();
        }
        let after = BYTES.load(Ordering::Relaxed) + 2 * len;
        let room = crate::host::video_memory().is_none_or(|[(used, budget), _]| used * 1e6 + (2 * len) as f64 <= 0.8 * budget * 1e6);
        if after > most() || !room || !ready(&host.device) {
            return refused();
        }
        let buffer = video_buffer(&host.device, len)?;
        let twin = video_buffer(&host.device, len)?;
        crate::host::name(&buffer, || format!("title buffer in video memory {start:#x}+{len:#x}"));
        crate::host::name(&twin, || format!("its twin {start:#x}+{len:#x}"));
        // What the title wrote before now goes up whole below: the watch's
        // marks so far are spent.
        written(start, len);
        BYTES.fetch_add(2 * len, Ordering::Relaxed);
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        let r = Arc::new(Resident {
            start,
            len,
            buffer,
            gpu,
            twin,
            source: source.clone(),
            source_gpu,
            dirty: Mutex::new(vec![(0, len)]),
        });
        lock(&ALL).push(Arc::downgrade(&r));
        STATS[0].fetch_add(1, Ordering::Relaxed);
        Some(r)
    }

    /// At submission: the pages of title memory `[from, to)` (cut to the
    /// resident) the title wrote since they last went up are to go up before
    /// the submission's lists. If write watching cannot say, all of them.
    pub(crate) fn refresh(&self, from: u64, to: u64) {
        let (from, to) = (from.max(self.start), to.min(self.start + self.len));
        if from >= to {
            return;
        }
        let (first, last) = ((from - self.start) / PAGE, (to - self.start).div_ceil(PAGE));
        let (at, n) = (self.start + first * PAGE, ((last - first) * PAGE).min(self.len - first * PAGE));
        match written(at, n) {
            Some(pages) => {
                for (a, b) in page_runs(&pages) {
                    self.mark(a * PAGE - self.start, b * PAGE - self.start);
                }
            }
            None => self.mark(first * PAGE, last * PAGE),
        }
    }

    /// Offsets `[from, to)` of `R` go up before the next submission.
    fn mark(&self, from: u64, to: u64) {
        let (from, to) = (from.min(self.len), to.min(self.len));
        if from >= to {
            return;
        }
        let mut dirty = lock(&self.dirty);
        match dirty.last_mut() {
            Some(last) if from <= last.1 && to >= last.0 => *last = (last.0.min(from), last.1.max(to)),
            _ => dirty.push((from, to)),
        }
    }

    /// What is to go up, taken: in order, joined.
    fn take(&self) -> Vec<(u64, u64)> {
        joined(std::mem::take(&mut *lock(&self.dirty)))
    }

    /// Title memory `[from, to)` cut to the resident, as offsets of `R` on
    /// 4 bytes.
    fn offsets(&self, from: u64, to: u64) -> Option<(u64, u64)> {
        let (a, b) = (from.max(self.start) - self.start, to.min(self.start + self.len).saturating_sub(self.start));
        let (a, b) = (a & !3, b.next_multiple_of(4).min(self.len));
        (a < b).then_some((a, b))
    }
}

/// A buffer of `len` bytes in the card's memory, unordered access allowed,
/// in the common state.
fn video_buffer(device: &ID3D12Device, len: u64) -> Option<ID3D12Resource> {
    buffer(device, D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, len)
}

fn buffer(device: &ID3D12Device, heap: D3D12_HEAP_PROPERTIES, len: u64) -> Option<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: len.max(256),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    let made = unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut out) };
    made.ok().and(out)
}

/// Whether a live resident holds any of title memory `[from, to)`.
fn overlapping(from: u64, to: u64) -> bool {
    let mut all = lock(&ALL);
    all.retain(|w| w.strong_count() > 0);
    all.iter().filter_map(Weak::upgrade).any(|r| r.start < to && from < r.start + r.len)
}

/// Whether `[start, start + len)` lies in write-watched memory.
fn watched(start: u64, len: u64) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    let (mut found, mut count, mut granularity) = (0usize, 1usize, 0u32);
    // SAFETY: room for one answer; no reset, so nothing changes.
    unsafe { GetWriteWatch(0, start as *const _, len as usize, (&raw mut found).cast(), &mut count, &mut granularity) == 0 }
}

/// The 64 KiB pages (by number) of `[at, at + len)` the title wrote since
/// last asked, the marks taken; `None` if write watching cannot say.
fn written(at: u64, len: u64) -> Option<Vec<u64>> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    const KERNEL_PAGE: u64 = 4096;
    let (at, end) = (at / KERNEL_PAGE * KERNEL_PAGE, (at + len).next_multiple_of(KERNEL_PAGE));
    let mut found = vec![0usize; ((end - at) / KERNEL_PAGE) as usize];
    let (mut count, mut granularity) = (found.len(), 0u32);
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(WRITE_WATCH_FLAG_RESET, at as *const _, (end - at) as usize, found.as_mut_ptr().cast(), &mut count, &mut granularity)
    } != 0;
    if failed {
        return None;
    }
    let mut pages: Vec<u64> = found[..count].iter().map(|&a| a as u64 / PAGE).collect();
    pages.dedup();
    Some(pages)
}

/// A resident and offsets of its `R`.
type Ranges = (Arc<Resident>, Vec<(u64, u64)>);

/// Runs of consecutive page numbers (ascending) as `[first, end)`, by
/// address (`page * PAGE`).
fn page_runs(pages: &[u64]) -> Vec<(u64, u64)> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &page in pages {
        match runs.last_mut() {
            Some(last) if last.1 == page => last.1 += 1,
            Some(last) if last.1 > page => {}
            _ => runs.push((page, page + 1)),
        }
    }
    runs
}

/// Ranges in order, those that overlap or touch as one.
fn joined(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

/// After a submission ran: title memory `[from, to)` was written by the card
/// through another copy than `by` (a buffer in place, the memory page
/// table): residents over it take those pages up again before the next
/// submission.
pub(crate) fn written_beside(by: Option<&Resident>, from: u64, to: u64) {
    if !on() || from >= to {
        return;
    }
    let all: Vec<Arc<Resident>> = lock(&ALL).iter().filter_map(Weak::upgrade).collect();
    for r in all.iter().filter(|r| by.is_none_or(|b| !std::ptr::eq(&***r, b))) {
        if r.start < to && from < r.start + r.len {
            let (a, b) = (from.max(r.start) - r.start, to.min(r.start + r.len) - r.start);
            r.mark(a / PAGE * PAGE, b.next_multiple_of(PAGE));
            STATS[6].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A dword of `R` that differs from `B` goes into the title's memory, and
/// into `B`: whole if every byte differs, else only the bytes that do, by
/// two atomics, so that the other bytes of the dword stay what they are in
/// the title's memory, which the CPU may write meanwhile. A row is 64
/// dwords side by side, so a group's stores into the title's memory go over
/// the bus as runs of whole bytes (with a thread per 16 bytes, as the merge
/// was first written in the fork this comes from, 16 MB changed took 167 ms
/// to merge on an RTX 4090; 0.7 ms this way). `counts` adds the dwords that
/// differed and how many of them in part.
pub(crate) const MERGE: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t0), UAV(u0), UAV(u1), UAV(u2)"
cbuffer Range : register(b0) {
    uint from;
    uint to;
};
ByteAddressBuffer now : register(t0);
RWByteAddressBuffer twin : register(u0);
RWByteAddressBuffer title : register(u1);
RWByteAddressBuffer counts : register(u2);
groupshared uint changed;
groupshared uint partial;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 group : SV_GroupID, uint gi : SV_GroupIndex) {
    if (gi == 0u) {
        changed = 0u;
        partial = 0u;
    }
    GroupMemoryBarrierWithGroupSync();
    uint c = 0u;
    uint p = 0u;
    uint at = from + group.x * 1024u + gi * 4u;
    [unroll]
    for (uint k = 0u; k < 4u; k++) {
        uint o = at + k * 256u;
        if (o < to) {
            uint gpu = now.Load(o);
            uint was = twin.Load(o);
            uint diff = gpu ^ was;
            if (diff != 0u) {
                uint mask = ((diff & 0x000000FFu) != 0u ? 0x000000FFu : 0u)
                          | ((diff & 0x0000FF00u) != 0u ? 0x0000FF00u : 0u)
                          | ((diff & 0x00FF0000u) != 0u ? 0x00FF0000u : 0u)
                          | ((diff & 0xFF000000u) != 0u ? 0xFF000000u : 0u);
                if (mask == 0xFFFFFFFFu) {
                    title.Store(o, gpu);
                } else {
                    title.InterlockedAnd(o, gpu | ~mask);
                    title.InterlockedOr(o, gpu & mask);
                    p++;
                }
                twin.Store(o, gpu);
                c++;
            }
        }
    }
    if (c != 0u) InterlockedAdd(changed, c);
    if (p != 0u) InterlockedAdd(partial, p);
    GroupMemoryBarrierWithGroupSync();
    if (gi == 0u && changed != 0u) {
        counts.InterlockedAdd(0, changed);
        counts.InterlockedAdd(4, partial);
    }
}
"#;

/// The bytes of `R` one dispatch of the merge covers at most (32,768
/// groups of 1 KiB; a dispatch takes 65,535 groups).
const MERGE_CHUNK: u64 = 32 << 20;

/// The merge's pipeline and its counters (in the card's memory, copied
/// after each merge into memory the CPU reads).
struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    counts: ID3D12Resource,
    seen: ID3D12Resource,
    seen_at: *const u32,
}

impl Pass {
    fn new(device: &ID3D12Device) -> windows::core::Result<Pass> {
        let (root, pso) = compute(device, MERGE)?;
        let counts = video_buffer(device, 256).ok_or_else(windows::core::Error::empty)?;
        let seen = buffer(
            device,
            D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            },
            256,
        )
        .ok_or_else(windows::core::Error::empty)?;
        let mut at = std::ptr::null_mut();
        // SAFETY: a CPU-visible buffer of ours, mapped for its life.
        unsafe { seen.Map(0, None, Some(&mut at))? };
        crate::host::name(&counts, || "residents: the merge's counters".into());
        Ok(Pass { root, pso, counts, seen, seen_at: at.cast() })
    }

    /// The counters as the last merge that ran left them: dwords that
    /// differed, and those of them that differed in part (both wrap).
    fn counts(&self) -> [u32; 2] {
        // SAFETY: the mapping holds 256 bytes; the card writes them whole.
        unsafe { std::array::from_fn(|i| self.seen_at.add(i).read_volatile()) }
    }
}

/// `source`'s compute shader, its root signature inside it.
fn compute(device: &ID3D12Device, source: &str) -> windows::core::Result<(ID3D12RootSignature, ID3D12PipelineState)> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(source.as_ptr().cast(), source.len(), s!("merge"), None, None, s!("main"), s!("cs_5_1"), 0, 0, &mut code, Some(&mut errors))
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe { std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize()) };
            tracing::warn!("residents' merge: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
            ..Default::default()
        })?;
        Ok((root, pso))
    }
}

/// Records the upload of `ups` (offsets of each `R`): each range from the
/// buffer in place into `R`, then from `R` into `B` (from `R`, not from title
/// memory, so that a CPU write landing between the two copies cannot make
/// them differ). Returns the bytes.
///
/// # Safety
/// `list` is open; the residents are alive until it has run.
unsafe fn record_upload(list: &ID3D12GraphicsCommandList, ups: &[Ranges]) -> u64 {
    let mut bytes = 0;
    // SAFETY: the caller's open list; live buffers, the ranges inside them.
    unsafe {
        let to_dest: Vec<D3D12_RESOURCE_BARRIER> =
            ups.iter().map(|(r, _)| transition(&r.buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)).collect();
        list.ResourceBarrier(&to_dest);
        for (r, ranges) in ups {
            for &(a, b) in ranges {
                list.CopyBufferRegion(&r.buffer, a, &r.source, a, b - a);
                bytes += b - a;
            }
        }
        let to_source: Vec<D3D12_RESOURCE_BARRIER> =
            ups.iter().map(|(r, _)| transition(&r.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)).collect();
        list.ResourceBarrier(&to_source);
        for (r, ranges) in ups {
            for &(a, b) in ranges {
                list.CopyBufferRegion(&r.twin, a, &r.buffer, a, b - a);
            }
        }
    }
    bytes
}

/// Records the merge of `merges` (offsets of each `R`, in order, apart, on 4
/// bytes). Returns the bytes compared and the dispatches.
///
/// # Safety
/// `list` is open and runs after the lists whose shader writes it merges,
/// in an `ExecuteCommandLists` of its own; the residents are alive until it
/// has run.
unsafe fn record_merge(list: &ID3D12GraphicsCommandList, pass: &Pass, merges: &[Ranges]) -> (u64, u64) {
    let (mut bytes, mut dispatches) = (0, 0);
    // SAFETY: the caller's open list; live buffers; the merge's own root
    // signature; every offset lies in `R`, `B` and the buffer in place.
    unsafe {
        list.ResourceBarrier(&[transition(&pass.counts, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
        list.SetComputeRootSignature(&pass.root);
        list.SetPipelineState(&pass.pso);
        list.SetComputeRootUnorderedAccessView(4, pass.counts.GetGPUVirtualAddress());
        for (r, ranges) in merges {
            list.SetComputeRootShaderResourceView(1, r.gpu);
            list.SetComputeRootUnorderedAccessView(2, r.twin.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(3, r.source_gpu);
            for &(a, b) in ranges {
                let mut at = a;
                while at < b {
                    let end = (at + MERGE_CHUNK).min(b);
                    let constants = [at as u32, end as u32];
                    list.SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
                    list.Dispatch((end - at).div_ceil(1024) as u32, 1, 1);
                    dispatches += 1;
                    at = end;
                }
                bytes += b - a;
            }
        }
        list.ResourceBarrier(&[transition(&pass.counts, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        list.CopyBufferRegion(&pass.seen, 0, &pass.counts, 0, 8);
    }
    (bytes, dispatches)
}

/// A list of ours with the residents its run needs alive.
struct Spare {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    kept: Vec<Arc<Resident>>,
}

/// What the host keeps for residents: the merge, and lists to reuse (the
/// host waits for every submission, so a list is free again once
/// [`prepare`] is asked for the next one).
struct Work {
    pass: Pass,
    spare: Vec<Spare>,
    out: Vec<Spare>,
    /// The counters as the last line read them.
    read: [u32; 2],
}

// SAFETY: D3D12 objects, used under the lock.
unsafe impl Send for Work {}

static WORK: Mutex<Option<Work>> = Mutex::new(None);

/// Whether the merge could be made (at the first ask); a resident is made
/// only if it could.
fn ready(device: &ID3D12Device) -> bool {
    static READY: OnceLock<bool> = OnceLock::new();
    *READY.get_or_init(|| match Pass::new(device) {
        Ok(pass) => {
            *lock(&WORK) = Some(Work { pass, spare: Vec::new(), out: Vec::new(), read: [0; 2] });
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "no title buffers in video memory: the merge could not be made (VICTORY_IN_PLACE_VRAM=1)");
            false
        }
    })
}

impl Work {
    /// A list of ours, open, holding `kept` until it is taken again. A list
    /// that cannot be had ends the run: what went up or was written would
    /// be lost.
    fn take(&mut self, device: &ID3D12Device, kept: Vec<Arc<Resident>>) -> ID3D12GraphicsCommandList {
        let spare = match self.spare.pop() {
            Some(spare) => Ok(spare),
            // SAFETY: plain object creation.
            None => unsafe {
                (|| {
                    let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                    let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                    list.Close()?;
                    Ok(Spare { allocator, list, kept: Vec::new() })
                })()
            },
        };
        // SAFETY: the host waited for the list's last run.
        let spare = spare.and_then(|s| unsafe { s.allocator.Reset().and_then(|()| s.list.Reset(&s.allocator, None)).map(|()| s) });
        match spare {
            Ok(spare) => {
                let list = spare.list.clone();
                self.out.push(Spare { kept, ..spare });
                list
            }
            Err(e) => crate::host::fail(format_args!("no list of ours for title buffers in video memory ({e}, VICTORY_IN_PLACE_VRAM=1)")),
        }
    }

    /// The lists of the last submission are free again (the host waited for
    /// it): what they held is let go.
    fn done(&mut self) {
        for mut spare in self.out.drain(..) {
            spare.kept.clear();
            self.spare.push(spare);
        }
    }
}

/// What [`prepare`] made for a submission: lists of ours, closed, to run
/// before and after the submission's lists, each in an `ExecuteCommandLists`
/// of its own.
#[derive(Default)]
pub(crate) struct Prepared {
    pub(crate) upload: Option<ID3D12CommandList>,
    pub(crate) merge: Option<ID3D12CommandList>,
}

/// At submission, after its refreshes (under the submission lock, the last
/// submission waited for): what goes up to residents is recorded to run
/// before its lists, and the merge of `declared` (title ranges its shaders
/// may write in residents) to run after them.
pub(crate) fn prepare(declared: &[(Arc<Resident>, u64, u64)]) -> Prepared {
    let mut out = Prepared::default();
    if !on() {
        return out;
    }
    let Some(host) = host() else { return out };
    let mut work = lock(&WORK);
    let Some(work) = work.as_mut() else { return out };
    work.done();
    let live: Vec<Arc<Resident>> = {
        let mut all = lock(&ALL);
        all.retain(|w| w.strong_count() > 0);
        all.iter().filter_map(Weak::upgrade).collect()
    };
    let ups: Vec<Ranges> =
        live.iter().map(|r| (r.clone(), r.take())).filter(|(_, ranges)| !ranges.is_empty()).collect();
    let mut merges: Vec<Ranges> = Vec::new();
    for (r, from, to) in declared {
        let Some(range) = r.offsets(*from, *to) else { continue };
        match merges.iter_mut().find(|(k, _)| Arc::ptr_eq(k, r)) {
            Some((_, ranges)) => ranges.push(range),
            None => merges.push((r.clone(), vec![range])),
        }
    }
    for (_, ranges) in &mut merges {
        *ranges = joined(std::mem::take(ranges));
    }
    if !ups.is_empty() {
        let kept = ups.iter().map(|(r, _)| r.clone()).collect();
        let list = work.take(&host.device, kept);
        // SAFETY: an open list of ours; the residents are kept with it.
        let bytes = unsafe { record_upload(&list, &ups) };
        STATS[2].fetch_add(bytes, Ordering::Relaxed);
        out.upload = close(list);
    }
    if !merges.is_empty() {
        let kept = merges.iter().map(|(r, _)| r.clone()).collect();
        let list = work.take(&host.device, kept);
        // SAFETY: an open list of ours, run after the submission's lists in
        // an `ExecuteCommandLists` of its own; the residents are kept with it.
        let (bytes, dispatches) = unsafe { record_merge(&list, &work.pass, &merges) };
        STATS[3].fetch_add(bytes, Ordering::Relaxed);
        STATS[4].fetch_add(dispatches, Ordering::Relaxed);
        out.merge = close(list);
    }
    out
}

fn close(list: ID3D12GraphicsCommandList) -> Option<ID3D12CommandList> {
    // SAFETY: an open list of ours.
    match unsafe { list.Close() } {
        Ok(()) => windows::core::Interface::cast(&list).ok(),
        Err(e) => crate::host::fail(format_args!("a list of ours for title buffers in video memory did not close ({e})")),
    }
}

/// Pages of residents shaders wrote through the memory page table in the
/// submission that ran (known only from its bitmaps, after it ran):
/// merged by [`merge_pages`] before the host tells the title.
static PAGES: Mutex<Vec<(Weak<Resident>, u64)>> = Mutex::new(Vec::new());

/// After a submission ran: shaders may have written the 64 KiB page at
/// title address `page` of `r` through the memory page table.
pub(crate) fn page_written(r: &Arc<Resident>, page: u64) {
    lock(&PAGES).push((Arc::downgrade(r), page));
}

/// After a submission ran, before the title is told: the list merging the
/// pages [`page_written`] noted, closed, for the host to run at once and
/// wait for; none if there are none.
pub(crate) fn merge_pages() -> Option<ID3D12CommandList> {
    let pages = std::mem::take(&mut *lock(&PAGES));
    if pages.is_empty() {
        return None;
    }
    let host = host()?;
    let mut merges: Vec<Ranges> = Vec::new();
    for (r, page) in pages {
        let Some(r) = r.upgrade() else { continue };
        let Some(range) = r.offsets(page, page + PAGE) else { continue };
        match merges.iter_mut().find(|(k, _)| Arc::ptr_eq(k, &r)) {
            Some((_, ranges)) => ranges.push(range),
            None => merges.push((r, vec![range])),
        }
    }
    for (_, ranges) in &mut merges {
        *ranges = joined(std::mem::take(ranges));
    }
    if merges.is_empty() {
        return None;
    }
    let mut work = lock(&WORK);
    let work = work.as_mut()?;
    let kept = merges.iter().map(|(r, _)| r.clone()).collect();
    let list = work.take(&host.device, kept);
    // SAFETY: an open list of ours, run once the submission has run; the
    // residents are kept with it.
    let (bytes, dispatches) = unsafe { record_merge(&list, &work.pass, &merges) };
    STATS[3].fetch_add(bytes, Ordering::Relaxed);
    STATS[4].fetch_add(dispatches, Ordering::Relaxed);
    STATS[5].fetch_add(1, Ordering::Relaxed);
    close(list)
}

/// The residents' line, every 5 s with the frame rate's: over `frames`
/// frames since the last call (they add up between lines).
pub(crate) fn line(frames: f64) {
    static SINCE: Mutex<Option<(std::time::Instant, f64)>> = Mutex::new(None);
    if !on() {
        return;
    }
    let mut since = lock(&SINCE);
    let (at, n) = since.get_or_insert((std::time::Instant::now(), 0.0));
    *n += frames;
    if at.elapsed() < std::time::Duration::from_secs(5) || *n <= 0.0 {
        return;
    }
    let frames = *n;
    *since = Some((std::time::Instant::now(), 0.0));
    let s: Vec<u64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed)).collect();
    let (changed, partial) = {
        let mut work = lock(&WORK);
        work.as_mut().map_or((0, 0), |w| {
            let now = w.pass.counts();
            let d = [0, 1].map(|i| now[i].wrapping_sub(w.read[i]));
            w.read = now;
            (d[0], d[1])
        })
    };
    let alive = lock(&ALL).iter().filter(|w| w.strong_count() > 0).count();
    let mb = |b: u64| b as f64 / (1 << 20) as f64 / frames;
    tracing::info!(
        "title buffers in video memory (VICTORY_IN_PLACE_VRAM=1): {alive} ({:.0} MB with their twins), {} made and {} turned down; \
         per frame: {:.1} MB gone up, {:.1} MB compared by the card in {:.0} dispatches, {:.2} MB written back ({:.0}% of the dwords in part), \
         {:.1} merges of pages written through the page table, {:.0} ranges marked by writes beside them",
        BYTES.load(Ordering::Relaxed) as f64 / (1 << 20) as f64,
        s[0],
        s[1],
        mb(s[2]),
        mb(s[3]),
        s[4] as f64 / frames,
        f64::from(changed) * 4.0 / (1 << 20) as f64 / frames,
        if changed > 0 { f64::from(partial) * 100.0 / f64::from(changed) } else { 0.0 },
        s[5] as f64 / frames,
        s[6] as f64 / frames,
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Residents on for the tests that hold it ([`on`] reads the switch
    /// once).
    pub(crate) static FORCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    pub(crate) struct On;
    impl On {
        pub(crate) fn force() -> On {
            FORCED.store(true, Ordering::Relaxed);
            On
        }
    }
    impl Drop for On {
        fn drop(&mut self) {
            FORCED.store(false, Ordering::Relaxed);
        }
    }

    /// The title's own writes so far are not seen by residents (as if made
    /// while the submission ran, after its upload).
    pub(crate) fn unseen(at: u64, len: u64) {
        drop(written(at, len));
    }

    #[test]
    fn pages_go_up_in_runs() {
        assert_eq!(page_runs(&[3, 4, 5, 9, 10, 12]), vec![(3, 6), (9, 11), (12, 13)]);
        assert_eq!(page_runs(&[]), Vec::<(u64, u64)>::new());
    }

    #[test]
    fn ranges_join_where_they_touch() {
        assert_eq!(joined(vec![(8, 12), (0, 4), (4, 6), (20, 24), (10, 16)]), vec![(0, 6), (8, 16), (20, 24)]);
    }
}
