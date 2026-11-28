//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory changed since the last detiling
//! ([`crate::watch`], which follows writes through every mapping of the
//! same physical pages) in the runtime's memory; elsewhere, a content hash
//! of the whole memory at every submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Textures read through descriptor tables are brought up to date, and for
//! shaders that index the heap themselves every texture with a resource
//! object the heap views (`crate::bindless`); textures the title placed in
//! memory itself are not made for the heap (see `descriptor::host`). Plane
//! 0 is uploaded (every mip and array slice), depth and stencil planes are
//! not.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;
use windows::core::Interface;

const BLOCK_BYTES: usize = 64 << 10;
const PAGE_BYTES: usize = 4096;
const MAX_BATCH: usize = 256;
type WorkingPage = windows_sys::Win32::System::ProcessStatus::PSAPI_WORKING_SET_EX_INFORMATION;

#[allow(clippy::undocumented_unsafe_blocks)]
fn query_pages(entries: &mut [WorkingPage]) -> bool {
    use windows_sys::Win32::System::{ProcessStatus::K32QueryWorkingSetEx, Threading::GetCurrentProcess};
    unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), entries.as_mut_ptr().cast(), std::mem::size_of_val(entries) as u32) != 0 }
}

#[allow(clippy::undocumented_unsafe_blocks)]
fn sample_readable(entry: &WorkingPage, asked: bool, address: usize) -> bool {
    let resident = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
    resident || crate::host::read_into(address as *const u8, &mut [0u8; 1])
}

#[allow(clippy::undocumented_unsafe_blocks)]
fn probe_blocks(addresses: &[usize], present: &mut [bool]) {
    let mut info: [WorkingPage; MAX_BATCH] = unsafe { std::mem::zeroed() };
    let entries = &mut info[..addresses.len()];
    for (entry, &address) in entries.iter_mut().zip(addresses) {
        entry.VirtualAddress = address as *mut _;
    }
    let asked = query_pages(entries);
    for ((value, entry), &address) in present.iter_mut().zip(entries).zip(addresses) {
        *value = sample_readable(entry, asked, address);
    }
}

fn batched_residency() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BATCHED_RESIDENCY").is_none_or(|v| v != "0"))
}

fn mapped_blocks(
    start: usize,
    end: usize,
    mut probe: impl FnMut(&[usize], &mut [bool]),
) -> Vec<(usize, usize)> {
    if start >= end {
        return Vec::new();
    }
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut addresses = [0usize; MAX_BATCH];
    let mut present = [false; MAX_BATCH];
    let mut block = start & !(BLOCK_BYTES - 1);
    while block < end {
        let mut count = 0;
        while count < MAX_BATCH && block < end {
            addresses[count] = block.max(start) & !(PAGE_BYTES - 1);
            count += 1;
            block = block.saturating_add(BLOCK_BYTES);
        }
        present[..count].fill(false);
        probe(&addresses[..count], &mut present[..count]);
        for i in 0..count {
            let base = addresses[i] & !(BLOCK_BYTES - 1);
            let (from, to) = (base.max(start), base.saturating_add(BLOCK_BYTES).min(end));
            if from >= to {
                continue;
            }
            if present[i] {
                match out.last_mut() {
                    Some((_, last)) if *last == from => *last = to,
                    _ => out.push((from, to)),
                }
                continue;
            }
            let mut page = from & !(PAGE_BYTES - 1);
            while page < to {
                let page_end = page.saturating_add(PAGE_BYTES).min(to);
                let mut page_ok = [false];
                probe(&[page], &mut page_ok);
                if page_ok[0] {
                    let (pf, pt) = (page.max(from), page_end);
                    match out.last_mut() {
                        Some((_, last)) if *last == pf => *last = pt,
                        _ => out.push((pf, pt)),
                    }
                }
                page = page.saturating_add(PAGE_BYTES);
            }
        }
    }
    out
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: how a staging buffer lays out the host
/// texture's subresources, and what was last detiled. The buffer itself is
/// taken for each upload from a small pool shared by all textures
/// ([`staging_buffer`]; kept for each texture's life, the buffers took its
/// size again in system memory: 2.6-2.8 GB with placed textures made on
/// read, run integ2).
pub(crate) struct Staging {
    /// A staging buffer's bytes.
    size: u64,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    memory: *const u8,
    /// The memory's pages changed since detiled (none: not the runtime's
    /// memory, hashed instead).
    tracked: Option<crate::watch::Tracked>,
    /// Per subresource, the bytes of the texture's memory it lies in (from
    /// its start, whole layout blocks); empty where the layout gives none.
    spans: Vec<(usize, usize)>,
    last: Mutex<Last>,
    /// Uploads [`record_upload`] recorded, and the subresources they
    /// copied.
    uploads: std::sync::atomic::AtomicU64,
    copied: std::sync::atomic::AtomicU64,
    refreshed: std::sync::atomic::AtomicU64,
}

/// What was last detiled: the hash of the memory (not tracked), or of each
/// subresource's bytes (tracked).
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    hashes: Vec<Option<blake3::Hash>>,
    published: bool,
    deferred: bool,
    asked: u64,
}

impl Last {
    fn defer_remap(&mut self) -> bool {
        if !self.published || self.deferred {
            return false;
        }
        self.deferred = true;
        self.asked = 0;
        true
    }

    fn discard_snapshot(&mut self) {
        self.hash = None;
        self.hashes.fill(None);
    }
}

/// Per subresource of `layout` (`mips` per array slice; a 3D texture's
/// are its mips), the bytes of the texture's memory it lies in, rounded out
/// to the layout's blocks; empty if the layout cannot say.
fn spans(layout: &victory_formats::texture::Layout, count: u32, mips: u32) -> Vec<(usize, usize)> {
    use victory_formats::texture::Dimension;
    let d = &layout.desc;
    let block = layout.alignment.max(256);
    (0..count)
        .map(|i| {
            let (mip, slice) = (i % mips, i / mips);
            let m = layout.mips.get(mip as usize)?;
            let (z0, z1) = if d.dimension == Dimension::Tex3d { (0, m.depth.max(1) - 1) } else { (slice, slice) };
            let first = layout.offset(mip, 0, 0, z0).ok()?;
            let last = layout.offset(mip, m.width.max(1) - 1, m.height.max(1) - 1, z1).ok()? + u64::from(d.element_bytes);
            let (from, to) = (first / block * block, last.div_ceil(block) * block);
            Some((from as usize, (to.min(layout.size)) as usize))
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
}

/// Staging buffers for detiles to reuse, by size: the menu movie rewrites
/// the same textures every frame, and a fresh 12.7 MB of upload buffers a
/// frame cost ~5 ms (run textures8). Uploads run only inside
/// `Host::execute`, which waits for each submission, so the buffers of one
/// submission are free from the next on.
struct Buffers {
    /// (bytes, buffer, mapping), oldest first.
    free: std::collections::VecDeque<(u64, ID3D12Resource, usize)>,
    free_bytes: u64,
    /// Taken by the submission being prepared.
    used: Vec<(u64, ID3D12Resource, usize)>,
}

// SAFETY: the mappings are persistent mappings of the upload buffers.
unsafe impl Send for Buffers {}

static BUFFERS: Mutex<Buffers> = Mutex::new(Buffers {
    free: std::collections::VecDeque::new(),
    free_bytes: 0,
    used: Vec::new(),
});

/// Free staging buffers kept at most.
const KEPT_BYTES: u64 = 64 << 20;

/// Called before a submission's uploads: the buffers of the last one are
/// free again (the oldest dropped past `KEPT_BYTES`).
pub(crate) fn recycle() {
    {
        let mut c = CHUNKS.lock().unwrap_or_else(|e| e.into_inner());
        (c.current, c.used) = (0, 0);
        c.all.truncate(CHUNKS_KEPT);
    }
    let mut b = BUFFERS.lock().unwrap_or_else(|e| e.into_inner());
    let b = &mut *b;
    for e in b.used.drain(..) {
        b.free_bytes += e.0;
        b.free.push_back(e);
    }
    while b.free_bytes > KEPT_BYTES
        && let Some((n, ..)) = b.free.pop_front()
    {
        b.free_bytes -= n;
    }
}

/// A free staging buffer of `size` bytes, or a new one.
/// Sizes are taken in powers of two (at least 64 KiB), so a buffer serves
/// textures of other sizes too: a new shot's textures came in new sizes and
/// made buffers 10 % of the submitting thread (run evict5).
fn staging_buffer(size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    let class = size.max(1 << 16).next_power_of_two();
    let taken = {
        let mut b = BUFFERS.lock().unwrap_or_else(|e| e.into_inner());
        let i = b.free.iter().position(|e| e.0 == class);
        i.and_then(|i| b.free.remove(i)).inspect(|_| b.free_bytes -= class)
    };
    // Made without the pool's lock (refreshes run on several threads).
    let e = match taken {
        Some(e) => e,
        None => {
            let (buffer, data) = host()?.upload_buffer(class)?;
            crate::host::name(&buffer, || "texture staging".into());
            (class, buffer, data as usize)
        }
    };
    let made = (e.1.clone(), e.2 as *mut u8);
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner()).used.push(e);
    Some(made)
}

/// Staging space carved from large upload buffers, front to back: most
/// textures take a part of one instead of a buffer of their own (making
/// upload buffers was 10 % of the submitting thread in a new shot, run
/// evict5). Rewound at each submission ([`recycle`]): the last one has run.
struct Chunks {
    /// (buffer, mapping), [`CHUNK`] bytes each.
    all: Vec<(ID3D12Resource, usize)>,
    /// The chunk being carved and the bytes taken from it.
    current: usize,
    used: u64,
}

// SAFETY: the mappings are persistent mappings of the upload buffers.
unsafe impl Send for Chunks {}

static CHUNKS: Mutex<Chunks> = Mutex::new(Chunks { all: Vec::new(), current: 0, used: 0 });

const CHUNK: u64 = 32 << 20;
/// Chunks kept between submissions (more are made while one needs them).
const CHUNKS_KEPT: usize = 16;

/// `size` bytes of staging space: a buffer, the offset of the space in it
/// (a multiple of 512, as texture copies need) and its mapping there.
/// Space larger than a chunk is a buffer of its own ([`staging_buffer`]).
fn staging_space(size: u64) -> Option<(ID3D12Resource, u64, *mut u8)> {
    if size > CHUNK {
        return staging_buffer(size).map(|(b, data)| (b, 0, data));
    }
    let mut c = CHUNKS.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some((buffer, data)) = c.all.get(c.current) {
            let at = c.used.next_multiple_of(512);
            if at + size <= CHUNK {
                let made = (buffer.clone(), at, (*data + at as usize) as *mut u8);
                c.used = at + size;
                return Some(made);
            }
            (c.current, c.used) = (c.current + 1, 0);
            continue;
        }
        let (buffer, data) = host()?.upload_buffer(CHUNK)?;
        crate::host::name(&buffer, || "texture staging chunk".into());
        c.all.push((buffer, data as usize));
    }
}

// SAFETY: the title memory is only read, under `last`'s lock.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

/// A host copy or resolve that `list` records writes `resource`: from the
/// submission that runs `list` on, its host texture's contents are the
/// host's, not title memory's, which stays as it was. Marked when that
/// submission is made, not when `list` is recorded: a list recorded before
/// it but run after it must not upload title memory over the host's texels
/// (submissions run in the order they are made, which need not be the
/// order their lists were recorded in), and one run before it must still
/// get its upload.
pub(crate) fn host_writes(list: &HostList, resource: This) {
    if let Some(copy) = crate::resource::host_copy(resource) {
        list.defer_host(move || copy.host_written.store(true, std::sync::atomic::Ordering::Relaxed));
    }
}

fn is_host_written(resource: This) -> bool {
    crate::resource::host_copy(resource)
        .is_some_and(|c| c.host_written.load(std::sync::atomic::Ordering::Relaxed))
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Copies [`alias_copy`] recorded since the last frame-rate line.
pub(crate) static ALIAS_COPIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Records, once per list, the copy of `from`'s host texture into `to`'s:
/// two title textures at one memory that cannot share a host texture
/// (`resource::alias_source`: block-compressed against plain formats of
/// the same bytes, or a cast the host refused), `to` (a placed view's)
/// reading what the GPU wrote through `from`. Through a buffer, each
/// subresource with its own format, as many as have the same bytes per
/// row and rows in both; `to` no longer takes its contents from title
/// memory.
pub(crate) fn alias_copy(list: &HostList, from: This, to: This) {
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    let (Some(a), Some(b), Some(host)) = (
        crate::resource::host_resource(from),
        crate::resource::host_resource(to),
        host(),
    ) else {
        return;
    };
    // An odd key: textures' own first uses are keyed by the host texture.
    if !list.first_use(to as usize | 1) {
        return;
    }
    // SAFETY: live resources.
    let (da, db) = unsafe { (a.GetDesc(), b.GetDesc()) };
    let count = |d: &D3D12_RESOURCE_DESC| {
        if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            u32::from(d.MipLevels)
        } else {
            u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize).max(1)
        }
    };
    let footprints = |d: &D3D12_RESOURCE_DESC, n: u32| {
        let mut f = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n as usize];
        let mut rows = vec![0u32; n as usize];
        let mut total = 0;
        // SAFETY: outputs sized for `n` subresources.
        unsafe {
            host.device
                .GetCopyableFootprints(d, 0, n, 0, Some(f.as_mut_ptr()), Some(rows.as_mut_ptr()), None, Some(&mut total));
        }
        (f, rows, total)
    };
    let (fa, ra, total) = footprints(&da, count(&da));
    let (fb, rb, _) = footprints(&db, count(&db));
    // Subresources in order while both have the same bytes there (mips of
    // a block-compressed texture past its last whole block do not).
    let n = (0..fa.len().min(fb.len()))
        .take_while(|&i| {
            let (x, y) = (fa[i], fb[i]);
            x.Offset == y.Offset && x.Footprint.RowPitch == y.Footprint.RowPitch && ra[i] == rb[i] && x.Footprint.Depth == y.Footprint.Depth
        })
        .count() as u32;
    if n == 0 {
        once!(?da, ?db, "an aliased texture not copied: its layouts differ on the host");
        return;
    }
    let Some(buffer) = host.default_buffer(total) else { return };
    list.keep(&buffer);
    crate::texture::host_writes(list, to);
    crate::list::ensure_state(list, from, COPY_SOURCE);
    crate::list::ensure_state(list, to, COPY_DEST);
    let place = |r: &ID3D12Resource, f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the copy does not release.
        pResource: unsafe { std::mem::transmute_copy(r) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
    };
    let sub = |r: &ID3D12Resource, i: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(r) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i },
    };
    let l = &list.list;
    // SAFETY: an open list; live resources in the states named.
    unsafe {
        for i in 0..n {
            l.CopyTextureRegion(&place(&buffer, fa[i as usize]), 0, 0, 0, &sub(&a, i), None);
        }
        l.ResourceBarrier(&[crate::host::transition(
            &buffer,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        for i in 0..n {
            l.CopyTextureRegion(&sub(&b, i), 0, 0, 0, &place(&buffer, fb[i as usize]), None);
        }
    }
    ALIAS_COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    static LOGGED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if logged.len() < 64 && !logged.contains(&(from as usize, to as usize)) {
        logged.push((from as usize, to as usize));
        tracing::info!(from = ?from, to = ?to, a = da.Format.0, b = db.Format.0, w = da.Width, h = da.Height, subresources = n, "aliased texture copied");
    }
}

/// Has `resource`'s host copy brought up to date at the submission that
/// runs `list`: its staging buffer detiled again if the memory changed, and
/// then copied into the host texture by the submission's own upload list,
/// once for all its lists (`crate::bindless::uploads`).
///
/// shortcut: a title copy into texture memory recorded in the same
/// submission shows from the next one on.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    // Once per list for each host texture, however many title textures of
    // its memory the list samples.
    if is_host_written(resource) {
        #[cfg(feature = "diagnostics")]
        {
            static SEEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);
            if SEEN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_default().insert(resource as usize) {
                let memory = crate::resource::texture_memory(resource).map(|(m, _)| m);
                // SAFETY: a live resource.
                let desc = unsafe { texture.GetDesc() };
                tracing::debug!(?resource, ?memory, ?desc, "a sampled texture keeps what the host copied into it: not uploaded from title memory");
            }
        }
        return;
    }
    if !list.first_use(texture.as_raw() as usize) {
        return;
    }
    let (Some(staging), Some(copy)) = (staging(resource, texture), crate::resource::host_copy(resource)) else {
        return;
    };
    // Each list recorded the copy before (run perf7: 149 textures uploaded
    // 3,195 times in a frame, 17 ms of GPU time), changed or not.
    list.defer_texture(staging, copy);
}

/// Records into `list` the copy of `buffer` (laid out as `staging` says)
/// into host texture `copy`. Returns the barrier that leaves it readable by
/// shaders, for the caller to record with the others after every copy: one
/// barrier per texture between the copies kept the GPU from overlapping
/// them (new shots copy hundreds of textures, run evict6).
#[must_use]
pub(crate) fn record_upload(
    list: &HostList,
    copy: &Arc<crate::resource::HostTexture>,
    staging: &Staging,
    (buffer, at, subresources): &Upload,
) -> Option<D3D12_RESOURCE_BARRIER> {
    const COPY_DEST: u32 = 0x400;
    const SHADER_RESOURCE: u32 = 0x40 | 0x80;
    if let Some(b) = list.move_texture(copy, COPY_DEST) {
        // SAFETY: an open list; the barrier names a live texture.
        unsafe { list.list.ResourceBarrier(&[b]) };
    }
    copies(list, &copy.texture, staging, (buffer, *at), subresources, String::new);
    list.keep(buffer);
    staging.uploads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    staging.copied.fetch_add(subresources.len() as u64, std::sync::atomic::Ordering::Relaxed);
    list.move_texture(copy, SHADER_RESOURCE)
}

/// The uploads [`record_upload`] recorded into `resource`'s host texture
/// (tests: counts of their own texture, whatever other tests upload
/// meanwhile).
#[cfg(test)]
pub(crate) fn uploads(resource: This) -> u64 {
    let staging = crate::resource::host_copy(resource).and_then(|c| c.staging.lock().unwrap_or_else(|e| e.into_inner()).clone());
    staging.map_or(0, |s| s.uploads.load(std::sync::atomic::Ordering::Relaxed))
}

/// The subresources those uploads copied (tests, as [`uploads`]).
#[cfg(test)]
pub(crate) fn copied(resource: This) -> u64 {
    let staging = crate::resource::host_copy(resource).and_then(|c| c.staging.lock().unwrap_or_else(|e| e.into_inner()).clone());
    staging.map_or(0, |s| s.copied.load(std::sync::atomic::Ordering::Relaxed))
}


/// Records the copy of `subresources` of `buffer` (laid out as `staging`
/// says) into `texture` (in the copy destination state); `name` names it
/// in diagnostics notes.
#[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
fn copies(
    list: &HostList,
    texture: &ID3D12Resource,
    staging: &Staging,
    (buffer, at): (&ID3D12Resource, u64),
    subresources: &[u32],
    name: impl Fn() -> String,
) {
    for &i in subresources {
        let i = i as usize;
        let mut footprint = staging.footprints[i];
        footprint.Offset += at;
        let footprint = &footprint;
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        name(),
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

/// What a refresh gives: a staging buffer, where its space starts in it,
/// and the subresources detiled into it.
pub(crate) type Upload = (ID3D12Resource, u64, Vec<u32>);

/// Runs `f(i)` for each `i` below `n` on up to a quarter of the CPUs (four
/// at most: the title's own threads need the rest), here when that is
/// fewer than two.
pub(crate) fn in_parallel(n: usize, f: impl Fn(usize) + Sync) {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get() / 4).clamp(1, 4).min(n);
    if threads < 2 {
        (0..n).for_each(f);
        return;
    }
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Relaxed);
                    if i >= n {
                        break;
                    }
                    f(i);
                }
            });
        }
    });
}

fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_NO_WAIT").is_none_or(|v| v != "0"))
}

pub(crate) static REMAP_CONFLICTS: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

struct StageConfig {
    shared: bool,
    span: Duration,
}

impl StageConfig {
    fn from_env() -> Self {
        let shared = std::env::var_os("VICTORY_TEXTURE_STAGE_PER_FRAME").is_none_or(|v| v != "0");
        let var = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok());
        let span = var("VICTORY_TEXTURE_STAGE_US")
            .map(Duration::from_micros)
            .unwrap_or_else(|| {
                Duration::from_millis(var("VICTORY_TEXTURE_STAGE_MS").unwrap_or(if shared { 6 } else { 250 }))
            });
        Self { shared, span }
    }

    fn slice(&self, frame: u64, used: (u64, Duration)) -> Option<Duration> {
        if !self.shared {
            return Some(self.span);
        }
        if used.0 > frame {
            return None;
        }
        let base = if used.0 == frame { used.1 } else { Duration::ZERO };
        let remaining = self.span.saturating_sub(base);
        (!remaining.is_zero()).then_some(remaining)
    }
}

static STAGE_CONFIG: std::sync::OnceLock<StageConfig> = std::sync::OnceLock::new();
static STAGE_SPENT: Mutex<(u64, Duration)> = Mutex::new((0, Duration::ZERO));

pub(crate) struct StageSlice {
    guard: std::sync::MutexGuard<'static, (u64, Duration)>,
    frame: u64,
    charged: Duration,
    began: Instant,
    shared: bool,
}

impl StageSlice {
    pub(crate) fn deadline(&self) -> Instant {
        self.began + self.charged.saturating_sub(self.began.elapsed())
    }
}

impl Drop for StageSlice {
    fn drop(&mut self) {
        if !self.shared {
            return;
        }
        let base = if self.guard.0 == self.frame { self.guard.1 } else { Duration::ZERO };
        self.guard.0 = self.frame;
        self.guard.1 = base.saturating_sub(self.charged).saturating_add(self.began.elapsed());
    }
}

pub(crate) fn stage_slice(frame: u64) -> Option<StageSlice> {
    let config = STAGE_CONFIG.get_or_init(StageConfig::from_env);
    let shared = config.shared && frame != 0;
    let mut used = STAGE_SPENT.lock().unwrap_or_else(|e| e.into_inner());
    let base = if used.0 == frame { used.1 } else { Duration::ZERO };
    let remaining = if shared {
        config.slice(frame, (frame, base))?
    } else {
        config.span
    };
    *used = (frame, if shared { base + remaining } else { Duration::ZERO });
    Some(StageSlice {
        guard: used,
        frame,
        charged: remaining,
        began: Instant::now(),
        shared,
    })
}

/// [`Staging::refresh`] of each of `work`, those whose memory may have
/// changed on several threads at once (reading title memory and detiling
/// cost ~6 ms of frame a MB on one thread in a new shot, run evict5).
pub(crate) fn refresh_all(
    work: &[(Arc<Staging>, Arc<crate::resource::HostTexture>)],
    deadline: Option<Instant>,
) -> Vec<Option<Upload>> {
    // Most find nothing changed (their trackers' clean bits say so): those
    // are not handed to threads.
    let changed: Vec<usize> = (0..work.len()).filter(|&i| work[i].0.maybe_changed()).collect();
    let results: Vec<Mutex<Option<Upload>>> = changed.iter().map(|_| Mutex::new(None)).collect();
    in_parallel(changed.len(), |k| {
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return;
        }
        *results[k].lock().unwrap_or_else(|e| e.into_inner()) = work[changed[k]].0.refresh();
    });
    let mut out: Vec<Option<Upload>> = vec![None; work.len()];
    for (k, r) in results.into_iter().enumerate() {
        out[changed[k]] = r.into_inner().unwrap_or_else(|e| e.into_inner());
    }
    out
}

impl Staging {
    /// Whether its memory may have changed since it was last detiled (an
    /// untracked texture always may).
    pub(crate) fn maybe_changed(&self) -> bool {
        let from = self.memory as usize;
        let to = from.saturating_add(self.layout.size as usize);
        if self.tracked.as_ref().is_none_or(|t| !t.clean()) {
            return true;
        }
        crate::watch::invalidation_generation_since(from, to, self.refreshed.load(std::sync::atomic::Ordering::Relaxed)).is_some()
    }

    /// A staging buffer with the texture memory detiled into it if it
    /// changed since the last time (and is mapped at all), and the
    /// subresources that changed (only those are detiled and to be copied).
    pub(crate) fn refresh(&self) -> Option<Upload> {
        use std::sync::atomic::Ordering::Relaxed;
        let started = Instant::now();
        let wait = !no_wait();
        let mut retries = 0;
        loop {
            let generation = crate::watch::snapshot_generation();
            self.last.lock().unwrap_or_else(|e| e.into_inner()).asked += 1;
            let (read, buffer) = self.refresh_now(generation);
            if crate::watch::snapshot_generation() == generation {
                let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
                if buffer.is_some() {
                    last.published = true;
                }
                last.deferred = false;
                drop(last);
                let preparing = &crate::host::PREPARING;
                preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
                preparing[3].fetch_add(read as u64, Relaxed);
                return buffer;
            }
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            last.discard_snapshot();
            if let Some(t) = &self.tracked {
                t.remark(self.memory as usize, (self.memory as usize).saturating_add(self.layout.size as usize));
            }
            if !wait && last.defer_remap() {
                REMAP_CONFLICTS[1].fetch_add(1, Relaxed);
                return None;
            }
            REMAP_CONFLICTS[0].fetch_add(1, Relaxed);
            drop(last);
            retries += 1;
            if retries >= 8 {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// Returns the bytes of title memory read, and the buffer it detiled
    /// into with the subresources detiled.
    fn refresh_now(&self, generation: u64) -> (usize, Option<Upload>) {
        use std::sync::atomic::Ordering::Relaxed;
        let all = || (0..self.footprints.len() as u32).collect::<Vec<_>>();
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let refreshed = self.refreshed.load(Relaxed);
        let seen = crate::watch::invalidation_generation_since(start, start + size, refreshed);
        let settle = |epoch: Option<u64>| {
            let epoch = epoch.unwrap_or(generation).max(refreshed);
            self.refreshed.store(epoch, Relaxed);
        };
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if let Some(t) = &self.tracked {
            // Most refreshes find nothing: the tracker's dense bit says so
            // without its lock (123,000 refreshes a frame, run evict3).
            if t.clean() && seen.is_none() {
                settle(seen);
                return (0, None);
            }
            let mut ranges = t.take(start, start + size);
            if ranges.is_empty() && seen.is_some() {
                t.remark(start, start + size);
                ranges = t.take(start, start + size);
            }
            if ranges.is_empty() {
                settle(seen);
                return (0, None);
            }
            // Only the subresources in the pages that changed (streamed
            // textures change page by page; a whole re-read and detile of
            // each cost up to 430 MB a frame, run integ2).
            let n = self.footprints.len();
            let span = |i: usize| self.spans.get(i).copied().unwrap_or((0, size));
            let touched: Vec<usize> = (0..n)
                .filter(|&i| {
                    let (a, b) = span(i);
                    ranges.iter().any(|&(f, e)| f - start < b && a < e - start)
                })
                .collect();
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page. Not kept: it would
            // double each texture's system memory. Only the touched spans
            // are written, and only they are hashed and detiled.
            let mut scratch = Scratch::take(size);
            let read = &mut scratch.0[..size];
            let mut bytes = 0;
            let mut done: Vec<(usize, usize)> = Vec::new();
            let batched = batched_residency();
            for &i in &touched {
                let (a, b) = span(i);
                if done.contains(&(a, b)) {
                    continue;
                }
                if batched {
                    read[a..b].fill(0);
                    for (from, to) in mapped_blocks(start + a, start + b, probe_blocks) {
                        let (lo, hi) = (from - start, to - start);
                        // SAFETY: inside the buffer of the texture's size.
                        unsafe {
                            crate::host::copy_mapped(read.as_mut_ptr().add(lo), self.memory.wrapping_add(lo), hi - lo, false);
                        }
                        bytes += hi - lo;
                    }
                } else {
                    read[a..b].fill(0);
                    for (from, to) in mapped_blocks(start + a, start + b, |addresses, present| {
                        for (address, value) in addresses.iter().zip(present.iter_mut()) {
                            *value = committed(*address as *const u8, PAGE_BYTES.min((start + b).saturating_sub(*address)));
                        }
                    }) {
                        let (lo, hi) = (from - start, to - start);
                        // SAFETY: inside the buffer of the texture's size.
                        unsafe {
                            crate::host::copy_mapped(read.as_mut_ptr().add(lo), self.memory.wrapping_add(lo), hi - lo, false);
                        }
                        bytes += hi - lo;
                    }
                }
                done.push((a, b));
            }
            // A changed page that holds only the subresource's bytes says it
            // changed. Pages it shares with other data the title writes
            // (small textures): its own bytes decide. (Hashing a movie frame
            // cost 8 ms a frame.)
            let page = crate::watch::PAGE;
            last.hashes.resize(n, None);
            let fresh: Vec<u32> = touched
                .into_iter()
                .filter(|&i| {
                    let (a, b) = span(i);
                    let (a0, b0) = (start + a, start + b);
                    if ranges.iter().any(|&(f, e)| f.max(a0).next_multiple_of(page) + page <= e.min(b0)) {
                        last.hashes[i] = None;
                        return true;
                    }
                    let hash = blake3::hash(&read[a..b]);
                    last.hashes[i].replace(hash) != Some(hash)
                })
                .map(|i| i as u32)
                .collect();
            if fresh.is_empty() {
                settle(seen);
                return (bytes, None);
            }
            let buffer = self.detile(read, &fresh);
            if buffer.is_none() {
                for &i in &fresh {
                    last.hashes[i as usize] = None;
                }
            } else {
                settle(seen);
            }
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, bytes as u64);
            return (bytes, buffer.map(|(b, at)| (b, at, fresh)));
        }
        if !committed(self.memory, size) {
            #[cfg(feature = "diagnostics")]
            tracing::debug!(
                memory = ?self.memory,
                size,
                desc = ?self.layout.desc,
                "a sampled texture's memory is not committed"
            );
            // Never uploaded: zeros, as a committed texture starts (one in
            // a heap of textures starts with whatever the range held).
            if last.hash.is_some() {
                return (0, None);
            }
            let zeros = vec![0u8; size];
            let every = all();
            let buffer = self.detile(&zeros, &every);
            last.hash = buffer.is_some().then(|| blake3::hash(&zeros));
            return (0, buffer.map(|(b, at)| (b, at, every)));
        }
        // SAFETY: committed memory of the texture's layout size.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = blake3::hash(tiled);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return (size, None);
        }
        let every = all();
        let buffer = self.detile(tiled, &every);
        last.hash = buffer.is_some().then_some(hash);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        (size, buffer.map(|(b, at)| (b, at, every)))
    }

    /// Detiles `subresources` of `tiled` (the texture's memory) into a
    /// staging buffer (`None` if none could be had or a subresource could
    /// not be read).
    fn detile(&self, tiled: &[u8], subresources: &[u32]) -> Option<(ID3D12Resource, u64)> {
        let (buffer, at, data) = staging_space(self.size)?;
        #[cfg(feature = "diagnostics")]
        // Scanned only when its message is wanted: a pass over every
        // texture read, in timing runs too.
        if tracing::enabled!(tracing::Level::DEBUG) {
            if tiled.iter().all(|&b| b == 0) {
                tracing::debug!(
                    memory = ?self.memory,
                    desc = ?self.layout.desc,
                    "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
                );
            } else {
                tracing::debug!(memory = ?self.memory, desc = ?self.layout.desc, "a sampled texture detiled");
            }
        }
        for &i in subresources {
            let (footprint, rows) = (&self.footprints[i as usize], self.rows[i as usize]);
            let (mip, slice) = (i % self.mips, i / self.mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = subresource_len(footprint.Offset, pitch, rows, f.Depth, self.size);
            // SAFETY: `subresource_len` clamps `len` to `self.size - footprint.Offset`,
            // keeping the slice strictly inside the mapped staging buffer even when
            // GetCopyableFootprints does not pad the final row of the last subresource.
            let linear = unsafe { std::slice::from_raw_parts_mut(data.add(footprint.Offset as usize), len) };
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return None;
            }
        }
        Some((buffer, at))
    }
}

/// A buffer for the title memory a refresh reads, kept by the thread for
/// its next refresh: a fresh one for each texture cost a zero-fill and a
/// page fault per 4 KiB (129,000 faults in a frame, run placed1).
struct Scratch(Vec<u8>);

thread_local! {
    static SCRATCH: std::cell::Cell<Vec<u8>> = const { std::cell::Cell::new(Vec::new()) };
}

impl Scratch {
    /// The thread's buffer, at least `size` bytes.
    fn take(size: usize) -> Self {
        let mut v = SCRATCH.take();
        if v.len() < size {
            v.resize(size, 0);
        }
        Scratch(v)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // The largest textures' buffers are not kept.
        const KEEP: usize = 32 << 20;
        if self.0.len() <= KEEP {
            SCRATCH.set(std::mem::take(&mut self.0));
        }
    }
}

/// Bytes available in the staging buffer for a subresource at `offset`.
/// D3D12's `GetCopyableFootprints` omits trailing row-pitch padding on the
/// final row of the final subresource, so `pitch * rows * depth` can exceed
/// `total - offset`.
fn subresource_len(offset: u64, pitch: usize, rows: u32, depth: u32, total: u64) -> usize {
    let padded = pitch
        .saturating_mul(rows as usize)
        .saturating_mul(depth as usize);
    let remaining = usize::try_from(total.saturating_sub(offset)).unwrap_or(usize::MAX);
    padded.min(remaining)
}

/// Whether `[at, at + len)` is committed, readable memory (streamed textures
/// may be unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let mut p = at as usize;
    let Some(end) = p.checked_add(len) else {
        return false;
    };
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        let unreadable = info.Protect == 0
            || info.Protect & (PAGE_NOACCESS | PAGE_GUARD) != 0;
        if n == 0 || info.State != MEM_COMMIT || unreadable {
            return false;
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= p {
            return false;
        }
        p = next;
    }
    true
}

/// The staging state of a title texture's host texture, made on first use
/// (one for all title textures of its memory).
pub(crate) fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let copy = crate::resource::host_copy(resource)?;
    let mut held = copy.staging.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = held.as_ref() {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    let layout_size = layout.size as usize;
    let spans = spans(&layout, count, u32::from(d.MipLevels));
    let s = Arc::new(Staging {
        size: total,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        memory,
        tracked: crate::watch::track(memory as usize, layout_size, true),
        spans,
        last: Mutex::default(),
        uploads: Default::default(),
        copied: Default::default(),
        refreshed: Default::default(),
    });
    *held = Some(s.clone());
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subresource_len_stays_inside_staging_total() {
        // A 4x4 RGBA8 texture has RowPitch=256, rows=4, last_row=16 -> total=784.
        // Padded size (1024) must be clamped to 784 so from_raw_parts_mut stays in bounds.
        assert_eq!(subresource_len(0, 256, 4, 1, 784), 784);
        assert_eq!(subresource_len(512, 256, 4, 1, 784), 272);
        assert_eq!(subresource_len(0, 256, 4, 1, 2048), 1024);
    }

    #[test]
    fn committed_rejects_overflow_and_noaccess_pages() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
            VirtualFree,
        };
        assert!(!committed(usize::MAX as *const u8, 16));
        // SAFETY: reserving and committing one private page for the test, freed below.
        let noaccess =
            unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_NOACCESS) };
        assert!(!noaccess.is_null());
        assert!(!committed(noaccess.cast::<u8>(), 4096));
        // SAFETY: freeing the allocation above.
        unsafe { VirtualFree(noaccess, 0, MEM_RELEASE) };

        // Negative control: a readable committed page still passes.
        // SAFETY: reserving and committing one private page for the test, freed below.
        let rw =
            unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!rw.is_null());
        assert!(committed(rw.cast::<u8>(), 4096));
        // SAFETY: freeing the allocation above.
        unsafe { VirtualFree(rw, 0, MEM_RELEASE) };
    }

    #[test]
    fn batched_residency_batches_256_blocks_and_keeps_holes() {
        let start = BLOCK_BYTES + 3 * PAGE_BYTES + 19;
        let end = (MAX_BATCH + 7) * BLOCK_BYTES + 71;
        let mut batches = Vec::new();
        let choose = |block: usize| block % 5 != 2;
        let ranges = mapped_blocks(start, end, |addresses, present| {
            batches.push(addresses.len());
            for (&address, value) in addresses.iter().zip(present) {
                let block = address / BLOCK_BYTES;
                *value = if addresses.len() == 1 { false } else { choose(block) };
            }
        });
        let first = start / BLOCK_BYTES;
        let last = (end - 1) / BLOCK_BYTES;
        let mut expected: Vec<(usize, usize)> = Vec::new();
        for block in (first..=last).filter(|&block| choose(block)) {
            let (from, to) = (start.max(block * BLOCK_BYTES), end.min((block + 1) * BLOCK_BYTES));
            match expected.last_mut() {
                Some((_, previous)) if *previous == from => *previous = to,
                _ => expected.push((from, to)),
            }
        }
        assert_eq!(ranges, expected);
        assert_eq!(batches[0], MAX_BATCH);
        assert!(batches.iter().skip(1).all(|&n| n == 1 || n == last - first + 1 - MAX_BATCH));
        assert!(mapped_blocks(start, start, |_, _| panic!("empty range")).is_empty());
    }

    #[test]
    #[allow(clippy::undocumented_unsafe_blocks)]
    fn probe_blocks_takes_the_safe_read_path_when_the_query_says_nothing() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        let page = unsafe { VirtualAlloc(std::ptr::null(), PAGE_BYTES, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        let hole = unsafe { VirtualAlloc(std::ptr::null(), PAGE_BYTES, MEM_RESERVE, PAGE_READWRITE) };
        assert!(!page.is_null() && !hole.is_null());
        let unknown: WorkingPage = unsafe { std::mem::zeroed() };
        assert!(sample_readable(&unknown, false, page as usize));
        assert!(!sample_readable(&unknown, false, hole as usize));
        unsafe { VirtualFree(page, 0, MEM_RELEASE) };
        unsafe { VirtualFree(hole, 0, MEM_RELEASE) };
    }

    #[test]
    fn conflicting_refresh_retains_only_a_published_image_and_retries_once() {
        let mut last = Last::default();
        assert!(!last.defer_remap(), "a first publication must wait");
        last.published = true;
        last.asked = 42;
        assert!(last.defer_remap());
        assert_eq!(last.asked, 0);
        assert!(!last.defer_remap(), "a continuous remap cannot starve the texture");
    }

    #[test]
    fn per_frame_stage_budget_slices_a_shared_six_ms_budget() {
        let shared = StageConfig {
            shared: true,
            span: Duration::from_millis(6),
        };
        assert_eq!(shared.slice(4, (3, Duration::from_millis(2))), Some(Duration::from_millis(6)));
        assert_eq!(shared.slice(4, (4, Duration::from_millis(2))), Some(Duration::from_millis(4)));
        assert_eq!(shared.slice(4, (4, Duration::from_millis(6))), None);
        assert_eq!(shared.slice(5, (4, Duration::from_millis(6))), Some(Duration::from_millis(6)));
        let per_pass = StageConfig {
            shared: false,
            span: Duration::from_millis(250),
        };
        assert_eq!(per_pass.slice(4, (4, Duration::from_secs(10))), Some(Duration::from_millis(250)));
    }

    #[test]
    fn stage_slice_budget_resets_on_frame_advance() {
        // Use high frame numbers to avoid colliding with other tests.
        let frame = 1_000_000u64;
        {
            let mut used = STAGE_SPENT.lock().unwrap_or_else(|e| e.into_inner());
            *used = (frame, Duration::from_millis(7));
        }
        // Same frame: exhausted.
        assert!(stage_slice(frame).is_none());
        // Next frame: budget resets.
        let slice = stage_slice(frame + 1).expect("budget should reset on frame advance");
        assert!(slice.charged <= Duration::from_millis(250));
        assert!(slice.charged > Duration::ZERO);
        drop(slice);
        let mut used = STAGE_SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if used.0 == frame + 1 {
            *used = (0, Duration::ZERO);
        }
    }

    #[test]
    fn mapped_blocks_preserves_committed_pages_in_partially_committed_block() {
        let start = 0usize;
        let end = BLOCK_BYTES;
        let committed_page = 2 * PAGE_BYTES;
        let ranges = mapped_blocks(start, end, |addresses, present| {
            for (&address, value) in addresses.iter().zip(present.iter_mut()) {
                let off = address % BLOCK_BYTES;
                *value = if addresses.len() == 1 { off == committed_page } else { off == 0 };
            }
        });
        assert_eq!(ranges, vec![(committed_page, committed_page + PAGE_BYTES)]);
    }

    #[test]
    fn conflicting_refresh_clears_deferred_on_success_path() {
        let mut last = Last { published: true, ..Default::default() };
        assert!(last.defer_remap());
        assert!(last.deferred);
        last.deferred = false;
        assert!(last.defer_remap());
    }

    #[test]
    fn concurrent_snapshot_generation_bump_does_not_lose_texture_updates() {
        use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
        let p = crate::watch::tests::pool(1);
        let desc = Desc {
            dimension: Dimension::Tex2d,
            width: 4,
            height: 4,
            depth: 1,
            mips: 1,
            block: 1,
            swizzle: Swizzle::LINEAR,
            element_bytes: 4,
            samples: 1,
        };
        let layout = Layout::new(&desc).expect("layout");
        let size = layout.size as usize;
        let staging = Staging {
            size: size as u64,
            footprints: vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default()],
            rows: vec![4],
            layout: layout.clone(),
            mips: 1,
            memory: p as *const u8,
            tracked: crate::watch::track(p, size, true),
            spans: spans(&layout, 1, 1),
            last: Mutex::default(),
            uploads: Default::default(),
            copied: Default::default(),
            refreshed: Default::default(),
        };
        let _ = staging.refresh_now(crate::watch::snapshot_generation());
        let t = staging.tracked.as_ref().expect("tracked");
        assert!(t.clean());
        // Mark dirty and force a generation bump as if a concurrent remap happened.
        t.remark(p, p + size);
        crate::watch::note_invalidated_for_test(p, size);
        crate::watch::bump_snapshot_generation_for_test();
        // Should not permanently lose the dirty state.
        let _ = staging.refresh();
        assert!(!t.clean() || crate::watch::invalidated(p, p + size));
        crate::watch::tests::free(p);
    }

    fn linear_staging(side: u32) -> (Staging, usize) {
        use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
        let p = crate::watch::tests::pool(1);
        let size = side as usize * side as usize * 4;
        let desc = Desc {
            dimension: Dimension::Tex2d,
            width: side,
            height: side,
            depth: 1,
            mips: 1,
            block: 1,
            swizzle: Swizzle::LINEAR,
            element_bytes: 4,
            samples: 1,
        };
        let layout = Layout::new(&desc).expect("layout");
        assert_eq!(layout.size as usize, size);
        let spans = spans(&layout, 1, 1);
        let staging = Staging {
            size: size as u64,
            footprints: vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    Width: side,
                    Height: side,
                    Depth: 1,
                    RowPitch: side * 4,
                },
            }],
            rows: vec![side],
            layout,
            mips: 1,
            memory: p as *const u8,
            tracked: crate::watch::track(p, size, true),
            spans,
            last: Mutex::default(),
            uploads: Default::default(),
            copied: Default::default(),
            refreshed: Default::default(),
        };
        (staging, p)
    }

    #[test]
    #[allow(clippy::undocumented_unsafe_blocks)]
    fn same_frame_submissions_settle_invalidations_without_redundant_uploads() {
        if crate::host::host().is_none() {
            return; // no host GPU
        }
        for (side, remap) in [(128, false), (64, true)] {
            let (staging, p) = linear_staging(side);
            let size = staging.layout.size as usize;
            let t = staging.tracked.as_ref().expect("tracked");
            assert!(staging.refresh().is_some(), "side {side}: first upload");
            assert!(t.clean(), "side {side}: clean after refresh");
            if remap {
                crate::watch::title_memory_remapped(p, size);
            } else {
                let _writing = crate::watch::writing(p, size);
                unsafe { std::ptr::write_bytes(p as *mut u8, 0x5a, size) };
            }
            assert!(staging.maybe_changed(), "side {side}: invalidation seen");
            let expected = usize::from(!remap);
            let mut uploads = 0;
            for submission in 0..4 {
                if staging.maybe_changed() && staging.refresh().is_some() {
                    uploads += 1;
                }
                assert!(uploads <= expected, "side {side}: submission {submission} uploaded redundantly");
            }
            assert_eq!(uploads, expected, "side {side}: one upload per genuine change");
            assert!(!staging.maybe_changed(), "side {side}: invalidation settled");
            crate::watch::tests::free(p);
        }
    }

    #[test]
    #[allow(clippy::undocumented_unsafe_blocks)]
    fn one_remap_uploads_once_across_six_same_frame_submissions() {
        if crate::host::host().is_none() {
            return; // no host GPU
        }
        let (staging, p) = linear_staging(128);
        let size = staging.layout.size as usize;
        assert!(staging.refresh().is_some(), "first upload");
        crate::watch::title_memory_remapped(p, size);
        // The remap opens the guarded pages: its bytes change once, with no
        // further write invalidation in this frame.
        unsafe { std::ptr::write_bytes(p as *mut u8, 0xa5, size) };
        assert!(staging.maybe_changed(), "remap seen");
        let mut uploads = 0;
        for _ in 0..6 {
            if staging.maybe_changed() && staging.refresh().is_some() {
                uploads += 1;
            }
        }
        assert_eq!(uploads, 1, "six same-frame submissions: redundant upload");
        // A present clears the frame's invalidations; a later frame advances
        // the snapshot generation without writing the texture again.
        crate::watch::invalidations_done();
        crate::watch::bump_snapshot_generation_for_test();
        for _ in 0..2 {
            if staging.maybe_changed() && staging.refresh().is_some() {
                uploads += 1;
            }
        }
        assert_eq!(uploads, 1, "next frame: extra upload");
        crate::watch::tests::free(p);
    }
}
