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

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;
use windows::core::Interface;

mod cooked;
mod profile;

#[cfg(feature = "diagnostics")]
pub(crate) fn audit_zero_sources(frame: u64) {
    cooked::audit_zero_sources(frame);
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
    /// Title mips before the first one this copy keeps ([`crate::resource`]
    /// shrinks a large streamed texture). Detile and the watched spans read
    /// that far into the title's chain.
    dropped: u32,
    title_format: u32,
    host_format: u32,
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
}

/// What was last detiled: the hash of the memory (not tracked), or of each
/// subresource's bytes (tracked).
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    hashes: Vec<Option<blake3::Hash>>,
}

/// Per subresource of `layout` (`mips` per array slice; a 3D texture's
/// are its mips), the bytes of the texture's memory it lies in, rounded out
/// to the layout's blocks; empty if the layout cannot say.
fn spans(
    layout: &victory_formats::texture::Layout,
    count: u32,
    mips: u32,
    dropped: u32,
) -> Vec<(usize, usize)> {
    use victory_formats::texture::Dimension;
    let d = &layout.desc;
    let block = layout.alignment.max(256);
    (0..count)
        .map(|i| {
            let (mip, slice) = (i % mips + dropped, i / mips);
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

/// How many retained subresource uploads from title memory were recorded for
/// this host copy. Recording does not establish GPU completion or nonzero
/// texels; it distinguishes an upload path from a texture with none yet.
#[cfg(feature = "diagnostics")]
pub(crate) fn retained_subresources_recorded(copy: &Arc<crate::resource::HostTexture>) -> u64 {
    copy.staging.lock().unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |staging| staging.copied.load(std::sync::atomic::Ordering::Relaxed))
}

/// Copies [`alias_copy`] recorded since the last frame-rate line.
pub(crate) static ALIAS_COPIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static TEST_ALIAS_COPIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn test_alias_copies() -> u64 {
    TEST_ALIAS_COPIES.with(std::cell::Cell::get)
}

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
    let (fa, ra, total_a) = footprints(&da, count(&da));
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
    // `GetCopyableFootprints` drops the last row's pitch padding. The copy
    // still reads every row at `RowPitch`; a tight buffer removes the device.
    let total = footprint_bytes(total_a, &fa[..n as usize], &ra[..n as usize]);
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
    #[cfg(test)]
    TEST_ALIAS_COPIES.with(|copies| copies.set(copies.get() + 1));
    static LOGGED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if logged.len() < 64 && !logged.contains(&(from as usize, to as usize)) {
        logged.push((from as usize, to as usize));
        tracing::info!(from = ?from, to = ?to, a = da.Format.0, b = db.Format.0, w = da.Width, h = da.Height, subresources = n, "aliased texture copied");
    }
}

/// Repack a bounded R32_UINT UAV through a row-major buffer into
/// its R9G9B9E5 read resource. Two placed textures can have equal allocation
/// sizes but the host GPU lays their texels out differently: a raw readback
/// of the writer filled 192 columns, while its R9 alias read all zero in
/// columns 128..192. The buffer copy transfers logical rows across both
/// texture layouts before the lighting shader samples the alias.
pub(crate) fn repack_shared_exp_alias(list: &HostList, resource: This) -> bool {
    let (Some(copy), Some(host)) = (crate::resource::host_copy(resource), host()) else {
        return false;
    };
    let Some(alias) = copy.shared_exp_resource() else { return false };
    let primary = &copy.texture;
    // SAFETY: both host resources stay live while `copy` is held.
    let (a, b) = unsafe { (primary.GetDesc(), alias.GetDesc()) };
    let bounded_shape = copy.shared_exp_separate()
        && crate::resource::separate_shared_exp_shape(a.Width, a.Height);
    if a.Format.0 != 42 || b.Format.0 != 67 || !bounded_shape
        || a.Width != b.Width || a.Height != b.Height || a.MipLevels != 1 || b.MipLevels != 1
        || a.SampleDesc.Count != 1 || b.SampleDesc.Count != 1
    {
        return false;
    }
    if !list.first_use(std::sync::Arc::as_ptr(&copy) as usize | 2) {
        return true;
    }
    let mut fa = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 1];
    let mut fb = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 1];
    let mut ra = [0u32; 1];
    let mut rb = [0u32; 1];
    let (mut ta, mut tb) = (0u64, 0u64);
    // SAFETY: outputs contain one subresource footprint each.
    unsafe {
        host.device.GetCopyableFootprints(&a, 0, 1, 0, Some(fa.as_mut_ptr()), Some(ra.as_mut_ptr()), None, Some(&mut ta));
        host.device.GetCopyableFootprints(&b, 0, 1, 0, Some(fb.as_mut_ptr()), Some(rb.as_mut_ptr()), None, Some(&mut tb));
    }
    if fa[0].Footprint.RowPitch != fb[0].Footprint.RowPitch || ra != rb
        || fa[0].Footprint.Width != fb[0].Footprint.Width
        || fa[0].Footprint.Height != fb[0].Footprint.Height
        || fa[0].Footprint.Depth != fb[0].Footprint.Depth
    {
        tracing::warn!(?a, ?b, "packed alias footprints differ; repack skipped");
        return false;
    }
    let bytes = footprint_bytes(ta.max(tb), &fa, &ra);
    let Some(buffer) = host.default_buffer(bytes) else { return false };
    list.keep(&buffer);
    let place = |r: &ID3D12Resource, f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: borrowed resource is held by this list until completion.
        pResource: unsafe { std::mem::transmute_copy(r) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
    };
    let sub = |r: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(r) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    };
    crate::list::ensure_state(list, resource, 0x800); // COPY_SOURCE on R32
    // SAFETY: the primary is in COPY_SOURCE and the new buffer in COPY_DEST.
    unsafe {
        list.list.CopyTextureRegion(&place(&buffer, fa[0]), 0, 0, 0, &sub(primary), None);
        list.list.ResourceBarrier(&[crate::host::transition(
            &buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
    }
    if !list.use_shared_exp_state(&copy, 0x400) { return false } // COPY_DEST on R9
    // SAFETY: the buffer now reads and the active R9 alias is in COPY_DEST.
    unsafe { list.list.CopyTextureRegion(&sub(&alias), 0, 0, 0, &place(&buffer, fb[0]), None) };
    let _ = list.use_shared_exp_state(&copy, 0x40 | 0x80); // shader resource
    static LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if count <= 8 || count.is_power_of_two() {
        tracing::info!(count, bytes, width = a.Width, height = a.Height, ?resource, "packed R32 to R9 repacked through buffer");
    }
    true
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
    staging: &Arc<Staging>,
    (buffer, at, subresources): &Upload,
) -> Option<D3D12_RESOURCE_BARRIER> {
    const COPY_DEST: u32 = 0x400;
    const SHADER_RESOURCE: u32 = 0x40 | 0x80;
    if let Some(b) = list.move_texture(copy, COPY_DEST) {
        // SAFETY: an open list; the barrier names a live texture.
        unsafe { list.list.ResourceBarrier(&[b]) };
    }
    let recorded = copies(list, &copy.texture, staging, (buffer, *at), subresources, String::new);
    #[cfg(feature = "diagnostics")]
    verify_priority_lighting_bc_gpu(list, copy, staging, subresources);
    list.keep(buffer);
    staging.uploads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    staging.copied.fetch_add(recorded as u64, std::sync::atomic::Ordering::Relaxed);
    #[cfg(feature = "diagnostics")]
    if let Some(index) = crate::resource::priority_bound_source(staging.memory as u64) {
        static SEEN: std::sync::OnceLock<Mutex<std::collections::HashSet<u64>>> = std::sync::OnceLock::new();
        if SEEN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner()).insert(staging.memory as u64) {
            // SAFETY: a live host texture named by this copy operation.
            let d = unsafe { copy.texture.GetDesc() };
            tracing::info!(index, address = format_args!("{:#x}", staging.memory as u64), requested = subresources.len(), recorded, host_width = d.Width, host_height = d.Height, host_mips = d.MipLevels, dropped_mips = staging.dropped, "bound full-mip lighting texture upload recorded");
        }
    }
    list.move_texture(copy, SHADER_RESOURCE)
}

/// Read back every uploaded mip of each bounded lighting BC source. The
/// upload list runs before its consumers, so this observes the bytes they
/// will sample, after `CopyTextureRegion` and before any later writes.
#[cfg(feature = "diagnostics")]
fn verify_priority_lighting_bc_gpu(
    list: &HostList,
    copy: &Arc<crate::resource::HostTexture>,
    staging: &Staging,
    subresources: &[u32],
) {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    let Some(dir) = DIR.get_or_init(|| {
        std::env::var_os("VICTORY_DUMP_LIGHTING_BC_DIR")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
    }).as_ref() else { return };
    if staging.dropped != 0 || !subresources.contains(&0) || staging.layout.desc.swizzle.0 != 5 {
        return;
    }
    let Some(index) = crate::resource::priority_bound_source(staging.memory as u64) else { return };
    let Some((width, bytes, mips, kind)) = [
        (2048, 8, 12, "bc1"), (1024, 16, 11, "bc3"),
        (512, 16, 10, "bc3"), (256, 8, 1, "bc1"),
    ].get(index).copied() else { return };
    let d = &staging.layout.desc;
    if d.width != width || d.height != width || d.element_bytes != bytes || d.block != 4 || d.mips != mips {
        return;
    }
    // Only one complete-chain readback per source and process. A failed
    // allocation may be retried at a later upload.
    static SEEN: Mutex<[bool; 4]> = Mutex::new([false; 4]);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen[index] { return; }
    let Some(host) = host() else { return };
    // Mips with implicit sample LOD matter here. `read_back`'s ordinary
    // target helper covers only mip 0, so pack every mip of this exact BC
    // chain into one mapped readback buffer, including its tiny tail.
    // SAFETY: the live host texture is retained by `copy`; GetDesc writes no caller memory.
    let desc = unsafe { copy.texture.GetDesc() };
    if desc.MipLevels != mips as u16 || !(0..mips).all(|mip| subresources.contains(&mip)) {
        return;
    }
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); mips as usize];
    let mut rows = vec![0u32; mips as usize];
    let mut total = 0;
    // SAFETY: the device and descriptor are live; the output arrays each have
    // `mips` elements, and all pointers remain valid for the duration of the call.
    unsafe {
        host.device.GetCopyableFootprints(
            &desc, 0, mips, 0,
            Some(footprints.as_mut_ptr()), Some(rows.as_mut_ptr()), None, Some(&mut total),
        );
    }
    let total = footprint_bytes(total, &footprints, &rows);
    let Some((buffer, mapped)) = list.readback(host, total) else { return };
    // The upload just left the host texture in COPY_DEST. Return it there
    // before `record_upload` asks the list to leave it shader-readable.
    // SAFETY: the command list, texture and readback buffer stay alive until
    // execution completes. `footprint_bytes` reserves every row pitch, and
    // each footprint/subresource index comes from GetCopyableFootprints.
    unsafe {
        list.list.ResourceBarrier(&[crate::host::transition(
            &copy.texture, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        for (mip, footprint) in footprints.iter().enumerate() {
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&copy.texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: mip as u32 },
            };
            list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        }
        list.list.ResourceBarrier(&[crate::host::transition(
            &copy.texture, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST,
        )]);
    }
    seen[index] = true;
    drop(seen);

    let dir = dir.clone();
    let mapped = mapped as usize;
    list.defer_after(move || {
        // SAFETY: the upload list's fence completed before `defer_after`,
        // and `HostList::readback` retains the mapped buffer until reset.
        for (mip, (footprint, &row_count)) in footprints.iter().zip(&rows).enumerate() {
            let edge = (width as usize >> mip).max(1);
            let row_bytes = edge.div_ceil(4) * bytes as usize;
            let row_count = row_count as usize;
            let f = footprint.Footprint;
            if (f.RowPitch as usize) < row_bytes || row_count != edge.div_ceil(4) {
                tracing::warn!(index, mip, pitch = f.RowPitch, row_count, "lighting BC GPU readback footprint differs from expected mip");
                continue;
            }
            let mut gpu = vec![0u8; row_bytes * row_count];
            for row in 0..row_count {
                // SAFETY: `readback` retains the mapped allocation until list
                // reset; this callback runs after its fence. The checked row
                // pitch and `footprint_bytes` keep this row inside that buffer.
                let src = unsafe { std::slice::from_raw_parts(
                    (mapped as *const u8).add(footprint.Offset as usize + row * f.RowPitch as usize),
                    row_bytes,
                ) };
                gpu[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(src);
            }
            let stem = format!("lighting-bc-{index}-{width}x{width}-{kind}-mip{mip}");
            let path = dir.join(format!("{stem}-gpu.bc"));
            let reference = dir.join(format!("{stem}-linear.bc"));
            match (std::fs::write(&path, &gpu), std::fs::read(&reference)) {
                (Ok(()), Ok(expected)) if expected.len() == gpu.len() => {
                    let mismatch = gpu.iter().zip(&expected).filter(|(a, b)| a != b).count();
                    tracing::info!(index, mip, bytes = gpu.len(), mismatch, path = %path.display(), "lighting BC GPU readback compared with staged bytes");
                }
                (Ok(()), Ok(expected)) => tracing::warn!(index, mip, gpu = gpu.len(), staged = expected.len(), "lighting BC GPU readback size differs"),
                (Ok(()), Err(error)) => tracing::warn!(index, mip, %error, "lighting BC staged reference unavailable"),
                (Err(error), _) => tracing::warn!(index, mip, %error, "lighting BC GPU readback write failed"),
            }
        }
    });
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
    staging: &Arc<Staging>,
    (buffer, at): (&ID3D12Resource, u64),
    subresources: &[u32],
    name: impl Fn() -> String,
) -> usize {
    let mut recorded = 0;
    for &i in subresources {
        let i = i as usize;
        let mut footprint = staging.footprints[i];
        footprint.Offset += at;
        let footprint = &footprint;
        let f = footprint.Footprint;
        #[cfg(feature = "diagnostics")]
        {
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
        // A chunk is far larger than one texture, but a buffer sized to the
        // tight footprint total is not. Reading the pitched last row past
        // that buffer removes the device, so skip the subresource.
        // SAFETY: a live upload buffer.
        let width = unsafe { buffer.GetDesc() }.Width;
        let pitched = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth.max(1));
        if footprint.Offset.saturating_add(pitched) > width {
            once!(
                offset = footprint.Offset,
                pitched,
                width,
                "texture upload skipped: the staging buffer does not cover the row pitch"
            );
            continue;
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
        #[cfg(feature = "diagnostics")]
        cooked::copy_recorded(staging, staging.memory as u64, i as u32);
        recorded += 1;
    }
    recorded
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

/// [`Staging::refresh`] of each of `work`, those whose memory may have
/// changed on several threads at once (reading title memory and detiling
/// cost ~6 ms of frame a MB on one thread in a new shot, run evict5).
pub(crate) fn refresh_all(work: &[(Arc<Staging>, Arc<crate::resource::HostTexture>)]) -> Vec<Option<Upload>> {
    // Most find nothing changed (their trackers' clean bits say so): those
    // are not handed to threads.
    let changed: Vec<usize> = (0..work.len()).filter(|&i| work[i].0.maybe_changed()).collect();
    let results: Vec<Mutex<Option<Upload>>> = changed.iter().map(|_| Mutex::new(None)).collect();
    in_parallel(changed.len(), |k| {
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
        self.tracked.as_ref().is_none_or(|t| !t.clean())
    }

    /// A staging buffer with the texture memory detiled into it if it
    /// changed since the last time (and is mapped at all), and the
    /// subresources that changed (only those are detiled and to be copied).
    pub(crate) fn refresh(&self) -> Option<Upload> {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let (read, buffer) = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
        buffer
    }

    /// Returns the bytes of title memory read, and the buffer it detiled
    /// into with the subresources detiled.
    fn refresh_now(&self) -> (usize, Option<Upload>) {
        let all = || (0..self.footprints.len() as u32).collect::<Vec<_>>();
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        if let Some(t) = &self.tracked {
            // Most refreshes find nothing: the tracker's dense bit says so
            // without its lock (123,000 refreshes a frame, run evict3).
            if t.clean() {
                return (0, None);
            }
            let ranges = t.take(start, start + size);
            if ranges.is_empty() {
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
            for &i in &touched {
                let (a, b) = span(i);
                if !done.contains(&(a, b)) {
                    // SAFETY: inside the buffer of the texture's size.
                    crate::host::copy_mapped(unsafe { read.as_mut_ptr().add(a) }, self.memory.wrapping_add(a), b - a, false);
                    (bytes, _) = (bytes + b - a, done.push((a, b)));
                }
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
                return (bytes, None);
            }
            let buffer = self.detile(read, &fresh);
            if buffer.is_none() {
                for &i in &fresh {
                    last.hashes[i as usize] = None;
                }
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
        #[cfg(feature = "diagnostics")]
        cooked::clear_pending(self as *const Staging as usize);
        let Some((buffer, at, data)) = staging_space(self.size) else {
            #[cfg(feature = "diagnostics")]
            self.note_priority_detile(tiled, subresources, false);
            return None;
        };
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
        let mut succeeded = true;
        for &i in subresources {
            let (footprint, rows) = (&self.footprints[i as usize], self.rows[i as usize]);
            let (mip, slice) = (i % self.mips + self.dropped, i / self.mips);
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
                succeeded = false;
                break;
            }
            #[cfg(feature = "diagnostics")]
            cooked::trace(
                self as *const Staging as usize, i,
                self.memory as u64,
                self.layout.desc.width, self.layout.desc.height, self.layout.desc.mips,
                self.title_format, self.dropped, self.mips, self.host_format,
                mip, slice, f.Width, f.Height, self.layout.desc.element_bytes,
                pitch, rows, linear,
            );
            if !profile::replace(
                self.memory as u64,
                self.layout.desc.width, self.layout.desc.height, self.layout.desc.mips,
                self.title_format, self.dropped, self.mips, self.host_format, mip,
                f.Width, f.Height, pitch, rows, linear,
            ) {
                cooked::replace(
                    cooked::ReplaceSource {
                        address: self.memory as u64,
                        width: self.layout.desc.width,
                        height: self.layout.desc.height,
                        mips: self.layout.desc.mips,
                        format: self.title_format,
                    },
                    cooked::ReplaceMip {
                        dropped: self.dropped,
                        host_mips: self.mips,
                        host_format: self.host_format,
                        source_mip: mip,
                        width: f.Width,
                        height: f.Height,
                        pitch,
                        rows,
                    },
                    linear,
                );
            }
        }
        #[cfg(feature = "diagnostics")]
        if succeeded {
            self.dump_priority_lighting_bc(tiled, data, subresources);
        }
        #[cfg(feature = "diagnostics")]
        self.note_priority_detile(tiled, subresources, succeeded);
        succeeded.then_some((buffer, at))
    }

    /// One local, opt-in pair of raw BC mip-0 dumps per selected lighting
    /// source: original console tile order and the bytes staged for upload.
    /// Lower mips are also saved in linear order. The exact four shapes keep
    /// output bounded. The readback beside `record_upload` verifies each mip.
    #[cfg(feature = "diagnostics")]
    fn dump_priority_lighting_bc(&self, tiled: &[u8], data: *mut u8, subresources: &[u32]) {
        use std::path::PathBuf;
        use std::sync::OnceLock;

        static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
        let Some(dir) = DIR
            .get_or_init(|| {
                std::env::var_os("VICTORY_DUMP_LIGHTING_BC_DIR")
                    .map(PathBuf::from)
                    .filter(|path| !path.as_os_str().is_empty())
            })
            .as_ref()
        else {
            return;
        };
        if !subresources.contains(&0) || self.dropped != 0 || self.layout.desc.swizzle.0 != 5 {
            return;
        }
        let Some(index) = crate::resource::priority_bound_source(self.memory as u64) else {
            return;
        };
        let d = &self.layout.desc;
        let Some((width, bytes, mips)) = [(2048, 8, 12), (1024, 16, 11), (512, 16, 10), (256, 8, 1)].get(index).copied() else {
            return;
        };
        if d.width != width || d.height != width || d.element_bytes != bytes || d.block != 4 || d.mips != mips {
            return;
        }
        let row_bytes = width as usize / 4 * bytes as usize;
        let rows = width as usize / 4;
        let Some(mip_bytes) = row_bytes.checked_mul(rows).filter(|&n| n <= 2 << 20) else {
            return;
        };
        let Some(mip) = self.layout.mips.first() else { return };
        let Some(source) = tiled.get(mip.offset as usize..mip.offset as usize + mip_bytes) else {
            tracing::warn!(index, "lighting BC source mip 0 is outside the tiled snapshot");
            return;
        };
        let Some(footprint) = self.footprints.first() else { return };
        let pitch = footprint.Footprint.RowPitch as usize;
        let Some(end) = footprint.Offset.checked_add(((rows - 1) * pitch + row_bytes) as u64) else { return };
        if pitch < row_bytes || end > self.size {
            tracing::warn!(index, pitch, row_bytes, "lighting BC mip 0 is outside the staging footprint");
            return;
        }

        static WRITTEN: Mutex<[bool; 4]> = Mutex::new([false; 4]);
        let mut written = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        if written[index] {
            return;
        }
        let mut linear = vec![0u8; mip_bytes];
        for row in 0..rows {
            // SAFETY: `end <= self.size` and `pitch >= row_bytes`, so every
            // row is inside the mapped staging space supplied to `detile`.
            let from = unsafe { std::slice::from_raw_parts(data.add(footprint.Offset as usize + row * pitch), row_bytes) };
            linear[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(from);
        }
        let name = format!("lighting-bc-{index}-{width}x{width}-{}-mip0", if bytes == 8 { "bc1" } else { "bc3" });
        let source_path = dir.join(format!("{name}-tiled.bc"));
        let linear_path = dir.join(format!("{name}-linear.bc"));
        let manifest_path = dir.join(format!("{name}.txt"));
        let manifest = format!(
            "index={index}\naddress={:#x}\nwidth={width}\nheight={width}\nformat={}\nswizzle=SW_4KB_S\nsource_mip0_offset={}\nsource_bytes={}\nlinear_row_bytes={}\nhost_row_pitch={}\n",
            self.memory as u64,
            if bytes == 8 { "BC1" } else { "BC3" },
            mip.offset,
            mip_bytes,
            row_bytes,
            pitch,
        );
        let result = std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&source_path, source))
            .and_then(|()| std::fs::write(&linear_path, &linear))
            .and_then(|()| std::fs::write(&manifest_path, manifest));
        match result {
            Ok(()) => {
                let mut lower = 0usize;
                for mip in 1..mips as usize {
                    if !subresources.contains(&(mip as u32)) { continue; }
                    let edge = (width as usize >> mip).max(1);
                    let row_bytes = edge.div_ceil(4) * bytes as usize;
                    let rows = edge.div_ceil(4);
                    let Some(footprint) = self.footprints.get(mip) else { break };
                    let pitch = footprint.Footprint.RowPitch as usize;
                    let Some(end) = footprint.Offset.checked_add(((rows - 1) * pitch + row_bytes) as u64) else { break };
                    if pitch < row_bytes || self.rows.get(mip).copied() != Some(rows as u32) || end > self.size {
                        tracing::warn!(index, mip, pitch, rows, "lighting BC lower mip lies outside staging footprint");
                        break;
                    }
                    let mut linear = vec![0u8; row_bytes * rows];
                    for row in 0..rows {
                        // SAFETY: this row is inside the mapped staging
                        // space by the footprint and end checks above.
                        let from = unsafe { std::slice::from_raw_parts(data.add(footprint.Offset as usize + row * pitch), row_bytes) };
                        linear[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(from);
                    }
                    let path = dir.join(format!("lighting-bc-{index}-{width}x{width}-{}-mip{mip}-linear.bc", if bytes == 8 { "bc1" } else { "bc3" }));
                    if let Err(error) = std::fs::write(&path, linear) {
                        tracing::warn!(index, mip, %error, "lighting BC lower mip write failed");
                        break;
                    }
                    lower += 1;
                }
                written[index] = true;
                tracing::info!(index, width, bytes = mip_bytes, lower_mips = lower, tiled = %source_path.display(), linear = %linear_path.display(), "lighting BC source and staged mip chain dumped locally");
            }
            Err(error) => tracing::warn!(index, %error, directory = %dir.display(), "lighting BC diagnostic dump failed"),
        }
    }

    /// Two bounded observations of each selected world-lighting source.
    /// The page probe is read-only and diagnostic: it reports pages that
    /// `ReadProcessMemory` cannot read, plus pages zero in the snapshot sent
    /// to the detiler. A zero page can also be valid texture data.
    #[cfg(feature = "diagnostics")]
    fn note_priority_detile(&self, tiled: &[u8], subresources: &[u32], succeeded: bool) {
        let address = self.memory as u64;
        let Some(index) = crate::resource::priority_bound_source(address) else { return };
        static SEEN: std::sync::OnceLock<Mutex<std::collections::HashMap<u64, u8>>> = std::sync::OnceLock::new();
        let seen = SEEN.get_or_init(Default::default);
        let attempt = {
            let mut seen = seen.lock().unwrap_or_else(|e| e.into_inner());
            let count = seen.entry(address).or_default();
            if *count >= 2 {
                return;
            }
            *count += 1;
            *count
        };
        const PAGE: usize = 4096;
        let size = self.layout.size as usize;
        let mut pages = vec![false; size.div_ceil(PAGE)];
        for &subresource in subresources {
            let (from, to) = self.spans.get(subresource as usize).copied().unwrap_or((0, size));
            for selected in pages.iter_mut().take(to.min(size).div_ceil(PAGE)).skip(from / PAGE) {
                *selected = true;
            }
        }
        let mut probed = [0u8; PAGE];
        let mut tile_bytes = 0usize;
        let mut zero_pages = 0usize;
        let mut unreadable_pages = 0usize;
        let mut page_count = 0usize;
        for (page, &selected) in pages.iter().enumerate() {
            if !selected {
                continue;
            }
            let from = page * PAGE;
            let to = (from + PAGE).min(size);
            let len = to - from;
            tile_bytes += len;
            page_count += 1;
            if tiled.get(from..to).is_some_and(|bytes| bytes.iter().all(|&byte| byte == 0)) {
                zero_pages += 1;
            }
            if !crate::host::read_into(self.memory.wrapping_add(from), &mut probed[..len]) {
                unreadable_pages += 1;
            }
        }
        tracing::info!(index, address = format_args!("{address:#x}"), attempt, original_width = self.layout.desc.width, original_height = self.layout.desc.height, original_mips = self.layout.desc.mips, host_mips = self.mips, dropped_mips = self.dropped, swizzle = self.layout.desc.swizzle.0, subresources = subresources.len(), tile_bytes, page_count, zero_pages, unreadable_pages, detile_succeeded = succeeded, "bound full-mip lighting texture detile probe");
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

/// Bytes a host buffer must cover for these footprints.
///
/// [`ID3D12Device::GetCopyableFootprints`] omits the row-pitch padding on
/// the last row of the last subresource. [`ID3D12GraphicsCommandList::CopyTextureRegion`]
/// still addresses every row at `RowPitch`, including that one. A buffer
/// sized to the tight total (a 2×1 `R32_FLOAT` mip is 8 bytes at pitch 256)
/// removes the device with `DXGI_ERROR_INVALID_CALL` on AMD.
fn footprint_bytes(
    total: u64,
    footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
    rows: &[u32],
) -> u64 {
    footprints.iter().zip(rows).fold(total, |total, (f, &n)| {
        let pitched = u64::from(f.Footprint.RowPitch)
            .saturating_mul(u64::from(n))
            .saturating_mul(u64::from(f.Footprint.Depth.max(1)));
        total.max(f.Offset.saturating_add(pitched))
    })
}

/// Bytes available in the staging buffer for a subresource at `offset`.
/// D3D12's `GetCopyableFootprints` omits trailing row-pitch padding on the
/// final row of the final subresource, so `pitch * rows * depth` can exceed
/// `total - offset` unless the caller used [`footprint_bytes`].
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
    let total = footprint_bytes(total, &footprints, &rows);
    let dropped = copy.dropped;
    tracing::debug!(?desc, total, dropped, "texture contents uploaded from title memory");
    let layout_size = layout.size as usize;
    let spans = spans(&layout, count, u32::from(d.MipLevels), dropped);
    let s = Arc::new(Staging {
        size: total,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        dropped,
        title_format: crate::resource::title_format(resource).unwrap_or(0),
        host_format: d.Format.0 as u32,
        memory,
        tracked: crate::watch::track(memory as usize, layout_size, true),
        spans,
        last: Mutex::default(),
        uploads: Default::default(),
        copied: Default::default(),
    });
    *held = Some(s.clone());
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprint_bytes_covers_the_last_rows_pitch() {
        // The 128×72 R32_FLOAT chain that removed the device: last mip
        // 2×1 at offset 0xd600, pitch 256, tight total 0xd608 (8 texels).
        let last = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0xd600,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Width: 2,
                Height: 1,
                Depth: 1,
                RowPitch: 256,
                ..Default::default()
            },
        };
        assert_eq!(footprint_bytes(0xd608, &[last], &[1]), 0xd700);
        let mut full = last;
        full.Offset = 0;
        full.Footprint.RowPitch = 512;
        full.Footprint.Height = 72;
        assert_eq!(footprint_bytes(512 * 72, &[full], &[72]), 512 * 72);
    }

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
}
