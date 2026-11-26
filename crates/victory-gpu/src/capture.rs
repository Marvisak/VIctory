//! Presented frames written to PNG files, to see what reaches the screen:
//! with `VICTORY_CAPTURE=<folder>`, every frame whose number is a power of
//! two (the frames the log reports) is saved as `frame-<n>.png`, at half
//! size. Only in builds with diagnostics.

use std::path::PathBuf;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// The folder frames go to, if capturing.
pub(crate) fn folder() -> Option<&'static PathBuf> {
    static FOLDER: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    FOLDER
        .get_or_init(|| std::env::var_os("VICTORY_CAPTURE").map(PathBuf::from))
        .as_ref()
}

/// Frames listed in `VICTORY_CAPTURE_FRAMES` (comma-separated): saved
/// too, and their draws traced at debug level. `VICTORY_CAPTURE_WORLD`
/// lists frames counted from the first one that ran 500 draws (the world's
/// first, whose number depends on how fast the title loaded).
pub(crate) fn listed(n: u64) -> bool {
    static LIST: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| numbers("VICTORY_CAPTURE_FRAMES")).contains(&n)
        || WORLD
            .get()
            .is_some_and(|&w| world_offsets().iter().any(|&k| w + k == n))
}

fn numbers(var: &str) -> Vec<u64> {
    let list = std::env::var(var).unwrap_or_default();
    list.split(',')
        .filter_map(|f| f.trim().parse().ok())
        .collect()
}

fn world_offsets() -> &'static [u64] {
    static OFFSETS: std::sync::OnceLock<Vec<u64>> = std::sync::OnceLock::new();
    OFFSETS.get_or_init(|| numbers("VICTORY_CAPTURE_WORLD"))
}

/// The first frame that ran 500 draws on the host.
static WORLD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Frame `n` was presented: notes the world's first frame.
pub(crate) fn presented(n: u64) {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let drawn = crate::draw::drawn();
    let draws = drawn - LAST.swap(drawn, Relaxed).min(drawn);
    if draws >= 500 && WORLD.get().is_none() && !world_offsets().is_empty() {
        let _ = WORLD.set(n);
        tracing::info!(frame = n, draws, "the world's first frame (VICTORY_CAPTURE_WORLD counts from it)");
    }
}

/// Whether frame `n` is saved.
pub(crate) fn wanted(n: u64) -> bool {
    folder().is_some() && (n.is_power_of_two() || listed(n))
}

/// Saves 8-bit RGBA or BGRA rows (`pitch` bytes apart) as frame `n`.
pub(crate) fn save(n: u64, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) {
    save_as(&format!("frame-{n}"), width, height, pitch, bgra, rows);
}

/// Saves 8-bit RGBA or BGRA rows as `<name>.png`, at half size.
fn save_as(name: &str, width: u32, height: u32, pitch: usize, bgra: bool, rows: &[u8]) {
    let Some(folder) = folder() else { return };
    let (w, h) = ((width / 2).max(1), (height / 2).max(1));
    // Filter byte 0 per row, then RGB of every other pixel of every other row.
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rows[(2 * y).min(height as usize - 1) * pitch..];
        for x in 0..w as usize {
            let p = &row[(2 * x).min(width as usize - 1) * 4..][..3];
            if bgra {
                raw.extend([p[2], p[1], p[0]]);
            } else {
                raw.extend_from_slice(p);
            }
        }
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(w.to_be_bytes());
    header.extend(h.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut png, b"IHDR", &header);
    chunk(
        &mut png,
        b"IDAT",
        &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 3),
    );
    chunk(&mut png, b"IEND", &[]);
    let path = folder.join(format!("{name}.png"));
    match std::fs::create_dir_all(folder).and_then(|()| std::fs::write(&path, png)) {
        Ok(()) => tracing::info!(path = %path.display(), "frame saved"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "frame not saved"),
    }
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// Most texels a target dump decodes: 512 MiB as the RGBA it writes. A
/// footprint naming more is no copy of a texture this title has.
const MAX_TEXELS: u64 = 1 << 27;

/// Most bytes a target's read-back buffer takes.
const MAX_READBACK: u64 = 1 << 30;

/// The bytes `rows` rows take `pitch` bytes apart: every row pitched but
/// the last, which the runtime leaves unpadded (a block-compressed
/// footprint's rows are rows of blocks). What a copy's total covers and what
/// its mapping spans. `None` when the footprint names no row at all, or a
/// row longer than its pitch.
fn row_bytes(pitch: u64, rows: u32, row: u64) -> Option<u64> {
    let rows = u64::from(rows);
    (rows > 0 && row > 0 && row <= pitch).then(|| pitch * (rows - 1) + row)
}

/// Whether `f` is a copy of `d`'s first subresource: a format with a known
/// element ([`crate::resource::element`]), the description's own extent at
/// mip 0 (block-aligned), a row pitch holding a row, a third dimension of
/// the copy as deep as the description is, no more texels than a dump
/// decodes, and nothing multi-sampled (a multi-sampled resource is not
/// copied out at all). What the host gives for a texture it cannot copy is a
/// footprint of 0xFF in every field — the format included — and no total:
/// sizing anything from that is what aborted a capture run (0xFFFFFFFF by
/// 0xFFFFFFFF texels named 103,079,215,080 bytes of PNG rows).
fn footprint_fits(d: &D3D12_RESOURCE_DESC, f: &D3D12_SUBRESOURCE_FOOTPRINT) -> bool {
    let Some((element, block)) = crate::resource::element(f.Format.0 as u32) else {
        return false;
    };
    let (Some(width), Some(height)) = (
        d.Width.checked_next_multiple_of(block),
        u64::from(d.Height).checked_next_multiple_of(block),
    ) else {
        return false;
    };
    // A third dimension is the copy's own extent; an array's layers are not.
    let depth = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u64::from(d.DepthOrArraySize)
    } else {
        1
    };
    let (w, h) = (u64::from(f.Width), u64::from(f.Height));
    let texels = (w / block)
        .checked_mul(h / block)
        .and_then(|t| t.checked_mul(depth));
    d.SampleDesc.Count == 1
        && width > 0
        && height > 0
        && w == width
        && h == height
        && u64::from(f.Depth) == depth
        && texels.is_some_and(|t| t <= MAX_TEXELS)
        && u64::from(f.RowPitch) >= width / block * element
}

/// Records a copy of `texture` (in the copy-source state) into a
/// readback buffer.
pub(crate) fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut total = 0;
    let (mut rows, mut row) = (0u32, 0u64);
    // SAFETY: a live texture; outputs for one subresource.
    unsafe {
        let desc = texture.GetDesc();
        device.GetCopyableFootprints(
            &desc,
            0,
            1,
            0,
            Some(&mut footprint),
            Some(&mut rows),
            Some(&mut row),
            Some(&mut total),
        );
        // Footprints the host cannot give are not copies: refuse them here,
        // where a buffer would take their size, rather than at whichever
        // arithmetic reads them later (the title aborts on that allocation).
        // Rows of no rows (or of none) are refused by name, not left to the
        // subtraction.
        let f = footprint.Footprint;
        let expected = row_bytes(u64::from(f.RowPitch), rows, row);
        let copyable = footprint_fits(&desc, &f)
            && expected.is_some_and(|expected| total >= expected && total <= MAX_READBACK);
        if !copyable {
            tracing::info!(
                resource =
                    format_args!("{:#x}", windows::core::Interface::as_raw(texture) as usize),
                format = desc.Format.0,
                width = f.Width,
                height = f.Height,
                depth = f.Depth,
                pitch = f.RowPitch,
                rows,
                row,
                total,
                "target not read back: the host gives no copyable footprint"
            );
            return None;
        }
    }
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: total,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; an open list; locations naming live resources.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
        let buffer = buffer?;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&buffer),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(texture),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        Some((buffer, footprint))
    }
}

/// Saves a frame read back by [`read_back`] (8-bit color only).
pub(crate) fn save_read_back(
    frame: u64,
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    let f = footprint.Footprint;
    let bgra = match f.Format {
        DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => false,
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => true,
        other => {
            tracing::warn!(format = other.0, "frame not saved: not 8-bit color");
            return;
        }
    };
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished (every submission is waited for); the
    // mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let len = f.RowPitch as usize * f.Height as usize;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), len);
        save(frame, f.Width, f.Height, f.RowPitch as usize, bgra, rows);
        buffer.Unmap(0, None);
    }
}

/// With `VICTORY_DUMP_TARGETS` set: title textures the frames listed in
/// `VICTORY_CAPTURE_FRAMES` render to or write, in first-use order, saved
/// at that frame's present as `frame-<n>-<i>-<resource>-<w>x<h>-f<format>.png`
/// (what each holds at the end of the frame; formats normalized to show
/// structure: floats scaled by their largest value, depth by its range).
/// The host copies are held: the title may release a target before the
/// frame is presented.
/// Each with the first pipeline that wrote it as an unordered-access view
/// (`-by-<pipeline>` in the name; `-black` when its colour is all zeros).
/// `VICTORY_DUMP_WRITERS` (shader hashes) adds, in any frame, the textures
/// a pipeline with one of them writes, the first frame it runs.
#[allow(clippy::type_complexity)]
static TARGETS: std::sync::Mutex<
    Vec<(usize, std::sync::Arc<crate::resource::HostTexture>, String)>,
> = std::sync::Mutex::new(Vec::new());

thread_local! {
    /// The pipeline whose draw or dispatch this thread is binding.
    static WRITER: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Names the pipeline whose views this thread binds next (empty: none).
pub(crate) fn binding(pipeline: &str) {
    if dump_on() {
        WRITER.with(|w| pipeline.clone_into(&mut w.borrow_mut()));
    }
}

pub(crate) fn dump_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && crate::switch::on("VICTORY_DUMP_TARGETS"))
}

/// `VICTORY_DUMP_WRITERS`' hashes not dumped yet.
static WRITERS: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// Buffers such a pipeline writes: title memory, its bytes when first
/// bound in the frame, the pipeline. Logged at the present: dwords changed
/// and dwords not zero.
#[allow(clippy::type_complexity)]
static BUFFERS: std::sync::Mutex<Vec<(usize, Vec<u8>, String)>> = std::sync::Mutex::new(Vec::new());

/// Notes a title texture the frame being built renders to or writes (by a
/// command of `list`, whose submission dumps it again when the frame is
/// listed).
pub(crate) fn note_target(list: &crate::host::HostList, resource: crate::object::This) {
    if resource.is_null() || !dump_on() {
        return;
    }
    let by = WRITER.with(|w| w.borrow().clone());
    // Lists are recorded ahead of the frame that submits them.
    list.note_target(resource as usize, &by);
    if !listed(crate::queue::presented() + 1) {
        let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
        let writers = writers.get_or_insert_with(|| {
            let list = std::env::var("VICTORY_DUMP_WRITERS").unwrap_or_default();
            list.split(',').map(|h| h.trim().to_owned()).filter(|h| !h.is_empty()).collect()
        });
        if by.is_empty() || !writers.iter().any(|h| by.contains(h.as_str())) {
            return;
        }
        if let Some((at, len)) = crate::resource::buffer_bytes(resource) {
            let mut buffers = BUFFERS.lock().unwrap_or_else(|e| e.into_inner());
            if !buffers.iter().any(|b| b.0 == at as usize) {
                let mut before = vec![0u8; (len as usize).min(64 << 20) & !3];
                if crate::host::read_into(at, &mut before) {
                    buffers.push((at as usize, before, by));
                }
            }
            return;
        }
    }
    let mut targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    if !targets.iter().any(|(r, _, _)| *r == resource as usize)
        && let Some(copy) = crate::resource::host_copy(resource)
    {
        targets.push((resource as usize, copy, by));
    }
}

/// Whether textures wait to be dumped at this present.
pub(crate) fn has_targets() -> bool {
    !TARGETS.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        || !BUFFERS.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
}

/// At the present of frame `frame`: saves the noted targets.
pub(crate) fn dump_targets(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let targets = std::mem::take(&mut *TARGETS.lock().unwrap_or_else(|e| e.into_inner()));
    let buffers = std::mem::take(&mut *BUFFERS.lock().unwrap_or_else(|e| e.into_inner()));
    // Writers dumped once.
    if let Some(writers) = WRITERS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        writers.retain(|h| {
            !targets.iter().any(|t| t.2.contains(h.as_str()))
                && !buffers.iter().any(|b| b.2.contains(h.as_str()))
        });
    }
    for (at, before, by) in &buffers {
        let mut after = vec![0u8; before.len()];
        if !crate::host::read_into(*at as *const u8, &mut after) {
            continue;
        }
        let dwords = |b: &[u8]| -> Vec<u32> {
            b.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap_or_default())).collect()
        };
        let (old, new) = (dwords(before), dwords(&after));
        let changed = old.iter().zip(&new).filter(|(a, b)| a != b).count();
        let set = new.iter().filter(|&&x| x != 0).count();
        tracing::info!(
            frame,
            buffer = format_args!("{at:#x}"),
            dwords = new.len(),
            changed,
            set,
            by = by.as_str(),
            "a buffer the named pipeline writes, at the present"
        );
    }
    let Some(host) = crate::host::host() else { return };
    for (i, (resource, copy, by)) in targets.iter().enumerate() {
        let texture = &copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.SampleDesc.Count > 1 || d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            continue;
        }
        // Every submission has run; the dump's list moves it there and back.
        let state = copy.state.load(Relaxed);
        let target = (i, *resource as crate::object::This, by.as_str());
        dump_one(host, texture, state, &format!("frame-{frame}"), target, &d);
    }
}

/// After submission `sub` of a listed frame has run: the textures its lists
/// rendered to or wrote, as `frame-<n>-s<sub>-<i>-...` (what each holds
/// then, to follow a frame pass by pass).
pub(crate) fn dump_submission(sub: u64, targets: &[(usize, String)]) {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(host) = crate::host::host() else { return };
    let frame = crate::queue::presented() + 1;
    peek(&format!("frame-{frame}-s{sub:04}"));
    for (i, (resource, by)) in targets.iter().enumerate() {
        let Some(copy) = crate::resource::host_copy(*resource as crate::object::This) else {
            continue;
        };
        let texture = &copy.texture;
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.SampleDesc.Count > 1 || d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            continue;
        }
        let state = copy.state.load(Relaxed);
        let target = (i, *resource as crate::object::This, by.as_str());
        dump_one(host, texture, state, &format!("frame-{frame}-s{sub:04}"), target, &d);
        // A depth texture's writable twin (`crate::twin`): what unordered
        // views wrote, which reaches the depth texture at its next use.
        for (p, plane) in copy.twin.get().and_then(Option::as_ref).iter().flat_map(|t| t.planes.iter().enumerate()) {
            // SAFETY: a live resource.
            let pd = unsafe { plane.texture.GetDesc() };
            let by = format!("{by}-twin{p}");
            let target = (i, *resource as crate::object::This, by.as_str());
            dump_one(host, &plane.texture, plane.state.load(Relaxed), &format!("frame-{frame}-s{sub:04}"), target, &pd);
        }
    }
}

/// `VICTORY_PEEK=<address>[:<bytes>],...` (hex addresses, 16 bytes by
/// default): those bytes of title memory, as dwords and floats, after each
/// submission of a listed frame.
fn peek(when: &str) {
    static LIST: std::sync::OnceLock<Vec<(u64, usize)>> = std::sync::OnceLock::new();
    let list = LIST.get_or_init(|| {
        std::env::var("VICTORY_PEEK")
            .unwrap_or_default()
            .split(',')
            .filter_map(|e| {
                let (a, n) = e.split_once(':').unwrap_or((e, "16"));
                let a = u64::from_str_radix(a.trim().trim_start_matches("0x"), 16).ok()?;
                Some((a, n.trim().parse().ok()?))
            })
            .collect()
    });
    for &(at, len) in list {
        let mut bytes = vec![0u8; len.min(256)];
        if !crate::host::read_into(at as *const u8, &mut bytes) {
            continue;
        }
        let words: Vec<String> = bytes
            .chunks(4)
            .map(|c| {
                let w = u32::from_le_bytes(c.try_into().unwrap_or_default());
                format!("{w:#x}={}", f32::from_bits(w))
            })
            .collect();
        tracing::info!("peek {when} {at:#x}: {}", words.join(" "));
    }
}

/// In a listed frame: copies an indirect execution's records (`args`: the
/// list's argument buffer and offset, in the indirect-argument state) and
/// its count (a title buffer's copy) to readback memory,
/// logged once the list has run: how many records ask for work, and the
/// first ones.
pub(crate) fn read_indirect(
    list: &crate::host::HostList,
    args: (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    (kind, stride, max): (u32, u32, u32),
    name: String,
    always: bool,
) {
    struct Readback(ID3D12Resource);
    // SAFETY: D3D12 objects are free-threaded.
    unsafe impl Send for Readback {}
    // SAFETY: as above.
    unsafe impl Sync for Readback {}
    let Some(host) = crate::host::host() else { return };
    let len = u64::from(stride) * u64::from(max.max(1));
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: len + 4,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let mut buffer: Option<ID3D12Resource> = None;
    let l = &list.list;
    use crate::host::transition;
    let (arg_state, copy) = (D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COPY_SOURCE);
    // SAFETY: plain creation; an open list and live buffers in the states
    // named, put back after the copies.
    unsafe {
        if host
            .device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut buffer)
            .is_err()
        {
            return;
        }
        let Some(buffer) = buffer else { return };
        l.ResourceBarrier(&[transition(args.0, arg_state, copy)]);
        l.CopyBufferRegion(&buffer, 0, args.0, args.1, len);
        if let Some((c, at)) = count {
            // A title buffer's copy: never transitioned, the read promotes it.
            l.CopyBufferRegion(&buffer, len, c, at, 4);
        }
        l.ResourceBarrier(&[transition(args.0, copy, arg_state)]);
        list.keep(&buffer);
        let buffer = Readback(buffer);
        let counted = count.is_some();
        list.defer_after(move || {
            // Recorded ahead: logged if the frame running it is listed.
            let frame = crate::queue::presented() + 1;
            if !always && !listed(frame) {
                return;
            }
            let mut data = std::ptr::null_mut();
            if buffer.0.Map(0, None, Some(&mut data)).is_err() {
                return;
            }
            let words = std::slice::from_raw_parts(data.cast::<u32>(), (len as usize + 4) / 4);
            let n = counted.then(|| words[len as usize / 4]);
            let records = n.unwrap_or(max).min(max) as usize;
            let step = stride as usize / 4;
            let first = |r: usize| &words[r * step..r * step + step.min(4)];
            // Draws: vertices (or indices) × instances; dispatches: groups.
            let work = |r: usize| -> u64 {
                let w = first(r);
                match kind {
                    0 | 1 => u64::from(w[0]) * u64::from(w.get(1).copied().unwrap_or(1)),
                    _ => w.iter().take(3).map(|&x| u64::from(x)).product(),
                }
            };
            let busy = (0..records).filter(|&r| work(r) > 0).count();
            let total: u64 = (0..records).map(work).sum();
            let shown: Vec<&[u32]> = (0..records.min(4)).map(first).collect();
            tracing::info!(
                target: "victory_gpu::trace",
                "indirect kind {kind} max {max} count {n:?}: {busy} of {records} records with work, {total} in all; first {shown:?} {name} frame {frame}"
            );
            buffer.0.Unmap(0, None);
        });
    }
}

fn dump_one(
    host: &crate::host::Host,
    texture: &ID3D12Resource,
    state: u32,
    prefix: &str,
    (i, resource, by): (usize, crate::object::This, &str),
    d: &D3D12_RESOURCE_DESC,
) {
    let Ok(list) = host.new_list() else { return };
    let state = D3D12_RESOURCE_STATES(crate::host::legal_state(texture, state) as i32);
    let copy = D3D12_RESOURCE_STATE_COPY_SOURCE;
    // SAFETY: an open list of ours and a live texture.
    let read = unsafe {
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, state, copy)]);
        }
        let read = read_back(&host.device, &list.list, texture);
        if state != copy {
            list.list
                .ResourceBarrier(&[crate::host::transition(texture, copy, state)]);
        }
        read
    };
    list.close();
    host.execute(&[&list]);
    let Some((buffer, footprint)) = read else { return };
    let f = footprint.Footprint;
    if !footprint_fits(d, &f) {
        tracing::info!(
            ?resource,
            format = f.Format.0,
            width = f.Width,
            height = f.Height,
            pitch = f.RowPitch,
            "target not dumped: its footprint names no copy of it"
        );
        return;
    }
    // One row of a format this dump decodes is a row of texels; a
    // block-compressed footprint's rows are rows of blocks, which this dump
    // (as before) does not decode.
    let Some((texel, 1)) = crate::resource::element(f.Format.0 as u32) else {
        tracing::info!(format = f.Format.0, ?resource, "target not dumped: format");
        return;
    };
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the rows read below.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let pitch = f.RowPitch as usize;
        let row = f.Width as usize * texel as usize;
        // The buffer's own bytes, not `pitch * height`: a copy stops at the
        // last row's own size (a 1x1 target fills 4 bytes of a 256-byte
        // pitch), so the pitch-named bytes past it are not there.
        let len = (buffer.GetDesc().Width as usize).min(MAX_READBACK as usize);
        let Some(span) = row_bytes(f.RowPitch as u64, f.Height, row as u64)
            .map(|bytes| bytes as usize)
            .filter(|&bytes| bytes <= len)
        else {
            tracing::info!(
                format = f.Format.0,
                ?resource,
                pitch,
                height = f.Height,
                buffer = len,
                "target not dumped: its rows do not fit its read-back"
            );
            buffer.Unmap(0, None);
            return;
        };
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), span);
        // Tiny targets (exposure, averages): their values as they are.
        if u64::from(f.Width) * u64::from(f.Height) <= 16 {
            let words: Vec<String> = (0..f.Height as usize)
                .flat_map(|y| rows[y * pitch..][..row].chunks(4))
                .map(|c| {
                    let w = u32::from_le_bytes(c.try_into().unwrap_or_default());
                    format!("{w:#x}={}", f32::from_bits(w))
                })
                .collect();
            tracing::info!(?resource, format = f.Format.0, "tiny target {prefix}-{i:02}: {}", words.join(" "));
        }
        // A 24-bit depth plane copies out as 32-bit words (`R32_TYPELESS`):
        // decode it as depth, not as floats (all denormals, shown black).
        let format = if matches!(d.Format.0, 44..=47) { d.Format.0 } else { f.Format.0 };
        if let Some(rgba) = to_rgba(format as u32, f.Width as usize, f.Height as usize, pitch, rows) {
            let mut name = format!(
                "{prefix}-{i:02}-{resource:?}-{}x{}-f{}",
                d.Width, d.Height, d.Format.0
            );
            // The pipeline's hashes (its name is "[stage hash, ...]").
            let by: String = by.chars().filter(char::is_ascii_alphanumeric).collect();
            if !by.is_empty() {
                name.push_str("-by-");
                name.push_str(&by);
            }
            // Whether anything but zeros is there (a dump is normalized).
            if rgba.chunks(4).all(|p| p[..3] == [0, 0, 0]) {
                name.push_str("-black");
            }
            save_as(&name, f.Width, f.Height, f.Width as usize * 4, false, &rgba);
        } else {
            tracing::info!(format = f.Format.0, ?resource, "target not dumped: format");
        }
        buffer.Unmap(0, None);
    }
}

fn half(h: u16) -> f32 {
    let (sign, exp, man) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 if man != 0.0 => f32::NAN,
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    };
    if sign == 1 { -v } else { v }
}

/// A float packed in `bits` with a 5-bit exponent and `man` mantissa bits
/// (the 11- and 10-bit floats of R11G11B10).
fn small_float(bits: u32, man: u32) -> f32 {
    let (exp, m) = ((bits >> man) & 0x1f, (bits & ((1 << man) - 1)) as f32);
    match exp {
        0 => m / (1 << man) as f32 * 2f32.powi(-14),
        31 if m != 0.0 => f32::NAN,
        31 => f32::INFINITY,
        e => (1.0 + m / (1 << man) as f32) * 2f32.powi(e as i32 - 15),
    }
}

/// The format a dump decodes `format` as: a typeless format as the value its
/// bits hold — its family's typed twin (R32_TYPELESS as R32_FLOAT, R24G8 as
/// D24_UNORM_S8, R16_TYPELESS as R16_FLOAT, the B8G8R8A8 typelesses as
/// their sRGB twins, whose bytes are the same) — and everything else as
/// itself. Block-compressed and video formats stay as they are: [`to_rgba`]
/// decodes those not at all and says so.
fn readable(format: u32) -> u32 {
    use crate::resource::family;
    // A typeless format starts its family, and so does the member after it.
    let typeless = family(format) == format && family(format.saturating_add(1)) == format;
    if typeless {
        format.saturating_add(1)
    } else {
        format
    }
}

/// Rows of a host format as 8-bit RGBA: 8-bit color as is, everything else
/// as channel values scaled so the largest finite one is white (single
/// channels: lowest black, highest white, as grey). `None` for formats not
/// handled, and for an extent no dump sizes (a footprint this dump did not
/// vouch for must never size an allocation).
fn to_rgba(format: u32, w: usize, h: usize, pitch: usize, rows: &[u8]) -> Option<Vec<u8>> {
    let format = readable(format);
    let texels = (w as u64).checked_mul(h as u64)?;
    if texels == 0 || texels > MAX_TEXELS {
        return None;
    }
    let px = |x: usize, y: usize, n: usize| &rows[y * pitch + x * n..][..n];
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap_or([0; 4]));
    if matches!(format, 42 | 43) {
        let words = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| u32_at(px(x, y, 4), 0));
        return Some(r32_integer_to_rgba(format, words));
    }
    let mut out = Vec::with_capacity(w * h * 4);
    // Channel values as floats, then scaled.
    type Decode = Box<dyn Fn(&[u8]) -> [f32; 3]>;
    let (n, decode): (usize, Decode) = match format {
        27..=32 => {
            // The PNG drops alpha: its range and how many texels have each
            // channel zero are logged.
            let (mut low, mut high, mut zero) = ([255u8; 4], [0u8; 4], [0usize; 4]);
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    for c in 0..4 {
                        (low[c], high[c]) = (low[c].min(p[c]), high[c].max(p[c]));
                        zero[c] += usize::from(p[c] == 0);
                    }
                    out.extend([p[0], p[1], p[2], 255]);
                }
            }
            tracing::info!(format, ?low, ?high, ?zero, texels = w * h, "target values (8-bit, RGBA)");
            return Some(out);
        }
        // B8G8R8A8 and B8G8R8X8 (89 is R10G10B10_XR_BIAS_A2, below).
        87 | 88 | 90..=93 => {
            for y in 0..h {
                for x in 0..w {
                    let p = px(x, y, 4);
                    out.extend([p[2], p[1], p[0], 255]);
                }
            }
            return Some(out);
        }
        23..=25 | 89 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [(v & 0x3ff) as f32, (v >> 10 & 0x3ff) as f32, (v >> 20 & 0x3ff) as f32]
        })),
        26 => (4, Box::new(move |b| {
            let v = u32_at(b, 0);
            [small_float(v & 0x7ff, 6), small_float(v >> 11 & 0x7ff, 6), small_float(v >> 22, 5)]
        })),
        9..=10 => (8, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), half(u16_at(b, 2))])),
        11..=14 => (8, Box::new(move |b| [0, 1, 2].map(|i| f32::from(u16_at(b, i))))),
        33..=34 => (4, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), 0.0])),
        1..=2 => (16, Box::new(move |b| {
            [0, 1, 2].map(|i| f32::from_bits(u32_at(b, i)))
        })),
        5..=6 => (12, Box::new(move |b| {
            [0, 1, 2].map(|i| f32::from_bits(u32_at(b, i)))
        })),
        15..=16 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)), f32::from_bits(u32_at(b, 1)), 0.0])),
        35..=38 => (4, Box::new(move |b| [f32::from(u16_at(b, 0)), f32::from(u16_at(b, 1)), 0.0])),
        48..=52 => (2, Box::new(move |b| [f32::from(b[0]), f32::from(b[1]), 0.0])),
        // Single channels (and depth planes): shown as grey.
        39..=41 => (4, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        19..=22 => (8, Box::new(move |b| [f32::from_bits(u32_at(b, 0)); 3])),
        44..=47 => (4, Box::new(move |b| [(u32_at(b, 0) & 0xff_ffff) as f32; 3])),
        53..=54 => (2, Box::new(move |b| [half(u16_at(b, 0)); 3])),
        55..=59 => (2, Box::new(move |b| [f32::from(u16_at(b, 0)); 3])),
        60..=65 => (1, Box::new(move |b| [f32::from(b[0]); 3])),
        _ => return None,
    };
    let values: Vec<[f32; 3]> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| decode(px(x, y, n)))
        .collect();
    let grey = matches!(format, 19..=22 | 39..=41 | 44..=47 | 53..=65);
    let finite = values.iter().flatten().copied().filter(|v| v.is_finite());
    let (low, max) = if grey {
        finite.fold((f32::MAX, f32::MIN), |(l, h), v| (l.min(v), h.max(v)))
    } else {
        (0.0, finite.fold(0.0f32, f32::max))
    };
    // Colour scaled by the 99th percentile of the non-zero values, so a few
    // hot pixels do not hide the rest; how many are not zero, and the median.
    let mut set: Vec<f32> = values
        .iter()
        .flatten()
        .copied()
        .filter(|v| v.is_finite() && *v != 0.0)
        .map(f32::abs)
        .collect();
    let nonzero = set.len() as f64 / (values.len() * 3).max(1) as f64;
    let mut at = |q: f64| -> f32 {
        if set.is_empty() {
            return 0.0;
        }
        let k = ((set.len() - 1) as f64 * q).round() as usize;
        *set.select_nth_unstable_by(k, f32::total_cmp).1
    };
    let (p50, p99) = (at(0.5), at(0.99));
    let not_finite = values.iter().flatten().filter(|v| !v.is_finite()).count();
    let high = if grey || p99 == 0.0 { max } else { p99 };
    let range = (high - low).max(1e-20);
    tracing::info!(format, low, high = max, p50, p99, nonzero, not_finite, "target values");
    for v in values {
        let c = v.map(|c| (((c - low) / range).clamp(0.0, 1.0) * 255.0) as u8);
        out.extend([c[0], c[1], c[2], 255]);
    }
    Some(out)
}

fn r32_integer_to_rgba(format: u32, words: impl Iterator<Item = u32>) -> Vec<u8> {
    let signed = format == 43;
    let mut values: Vec<i64> = words
        .map(|word| if signed { i64::from(word as i32) } else { i64::from(word) })
        .collect();
    let first = values.first().copied().unwrap_or(0);
    let (low, high) = values.iter().copied().fold((first, first), |(low, high), value| {
        (low.min(value), high.max(value))
    });
    // The full 32-bit span scaled by 255 fits in i64, without float rounding.
    let range = (high - low).max(1);
    let flat_nonzero = high == low && low != 0;
    let mut out = Vec::with_capacity(values.len() * 4);
    for &value in &values {
        let grey = if flat_nonzero {
            255
        } else {
            ((value - low) * 255 / range) as u8
        };
        out.extend([grey, grey, grey, 255]);
    }

    let total = values.len();
    values.retain(|&value| value != 0);
    let nonzero = values.len() as f64 / total.max(1) as f64;
    for value in &mut values {
        *value = value.abs();
    }
    let mut at = |q: f64| -> i64 {
        if values.is_empty() {
            return 0;
        }
        let k = ((values.len() - 1) as f64 * q).round() as usize;
        *values.select_nth_unstable(k).1
    };
    let (p50, p99) = (at(0.5), at(0.99));
    tracing::info!(format, low, high, p50, p99, nonzero, not_finite = 0, "target values");
    out
}

/// Pixels per side of the grid [`probe`] reads.
const GRID: u32 = 4;
/// Bytes between the grid's pixels in the readback buffer (placement
/// alignment).
const SLOT: u64 = 512;

/// Records copies of a grid of pixels of `texture` (8-bit color, in the
/// copy-source state) into a readback buffer, to tell black frames.
pub(crate) fn probe(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<ID3D12Resource> {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    if !matches!(d.Format.0, 27..=29 | 87 | 90 | 91) {
        return None;
    }
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: SLOT * u64::from(GRID * GRID),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    // One buffer for every present: each is waited for before the next
    // (made and released per present, it cost a kernel call per frame).
    static BUFFER: std::sync::OnceLock<Option<ID3D12Resource>> = std::sync::OnceLock::new();
    let buffer = BUFFER
        .get_or_init(|| {
            let mut buffer: Option<ID3D12Resource> = None;
            // SAFETY: plain creation.
            unsafe {
                device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut buffer,
                )
            }
            .ok()?;
            buffer
        })
        .clone()?;
    // SAFETY: an open list; locations naming live resources.
    unsafe {
        for i in 0..GRID * GRID {
            let (x, y) = (
                (2 * (i % GRID) + 1) * d.Width as u32 / (2 * GRID),
                (2 * (i / GRID) + 1) * d.Height / (2 * GRID),
            );
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&buffer),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: SLOT * u64::from(i),
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: d.Format,
                            Width: 1,
                            Height: 1,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let pixel = D3D12_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + 1,
                bottom: y + 1,
                back: 1,
            };
            list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&pixel));
        }
        Some(buffer)
    }
}

/// Whether every pixel [`probe`] read is (near) black.
pub(crate) fn black(buffer: &ID3D12Resource) -> bool {
    let mut data = std::ptr::null_mut();
    // SAFETY: the copies finished (every submission is waited for).
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return false;
        }
        let bytes =
            std::slice::from_raw_parts(data.cast::<u8>(), (SLOT * u64::from(GRID * GRID)) as usize);
        let level = bytes
            .chunks(SLOT as usize)
            .flat_map(|pixel| pixel[..3].iter().copied())
            .max()
            .unwrap_or(0);
        buffer.Unmap(0, None);
        let mut levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
        if levels.len() == 12 {
            levels.pop_front();
        }
        levels.push_back(level);
        level < 8
    }
}

/// The brightest probed channel of the last frames, oldest first: a fade
/// darkens over several, a glitch drops at once.
static LEVELS: std::sync::Mutex<std::collections::VecDeque<u8>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

pub(crate) fn recent_levels() -> Vec<u8> {
    let levels = LEVELS.lock().unwrap_or_else(|e| e.into_inner());
    levels.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn half_classifies_special_and_finite_values() {
        use super::half;

        assert!(half(0x7c01).is_nan());
        assert!(half(0xfe00).is_nan());
        assert_eq!(half(0x7c00), f32::INFINITY);
        assert_eq!(half(0xfc00), f32::NEG_INFINITY);
        assert_eq!(half(0x0000).to_bits(), 0.0f32.to_bits());
        assert_eq!(half(0x8000).to_bits(), (-0.0f32).to_bits());
        assert_eq!(half(0x0001), 2f32.powi(-24));
        assert_eq!(half(0xbe00), -1.5);
    }

    #[test]
    fn packed_float_classifies_special_and_finite_values() {
        use super::small_float;

        for mantissa_bits in [5, 6] {
            let special = 31 << mantissa_bits;
            assert!(small_float(special | 1, mantissa_bits).is_nan());
            assert!(small_float(special | (1 << (mantissa_bits - 1)), mantissa_bits).is_nan());
            assert_eq!(small_float(special, mantissa_bits), f32::INFINITY);
            assert_eq!(small_float(0, mantissa_bits).to_bits(), 0.0f32.to_bits());
            assert_eq!(small_float(1, mantissa_bits), 2f32.powi(-14 - mantissa_bits as i32));
            assert_eq!(small_float(15 << mantissa_bits, mantissa_bits), 1.0);
        }
    }

    #[test]
    fn hdr_targets_render_nan_and_infinity_as_distinct_channel_values() {
        // R16G16B16A16_FLOAT: [NaN, +Inf, NaN], [1, 0.5, 0].
        let half = [0x7c01u16, 0x7c00, 0xfe00, 0, 0x3c00, 0x3800, 0, 0];
        let half: Vec<_> = half.into_iter().flat_map(u16::to_le_bytes).collect();
        // R11G11B10_FLOAT: the same values with 6-bit and 5-bit mantissas.
        let packed = [
            0x7c1u32 | (0x7c0 << 11) | (0x3e1 << 22),
            0x3c0 | (0x380 << 11),
        ];
        let packed: Vec<_> = packed.into_iter().flat_map(u32::to_le_bytes).collect();
        let expected = [0, 255, 0, 255, 255, 127, 0, 255];
        for (format, row) in [(10, half), (26, packed)] {
            assert_eq!(
                super::to_rgba(format, 2, 1, row.len(), &row).as_deref(),
                Some(&expected[..]),
                "format {format}"
            );
        }
    }

    #[test]
    fn crc_matches_the_png_check_value() {
        assert_eq!(super::crc32(b"IEND"), 0xae42_6082);
    }

    /// R16G16B16A16_UNORM is integers, not half floats; 89 is 10:10:10:2,
    /// not B8G8R8A8.
    #[test]
    fn unorm16_and_xr_bias_targets_decode_as_their_formats() {
        // Red 0x8000 (a half-float -0), green 0x4000, blue 0.
        let unorm16 = [0x00, 0x80, 0x00, 0x40, 0, 0, 0xff, 0xff];
        assert_eq!(super::to_rgba(11, 1, 1, 8, &unorm16), Some(vec![255, 127, 0, 255]));
        // Red 0x3ff, green 0x1ff, blue 0, alpha 3.
        let xr = (0x3ffu32 | 0x1ff << 10 | 3 << 30).to_le_bytes();
        assert_eq!(super::to_rgba(89, 1, 1, 4, &xr), Some(vec![255, 127, 0, 255]));
        // B8G8R8A8 still swaps to RGBA.
        assert_eq!(super::to_rgba(87, 1, 1, 4, &[1, 2, 3, 4]), Some(vec![3, 2, 1, 255]));
    }

    /// A typeless host format is decoded as the value its bits hold: its
    /// family's typed twin. Typed formats and the 0xFF the host leaves when
    /// it has no footprint at all stay as they are.
    #[test]
    fn typeless_formats_read_as_the_value_they_hold() {
        use super::readable;

        for (typeless, typed) in [
            (1, 2),   // R32G32B32A32: float
            (9, 10),  // R16G16B16A16: half
            (23, 24), // R10G10B10A2: unorm
            (27, 28), // R8G8B8A8: unorm
            (39, 40), // R32: float (depth copied out of a typeless 32)
            (44, 45), // R24G8: D24_UNORM_S8
            (53, 54), // R16: half
            (60, 61), // R8: unorm
            (90, 91), // B8G8R8A8: its sRGB twin, the same bytes
        ] {
            assert_eq!(readable(typeless), typed, "typeless {typeless}");
        }
        for typed in [2, 10, 16, 28, 40, 41, 42, 45, 54, 61, 87, 91] {
            assert_eq!(readable(typed), typed);
        }
        assert_eq!(readable(u32::MAX), u32::MAX);
    }

    /// A footprint the host could not give (every field 0xFF) fits no
    /// description; one of the description's own shape does.
    #[test]
    fn footprints_of_failed_copies_are_refused() {
        use super::{MAX_READBACK, footprint_fits};
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;

        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: 2560,
            Height: 1440,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };
        let copy = |format: i32, width: u32, height: u32, depth: u32, pitch: u32| {
            D3D12_SUBRESOURCE_FOOTPRINT {
                Format: DXGI_FORMAT(format),
                Width: width,
                Height: height,
                Depth: depth,
                RowPitch: pitch,
            }
        };
        // R8_UNORM 2560x1440: 2560-byte rows, pitched to the same.
        assert!(footprint_fits(&desc, &copy(61, 2560, 1440, 1, 2560)));
        // What vkd3d-proton leaves when it cannot give one: 0xFF everywhere,
        // the format included — 4 Gi texels a side, no format at all.
        assert!(!footprint_fits(
            &desc,
            &copy(-1, u32::MAX, u32::MAX, u32::MAX, u32::MAX)
        ));
        // Another description's shape, and a pitch too short for a row.
        assert!(!footprint_fits(&desc, &copy(61, 1280, 720, 1, 1280)));
        assert!(!footprint_fits(&desc, &copy(61, 2560, 1440, 1, 1024)));
        // Nothing multi-sampled is copied out, whatever its footprint says.
        let msaa = D3D12_RESOURCE_DESC {
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 4,
                Quality: 0,
            },
            ..desc
        };
        assert!(!footprint_fits(&msaa, &copy(61, 2560, 1440, 1, 2560)));
        const {
            assert!(MAX_READBACK >= 2560 * 1440);
        }
    }

    /// 6 x 0xFFFFFFFF texels are 103,079,215,080 bytes of PNG rows: no
    /// extent a title texture has, and nothing this dump allocates for.
    #[test]
    fn an_implausible_extent_is_not_allocated_for() {
        assert_eq!(super::to_rgba(61, 6, u32::MAX as usize, 256, &[]), None);
        assert_eq!(super::to_rgba(61, u32::MAX as usize, 6, 256, &[]), None);
        assert_eq!(super::to_rgba(u32::MAX, 0, 0, 0, &[]), None);
    }

    /// A mapped copy spans its rows, not its pitch: the last row is its own
    /// size (the read-back buffers a live matrix measured: a 1x1 target's
    /// 4 bytes in a 256-byte pitch, D24 64x64's 32,512, BC7 64x64's 4,096).
    #[test]
    fn a_mapping_spans_its_rows_and_not_its_pitch() {
        use super::row_bytes;

        // R8G8B8A8_UNORM 1x1: 4 bytes, not 256.
        assert_eq!(row_bytes(256, 1, 4), Some(4));
        // R8_UNORM 2560x1440: every row pitched but the last.
        assert_eq!(row_bytes(2560, 1440, 2560), Some(1439 * 2560 + 2560));
        // D24_UNORM_S8_UINT 64x64: 32,512 bytes, not 64 * 512.
        assert_eq!(row_bytes(512, 64, 256), Some(63 * 512 + 256));
        // BC7_UNORM 64x64: 16 rows of blocks, 4,096 bytes, not 64 * 256.
        assert_eq!(row_bytes(256, 16, 256), Some(15 * 256 + 256));
        // No rows, no row, or a row longer than the pitch it sits in.
        assert_eq!(row_bytes(256, 0, 4), None);
        assert_eq!(row_bytes(256, 1, 0), None);
        assert_eq!(row_bytes(256, 1, 300), None);
    }
}

#[cfg(test)]
mod r32_tests {
    #[test]
    fn r32_integer_captures_scale_numeric_values() {
        let unsigned = [1u32, 2, 3].map(u32::to_le_bytes).concat();
        let signed = [-2i32, -1, 0].map(i32::to_le_bytes).concat();
        let expected = Some(vec![0, 0, 0, 255, 127, 127, 127, 255, 255, 255, 255, 255]);

        assert_eq!(super::to_rgba(42, 3, 1, 12, &unsigned), expected);
        assert_eq!(super::to_rgba(43, 3, 1, 12, &signed), expected);
    }

    #[test]
    fn r32_integer_captures_keep_adjacent_contrast() {
        let unsigned = [16_777_216u32, 16_777_217].map(u32::to_le_bytes).concat();
        let signed = [-16_777_217i32, -16_777_216].map(i32::to_le_bytes).concat();
        let expected = Some(vec![0, 0, 0, 255, 255, 255, 255, 255]);

        assert_eq!(super::to_rgba(42, 2, 1, 8, &unsigned), expected);
        assert_eq!(super::to_rgba(43, 2, 1, 8, &signed), expected);
    }

    #[test]
    fn r32_integer_captures_handle_full_range_and_row_pitch() {
        let signed = [i32::MIN, -1, i32::MIN, 0, i32::MAX].map(i32::to_le_bytes).concat();
        let expected = Some(vec![0, 0, 0, 255, 127, 127, 127, 255, 127, 127, 127, 255, 255, 255, 255, 255]);

        assert_eq!(super::to_rgba(43, 2, 2, 12, &signed), expected);
    }

    #[test]
    fn r32_integer_captures_constant_uint_is_white() {
        let rows = [7u32; 2].map(u32::to_le_bytes).concat();
        assert_eq!(super::to_rgba(42, 2, 1, 8, &rows), Some(vec![255; 8]));
    }

    #[test]
    fn r32_integer_captures_constant_sint_is_white() {
        let rows = [-7i32; 2].map(i32::to_le_bytes).concat();
        assert_eq!(super::to_rgba(43, 2, 1, 8, &rows), Some(vec![255; 8]));
    }

    #[test]
    fn r32_integer_captures_constant_zero_is_black() {
        let expected = Some([0, 0, 0, 255].repeat(2));
        assert_eq!(super::to_rgba(42, 2, 1, 8, &[0; 8]), expected);
        assert_eq!(super::to_rgba(43, 2, 1, 8, &[0; 8]), expected);
    }
}
