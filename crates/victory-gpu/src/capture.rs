//! Presented frames written to PNG files, to see what reaches the screen:
//! with `VICTORY_CAPTURE=<folder>`, every frame whose number is a power of
//! two (the frames the log reports) is saved as `frame-<n>.png`, at half
//! size by default. `VICTORY_CAPTURE_FULL_RES=1` keeps presented frames at
//! their original size. Only in builds with diagnostics.

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

/// Keep only a specifically requested title shader container for offline
/// ISA inspection. The bytes stay in the run's capture directory.
pub(crate) fn dump_shader_source(hash: &str, bytes: &[u8]) {
    static WANTED: std::sync::OnceLock<std::collections::HashSet<String>> = std::sync::OnceLock::new();
    let wanted = WANTED.get_or_init(|| {
        std::env::var("VICTORY_DUMP_SHADER_HASH")
            .unwrap_or_default()
            .split(',')
            .map(|item| item.trim().to_ascii_lowercase())
            .filter(|item| item.len() == 16 && item.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect()
    });
    if !wanted.contains(hash) {
        return;
    }
    let Some(folder) = folder() else { return };
    let path = folder.join(format!("shader-{hash}.dxbc"));
    if let Err(error) = std::fs::write(&path, bytes) {
        tracing::warn!(%error, %hash, "shader source dump failed");
    } else {
        tracing::info!(%hash, path = %path.display(), bytes = bytes.len(), "shader source dumped");
    }
}

/// Frames listed in `VICTORY_CAPTURE_FRAMES` (comma-separated): saved
/// too, and their draws traced at debug level. `VICTORY_CAPTURE_WORLD`
/// lists frames counted from the first one that reaches the configured draw
/// threshold (whose frame number depends on how fast the title loaded).
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

/// First frame reaching `VICTORY_CAPTURE_WORLD_MIN_DRAWS` host draws.
static WORLD: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn world_min_draws() -> u64 {
    static MIN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("VICTORY_CAPTURE_WORLD_MIN_DRAWS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|&value| (500..=100_000).contains(&value))
            .unwrap_or(500)
    })
}

/// The first world frame, available to later targeted shader captures.
pub(crate) fn world_frame() -> Option<u64> {
    WORLD.get().copied()
}

/// Frame `n` was presented: notes the world's first frame.
pub(crate) fn presented(n: u64) {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let drawn = crate::draw::drawn();
    let draws = drawn - LAST.swap(drawn, Relaxed).min(drawn);
    if draws >= world_min_draws() && WORLD.get().is_none() && !world_offsets().is_empty() {
        let _ = WORLD.set(n);
        tracing::info!(frame = n, draws, threshold = world_min_draws(),
            "the world's first frame (VICTORY_CAPTURE_WORLD counts from it)");
    }
    crate::texture::audit_zero_sources(n);
    // The harness may stop immediately after a requested world frame. Flush
    // view outcomes here as well as at the periodic host report, so missing
    // views in the last seconds are included in coverage.json.
    if WORLD.get().is_some() && crate::switch::on("VICTORY_COVERAGE") {
        for ((kind, title, requested, host, outcome), count) in
            crate::descriptor::host::take_view_coverage()
        {
            tracing::info!(frame = n, kind, title, requested, host, outcome, count,
                "texture view coverage");
        }
    }
}

/// Whether frame `n` is saved.
pub(crate) fn wanted(n: u64) -> bool {
    folder().is_some() && (n.is_power_of_two() || listed(n))
}

/// Pixel layout accepted by the presented-frame PNG writer. Target dumps
/// enter through their existing RGBA8 conversion and keep half-size output.
#[derive(Clone, Copy)]
enum PngPixel {
    Rgba8,
    Bgra8,
    Rgba16Float,
}

impl PngPixel {
    fn for_present_format(format: DXGI_FORMAT) -> Option<Self> {
        Some(match format {
            DXGI_FORMAT_R8G8B8A8_UNORM | DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => Self::Rgba8,
            DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => Self::Bgra8,
            DXGI_FORMAT_R16G16B16A16_FLOAT | DXGI_FORMAT_R16G16B16A16_TYPELESS => Self::Rgba16Float,
            _ => return None,
        })
    }

    fn bytes(self) -> usize {
        match self {
            Self::Rgba16Float => 8,
            _ => 4,
        }
    }

    fn rgb(self, pixel: &[u8]) -> [u8; 3] {
        match self {
            Self::Rgba8 => [pixel[0], pixel[1], pixel[2]],
            Self::Bgra8 => [pixel[2], pixel[1], pixel[0]],
            Self::Rgba16Float => [0, 2, 4].map(|at| {
                let value = half(u16::from_le_bytes([pixel[at], pixel[at + 1]]));
                (value.clamp(0.0, 1.0) * 255.0).round() as u8
            }),
        }
    }
}

/// Saves color rows as `<name>.png`. The full-resolution switch applies only
/// to presented frames; target dumps always pass `false`.
fn save_as(name: &str, width: u32, height: u32, pitch: usize,
    format: PngPixel, rows: &[u8], full_resolution: bool)
{
    let Some(folder) = folder() else { return };
    let Some(png) = encode_rgb_png(width, height, pitch, format, rows, full_resolution) else {
        tracing::warn!(width, height, pitch, bytes = rows.len(), "frame not saved: invalid color rows");
        return;
    };
    let path = folder.join(format!("{name}.png"));
    match std::fs::create_dir_all(folder).and_then(|()| std::fs::write(&path, png)) {
        Ok(()) => tracing::info!(path = %path.display(), "frame saved"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "frame not saved"),
    }
}

/// Encode padded readback rows without trusting row pitch or dimensions.
fn encode_rgb_png(width: u32, height: u32, pitch: usize,
    format: PngPixel, rows: &[u8], full_resolution: bool) -> Option<Vec<u8>>
{
    if width == 0 || height == 0 { return None }
    let bytes = format.bytes();
    let row_bytes = usize::try_from(width).ok()?.checked_mul(bytes)?;
    let source_len = usize::try_from(height - 1).ok()?.checked_mul(pitch)?.checked_add(row_bytes)?;
    if pitch < row_bytes || rows.len() < source_len { return None }
    let step = if full_resolution { 1usize } else { 2usize };
    let (w, h) = ((width / step as u32).max(1), (height / step as u32).max(1));
    let raw_len = usize::try_from(w).ok()?.checked_mul(3)?.checked_add(1)?
        .checked_mul(usize::try_from(h).ok()?)?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(raw_len).ok()?;
    for y in 0..h as usize {
        raw.push(0); // PNG filter None.
        let row = &rows[(step * y).min(height as usize - 1) * pitch..];
        for x in 0..w as usize {
            let start = (step * x).min(width as usize - 1) * bytes;
            raw.extend(format.rgb(&row[start..start + bytes]));
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
    Some(png)
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

/// Records a copy of `texture` (in the copy-source state) into a
/// readback buffer.
pub(crate) fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
) -> Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut rows = 0u32;
    let mut total = 0;
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
            None,
            Some(&mut total),
        );
    }
    // The reported total omits the last row's pitch padding. The copy reads it.
    let pitched = u64::from(footprint.Footprint.RowPitch)
        .saturating_mul(u64::from(rows.max(1)))
        .saturating_mul(u64::from(footprint.Footprint.Depth.max(1)));
    total = total.max(footprint.Offset.saturating_add(pitched));
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

/// Saves a supported color frame read back by [`read_back`].
pub(crate) fn save_read_back(
    frame: u64,
    buffer: &ID3D12Resource,
    footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    let f = footprint.Footprint;
    let Some(format) = PngPixel::for_present_format(f.Format) else {
        tracing::warn!(format = f.Format.0, "frame not saved: unsupported color format");
        return;
    };
    let pitch = f.RowPitch as usize;
    let Some(row_bytes) = (f.Width as usize).checked_mul(format.bytes()) else { return };
    let Some(len) = (f.Height as usize).checked_sub(1)
        .and_then(|last| last.checked_mul(pitch))
        .and_then(|prefix| prefix.checked_add(row_bytes))
    else { return };
    let Ok(offset) = usize::try_from(footprint.Offset) else { return };
    let Some(end) = offset.checked_add(len) else { return };
    // The copyable footprint may omit last-row padding. Read only actual
    // pixels, and never construct a slice beyond the readback resource.
    // SAFETY: the readback buffer remains live through this description query.
    let buffer_width = unsafe { buffer.GetDesc().Width };
    if pitch < row_bytes || end as u64 > buffer_width {
        tracing::warn!(width = f.Width, height = f.Height, pitch,
            "frame not saved: invalid readback footprint");
        return;
    }
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished, the checked interval lies in the readback
    // buffer, and Map covers it until Unmap below.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        if data.is_null() {
            buffer.Unmap(0, None);
            return;
        }
        let rows = std::slice::from_raw_parts(data.cast::<u8>().add(offset), len);
        save_as(&format!("frame-{frame}"), f.Width, f.Height, pitch, format,
            rows, crate::switch::on("VICTORY_CAPTURE_FULL_RES"));
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
    /// The first 348e UAV whose contents were copied before its dispatch.
    static LIT_OUTPUT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The f0c91 color UAV selected for the earlier world-lighting capture.
    static EARLY_LIT_OUTPUT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The 577 lighting target selected for an immediate pre/post pair.
    static LIT_577_OUTPUT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The format-10 UAV bound by the first 731a world composite dispatch.
    static LIT_731_OUTPUT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static WRITES_LIT_PATH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The submission currently running deferred readbacks on this thread.
    static SNAPSHOT_ORDER: std::cell::Cell<(u64, usize)> = const { std::cell::Cell::new((0, 0)) };
}

pub(crate) fn snapshot_order(submission: u64, list_index: usize) {
    SNAPSHOT_ORDER.with(|order| order.set((submission, list_index)));
}

static LIT_OUTPUT_PATH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static EARLY_LIT_TARGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static EARLY_LIT_POST_LIST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LIT_POST_LIST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LIT_577_POST_LIST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FFX_PRE_LIST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Names the pipeline whose views this thread binds next (empty: none).
pub(crate) fn binding(pipeline: &str) {
    if dump_on() || lit_inputs_on() || lit_output_on() || early_lit_on() || shader_577_inputs_on() || capture_731_on() {
        WRITER.with(|w| pipeline.clone_into(&mut w.borrow_mut()));
    }
    if !pipeline.is_empty() && lit_output_on() {
        LIT_OUTPUT.with(|o| o.set(0));
    }
    if !pipeline.is_empty() {
        EARLY_LIT_OUTPUT.with(|o| o.set(0));
        LIT_577_OUTPUT.with(|o| o.set(0));
        LIT_731_OUTPUT.with(|o| o.set(0));
    }
    WRITES_LIT_PATH.with(|v| v.set(false));
}

fn lit_output_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        folder().is_some()
            && std::env::var("VICTORY_CAPTURE_348E_OUTPUT").is_ok_and(|v| v == "1")
    })
}

/// An opt-in, one-frame diagnostic for the color target written before 348e.
fn early_lit_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && crate::switch::on("VICTORY_CAPTURE_EARLY_LIT"))
}

fn early_draws_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| early_lit_on() && crate::switch::on("VICTORY_CAPTURE_EARLY_DRAWS"))
}

fn early_draw_range_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| early_lit_on() && crate::switch::on("VICTORY_CAPTURE_EARLY_DRAW_RANGE"))
}

/// The world marker is set only when its frame is presented, after that
/// frame's 348e dispatch. A requested offset therefore starts at one.
fn capture_348e_frame(marker: Option<u64>, offset: Option<u64>, frame: u64) -> bool {
    match offset {
        None => true,
        Some(offset) => marker.and_then(|marker| marker.checked_add(offset))
            .is_some_and(|target| frame >= target),
    }
}

fn capture_348e_target() -> bool {
    static OFFSET: std::sync::OnceLock<Option<Option<u64>>> = std::sync::OnceLock::new();
    let setting = OFFSET.get_or_init(|| {
        std::env::var("VICTORY_CAPTURE_348E_OFFSET").ok()
            .map(|value| value.parse::<u64>().ok().filter(|&offset| (1..=120).contains(&offset)))
    });
    match setting {
        None => true, // Existing first-dispatch diagnostic.
        Some(Some(offset)) => capture_348e_frame(world_frame(), Some(*offset), crate::queue::presented() + 1),
        Some(None) => false,
    }
}

/// Copies the f10 UAV on the exact first 348e bind, before the dispatch
/// changes it. `readable` has already brought the texture to UAV state.
pub(crate) fn note_lit_output_pre(list: &crate::host::HostList, resource: crate::object::This) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TAKEN: AtomicBool = AtomicBool::new(false);
    if !lit_output_on() || !capture_348e_target() || resource.is_null() || crate::resource::title_format(resource) != Some(10) {
        return;
    }
    if !WRITER.with(|w| w.borrow().contains("348e1e8c414c7eca")) {
        return;
    }
    if TAKEN.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    if snapshot_lit_output(list, resource, "pre".into()) {
        LIT_OUTPUT.with(|o| o.set(resource as usize));
        LIT_OUTPUT_PATH.store(resource as usize, Ordering::Release);
        if let Some(marker) = world_frame() {
            let frame = crate::queue::presented() + 1;
            tracing::info!(frame, marker, actual_offset = frame.saturating_sub(marker),
                "348e pre/post capture selected relative to the world marker");
        }
    } else {
        TAKEN.store(false, Ordering::Release);
    }
}

/// Copies the same UAV immediately after that dispatch, before any later
/// pass can change it. The pair distinguishes inherited contents from 348e.
pub(crate) fn note_lit_output_post(list: &crate::host::HostList) {
    if !lit_output_on() {
        return;
    }
    let resource = LIT_OUTPUT.with(|o| o.replace(0));
    if resource != 0 {
        LIT_POST_LIST.store(list.id(), std::sync::atomic::Ordering::Release);
        let _ = snapshot_lit_output(list, resource as crate::object::This, "post".into());
    }
}

/// Capture the f0c91 color UAV before its first dispatch in the selected
/// world frame. This is the same 1280x720 title-memory range later read by
/// 348e. The view is recorded before the normal UAV transition, then its
/// previous state is restored by `snapshot_lit_output`.
fn note_early_lit_target(list: &crate::host::HostList, resource: crate::object::This) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TAKEN: AtomicBool = AtomicBool::new(false);
    if !early_lit_on() || world_frame().is_none() || !capture_348e_target()
        || crate::resource::title_format(resource) != Some(10)
        || !WRITER.with(|w| w.borrow().contains("f0c91b4d2578f5bf"))
        || TAKEN.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err()
    {
        return;
    }
    if snapshot_lit_output(list, resource, "early-pre".into()) {
        EARLY_LIT_OUTPUT.with(|o| o.set(resource as usize));
        EARLY_LIT_TARGET.store(resource as usize, Ordering::Release);
        tracing::info!(?resource, frame = crate::queue::presented() + 1,
            marker = ?world_frame(), "early lighting target selected");
    } else {
        TAKEN.store(false, Ordering::Release);
    }
}

/// Record the matching image immediately after f0c91, before any graphics
/// command in a later list can alter it.
fn note_early_lit_post(list: &crate::host::HostList) {
    use std::sync::atomic::Ordering;
    if !early_lit_on() {
        return;
    }
    let resource = EARLY_LIT_OUTPUT.with(|o| o.replace(0));
    if resource != 0 {
        EARLY_LIT_POST_LIST.store(list.id(), Ordering::Release);
        let _ = snapshot_lit_output(list, resource as crate::object::This, "early-post".into());
    }
}

fn snapshot_lit_output(
    list: &crate::host::HostList,
    resource: crate::object::This,
    phase: String,
) -> bool {
    struct Readback(ID3D12Resource);
    // SAFETY: D3D12 resource interfaces are free-threaded.
    unsafe impl Send for Readback {}
    let Some(copy) = crate::resource::host_copy(resource) else { return false };
    let Some(host) = crate::host::host() else { return false };
    // A placed R9 view may be backed by a paired R32_UINT texture. Read the
    // actual R9 resource that the shader samples, after its alias transition.
    let input_577 = phase.starts_with("577-input-");
    let input_731 = phase.starts_with("731-input-");
    let diagnostic_input = input_577 || input_731;
    let shared_exp = diagnostic_input
        && crate::resource::title_format(resource) == Some(67)
        && copy.shared_exp_resource().is_some();
    let texture = if shared_exp {
        copy.shared_exp_resource().unwrap()
    } else {
        copy.texture.clone()
    };
    if phase == "pre" || phase == "ffx-pre" {
        tracing::info!(?resource, host_copy = format_args!("{:p}", std::sync::Arc::as_ptr(&copy)),
            host_resource = format_args!("{:p}", windows::core::Interface::as_raw(&texture)),
            title_memory = ?crate::resource::texture_memory(resource).map(|(at, _)| at as usize),
            "lighting output capture identity");
    }
    // SAFETY: the host copy is live while `copy` is held.
    let d = unsafe { texture.GetDesc() };
    if diagnostic_input {
        tracing::info!(phase, title_format = ?crate::resource::title_format(resource),
            host_format = d.Format.0, shared_exp, separate = copy.shared_exp_separate(),
            "sampled source capture selected");
    }
    let format = d.Format.0 as u32;
    if (!diagnostic_input && format != 10)
        || (input_577 && !matches!(format, 24 | 26 | 28 | 41 | 67))
        || (input_731 && !matches!(format, 9 | 10 | 19 | 24 | 26 | 28 | 30 | 34 | 39 | 41 | 54 | 61 | 67 | 87))
        || d.SampleDesc.Count != 1
        || d.Width.saturating_mul(u64::from(d.Height)) > 2_000_000
    {
        return false;
    }
    let old_state = list.recorded_state(&copy);
    if shared_exp {
        let _ = list.use_shared_exp_state(&copy, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
    } else {
        crate::list::ensure_state(list, resource, D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32);
    }
    let read = read_back(&host.device, &list.list, &texture);
    if shared_exp {
        let _ = list.use_shared_exp_state(&copy, 0x40 | 0x80);
    } else {
        crate::list::ensure_state(list, resource, old_state);
    }
    let Some((buffer, footprint)) = read else { return false };
    list.keep(&texture);
    let buffer = Readback(buffer);
    let at = resource as usize;
    let logged_phase = phase.clone();
    list.defer_after(move || {
        let mut data = std::ptr::null_mut();
        // SAFETY: this callback runs after the containing list's GPU fence.
        unsafe {
            if buffer.0.Map(0, None, Some(&mut data)).is_err() {
                tracing::warn!(phase, resource = format_args!("{at:#x}"), "lighting output readback map failed");
                return;
            }
            let f = footprint.Footprint;
            let rows = std::slice::from_raw_parts(data.cast::<u8>(), f.RowPitch as usize * f.Height as usize);
            if let Some(rgba) = to_rgba(f.Format.0 as u32, f.Width as usize, f.Height as usize, f.RowPitch as usize, rows) {
                let frame = crate::queue::presented() + 1;
                let (submission, list_index) = SNAPSHOT_ORDER.with(std::cell::Cell::get);
                let name = if phase.starts_with("early-") {
                    format!("frame-{frame}-{phase}-s{submission}-l{list_index}-f{}-{at:#x}", f.Format.0)
                } else {
                    format!("frame-{frame}-348e-{phase}-s{submission}-l{list_index}-f{}-{at:#x}", f.Format.0)
                };
                save_as(&name, f.Width, f.Height, f.Width as usize * 4,
                    PngPixel::Rgba8, &rgba, false);
                tracing::info!(phase, frame, submission, list_index, resource = format_args!("{at:#x}"), "lighting output snapshot saved");
            }
            buffer.0.Unmap(0, None);
        }
    });
    tracing::info!(phase = logged_phase, ?resource, width = d.Width, height = d.Height, "lighting output snapshot recorded");
    true
}

/// Capture the exact color texture just before the first host FSR call.
pub(crate) fn snapshot_ffx_input_pre(list: &crate::host::HostList, resource: crate::object::This) {
    if crate::switch::on("VICTORY_CAPTURE_FFX") && crate::resource::title_format(resource) == Some(10) {
        FFX_PRE_LIST.store(list.id(), std::sync::atomic::Ordering::Release);
        let _ = snapshot_lit_output(list, resource, "ffx-pre".into());
    }
}

/// The first lighting pass and first FFX call may be recorded on different
/// threads, so find their order in the actual submission before inserting
/// bounded readbacks between lists. This is enabled only for a diagnostic run.
pub(crate) fn lit_ffx_checkpoint_range(runs: &[&crate::host::HostList]) -> Option<(usize, usize)> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TAKEN: AtomicBool = AtomicBool::new(false);
    if !crate::switch::on("VICTORY_TRACE_LIT_STAGES") || TAKEN.load(Ordering::Acquire) {
        return None;
    }
    let post = LIT_POST_LIST.load(Ordering::Acquire);
    let ffx = FFX_PRE_LIST.load(Ordering::Acquire);
    if post == 0 || ffx == 0 || lit_path_resource() == 0 {
        return None;
    }
    let start = runs.iter().position(|r| r.id() == post)?;
    let end = runs.iter().position(|r| r.id() == ffx)?;
    if start >= end || end - start > 64 || TAKEN.swap(true, Ordering::AcqRel) {
        return None;
    }
    tracing::info!(start, end, post, ffx, "lighting to FFX list checkpoints enabled");
    Some((start, end))
}

pub(crate) fn snapshot_lit_checkpoint(list: &crate::host::HostList, number: usize) {
    let resource = lit_path_resource() as crate::object::This;
    if !resource.is_null() {
        let _ = snapshot_lit_output(list, resource, format!("checkpoint{number}"));
    }
}

pub(crate) fn snapshot_lit_writer_checkpoint(list: &crate::host::HostList, number: usize) {
    let resource = lit_path_resource() as crate::object::This;
    if !resource.is_null() {
        let _ = snapshot_lit_output(list, resource, format!("writercheckpoint{number}"));
    }
}

/// In the submission containing the selected f0c91 dispatch, read the same
/// color texture after that list and each later list that binds it. This
/// brackets the large indirect graphics batches without a readback per draw.
pub(crate) fn early_lit_checkpoint_indices(runs: &[&crate::host::HostList]) -> Vec<(usize, usize)> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TAKEN: AtomicBool = AtomicBool::new(false);
    if !early_lit_on() || TAKEN.load(Ordering::Acquire) {
        return Vec::new();
    }
    let post = EARLY_LIT_POST_LIST.load(Ordering::Acquire);
    let target = EARLY_LIT_TARGET.load(Ordering::Acquire);
    if post == 0 || target == 0 {
        return Vec::new();
    }
    let Some(start) = runs.iter().position(|run| run.id() == post) else {
        return Vec::new();
    };
    if TAKEN.swap(true, Ordering::AcqRel) {
        return Vec::new();
    }
    let mut selected = Vec::new();
    for (index, run) in runs.iter().enumerate().skip(start) {
        if index == start || run.binds_target(target) {
            if selected.len() == 8 {
                tracing::warn!("early lighting list checkpoints capped at eight");
                break;
            }
            tracing::info!(index, list_id = run.id(), target = format_args!("{target:#x}"),
                checkpoint = selected.len(), "early lighting checkpoint scheduled");
            selected.push((index, selected.len()));
        }
    }
    selected
}

pub(crate) fn snapshot_early_lit_checkpoint(list: &crate::host::HostList, number: usize) {
    let resource = EARLY_LIT_TARGET.load(std::sync::atomic::Ordering::Acquire) as crate::object::This;
    if !resource.is_null() {
        let _ = snapshot_lit_output(list, resource, format!("early-list{number}"));
    }
}

/// A bounded set of GPU-order readbacks inside the first world color draw
/// list. The f0c91 target is selected before this list; its scene-color
/// writer's three vertex variants identify the indirect batch seen in the
/// preceding list checkpoints. The samples bracket 180 title draws without
/// omitting or changing any of them.
pub(crate) fn note_early_indirect_draw(
    list: &crate::host::HostList,
    target: crate::object::This,
    pipeline: crate::object::This,
) {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    static SELECTED_LIST: AtomicU64 = AtomicU64::new(0);
    static COUNT: AtomicU32 = AtomicU32::new(0);
    if !(early_draws_on() || early_draw_range_on()) || target.is_null() || !capture_348e_target()
        || EARLY_LIT_POST_LIST.load(Ordering::Acquire) == 0
        || target as usize != EARLY_LIT_TARGET.load(Ordering::Acquire)
    {
        return;
    }
    let name = crate::pipeline::shaders_of(pipeline);
    if !["25ebc92075b62822", "1d7b30217b395cd8", "72c739b61097456d"]
        .iter().any(|hash| name.contains(hash))
    {
        return;
    }
    let selected = SELECTED_LIST.load(Ordering::Acquire);
    if selected != list.id()
        && (selected != 0 || SELECTED_LIST.compare_exchange(0, list.id(), Ordering::AcqRel, Ordering::Acquire).is_err())
    {
        return;
    }
    let draw = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let focused = early_draw_range_on();
    if focused && (17..=32).contains(&draw) {
        tracing::info!(list_id = list.id(), draw, %name, resource = ?target,
            "early world color draw shader pair");
    }
    let samples: &[u32; 8] = if focused {
        &[17, 18, 20, 22, 24, 26, 28, 32]
    } else {
        &[1, 2, 4, 8, 16, 32, 64, 128]
    };
    if !samples.contains(&draw) {
        return;
    }
    let phase = format!("early-draw{draw}");
    if snapshot_lit_output(list, target, phase.clone()) {
        tracing::info!(list_id = list.id(), draw, %name, resource = ?target,
            "early world color draw checkpoint recorded");
    }
}

/// Insert a readback after the 577 resolve and after each later list known to
/// write its output, stopping before FFX reads it. Unlike the 348e checkpoint
/// range, this spans separate submissions. The execute lock serializes calls.
pub(crate) fn lit_writer_checkpoint_indices(runs: &[&crate::host::HostList]) -> Vec<(usize, usize)> {
    static STATE: std::sync::Mutex<(bool, usize, bool)> =
        std::sync::Mutex::new((false, 0, false));
    if !crate::switch::on("VICTORY_TRACE_LIT_WRITER_CHECKPOINTS") || lit_path_resource() == 0 {
        return Vec::new();
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.2 {
        return Vec::new();
    }
    let post = LIT_577_POST_LIST.load(std::sync::atomic::Ordering::Acquire);
    let ffx = FFX_PRE_LIST.load(std::sync::atomic::Ordering::Acquire);
    let mut selected = Vec::new();
    for (index, run) in runs.iter().enumerate() {
        if !state.0 && post != 0 && run.id() == post {
            state.0 = true;
            selected.push((index, state.1));
            state.1 += 1;
            continue;
        }
        if !state.0 {
            continue;
        }
        if ffx != 0 && run.id() == ffx {
            // Some lists were recorded before the 577 marker established the
            // tracked resource, so their writes have no lit_writes entry.
            // Check every intervening list in this final submission.
            let start = selected.last().map_or(0, |(last, _)| last + 1);
            for gap in start..index {
                if state.1 >= 48 {
                    tracing::warn!("lighting writer checkpoints capped at 48 before FFX");
                    break;
                }
                selected.push((gap, state.1));
                state.1 += 1;
            }
            state.2 = true;
            break;
        }
        if run.has_lit_write() {
            if state.1 >= 48 {
                tracing::warn!("lighting writer checkpoints capped at 48; trace may be incomplete");
                state.2 = true;
                break;
            }
            selected.push((index, state.1));
            state.1 += 1;
        }
    }
    // The FFX list may be submitted alone. Once a known writer appears after
    // the 577 baseline, also cover the rest of that submission, since lists
    // recorded before the marker can write the resource without a lit_writes
    // entry. The bounded budget keeps this from tracing every startup list.
    if !state.2 && ffx != 0 && state.1 >= 2
        && let Some(&(last, _)) = selected.last()
    {
        for gap in last + 1..runs.len() {
            if state.1 >= 48 {
                tracing::warn!("lighting writer checkpoints capped at 48 before FFX");
                state.2 = true;
                break;
            }
            selected.push((gap, state.1));
            state.1 += 1;
        }
    }
    for &(list_index, checkpoint) in &selected {
        tracing::info!(list_index, checkpoint, list_id = runs[list_index].id(),
            "lighting writer checkpoint scheduled after original list");
    }
    selected
}

fn shader_577_inputs_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && crate::switch::on("VICTORY_CAPTURE_577_INPUTS"))
}

/// The first frame that binds the world lighting resolve. The draw-count
/// marker can also fire in an intro scene, before these textures exist.
static LIT_FRAME: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

pub(crate) fn lit_frame() -> Option<u64> {
    LIT_FRAME.get().copied()
}

fn frame_577_target() -> bool {
    if !shader_577_inputs_on() || !WRITER.with(|w| w.borrow().contains("577248728544d74b")) {
        return false;
    }
    let first = *LIT_FRAME.get_or_init(|| {
        let frame = crate::queue::presented() + 1;
        tracing::info!(frame, "first world lighting resolve (577 capture offsets count from it)");
        frame
    });
    static TARGET_OFFSET: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let target_offset = *TARGET_OFFSET.get_or_init(|| std::env::var("VICTORY_CAPTURE_577_OFFSET")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    // The resolve may not run on every presented frame. Capture its first
    // use at or after the requested offset instead of missing it entirely.
    crate::queue::presented() + 1 >= first + target_offset
}

/// Paired readbacks of the exact 1280x720 color target around the 577 write.
pub(crate) fn note_577_output_pre(list: &crate::host::HostList, resource: crate::object::This, slot: u8, offset: u32) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TAKEN: AtomicBool = AtomicBool::new(false);
    if !frame_577_target() || slot != 1 || offset != 48 || crate::resource::title_format(resource) != Some(10)
        || TAKEN.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err()
    {
        return;
    }
    if snapshot_lit_output(list, resource, "577-pre".into()) {
        LIT_577_OUTPUT.with(|o| o.set(resource as usize));
        if trace_lit_writers_only_on() {
            LIT_OUTPUT_PATH.store(resource as usize, Ordering::Release);
        }
    } else {
        TAKEN.store(false, Ordering::Release);
    }
}

pub(crate) fn note_577_output_post(list: &crate::host::HostList) {
    let resource = LIT_577_OUTPUT.with(|o| o.replace(0));
    if resource != 0 {
        LIT_577_POST_LIST.store(list.id(), std::sync::atomic::Ordering::Release);
        let _ = snapshot_lit_output(list, resource as crate::object::This, "577-post".into());
    }
}

fn capture_731_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| folder().is_some() && crate::switch::on("VICTORY_CAPTURE_731"))
}

/// Capture the first world composite dispatch on its exact format-10 UAV.
pub(crate) fn note_731_output_pre(list: &crate::host::HostList, pipeline: &str, work: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TAKEN: AtomicBool = AtomicBool::new(false);
    if !capture_731_on() || !pipeline.contains("731a39171b568298")
        || TAKEN.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err()
    {
        return;
    }
    let resource = LIT_731_OUTPUT.with(std::cell::Cell::get) as crate::object::This;
    if resource.is_null() || !snapshot_lit_output(list, resource, "731-pre".into()) {
        TAKEN.store(false, Ordering::Release);
        return;
    }
    tracing::info!(?resource, work, "731 world composite dispatch captured");
}

pub(crate) fn note_731_output_post(list: &crate::host::HostList, pipeline: &str) {
    if !capture_731_on() || !pipeline.contains("731a39171b568298") {
        return;
    }
    let resource = LIT_731_OUTPUT.with(|o| o.replace(0));
    if resource != 0 {
        let _ = snapshot_lit_output(list, resource as crate::object::This, "731-post".into());
    }
}

/// Shader 731's texture sources on its first tiled write, including placed
/// views resolved by the normal descriptor path. The readback is one time.
pub(crate) fn note_731_input(
    list: &crate::host::HostList,
    resource: crate::object::This,
    slot: u8,
    offset: u32,
) {
    if !capture_731_on() || !matches!((slot, offset),
        (0, 72 | 120 | 480 | 568) | (1, 8 | 16 | 24 | 32 | 40 | 48 | 56 | 64 | 72)) {
        return;
    }
    let _ = snapshot_lit_output(list, resource, format!("731-input-s{slot}o{offset}"));
}

/// Read the small color sources at the first faulty 577 resolve before its
/// dispatch. Each slot is captured once and the total is below 24 MiB.
pub(crate) fn note_577_input(
    list: &crate::host::HostList,
    resource: crate::object::This,
    slot: u8,
    offset: u32,
) {
    if !frame_577_target() {
        return;
    }
    if !matches!((slot, offset), (0, 552) | (1, 16 | 24 | 32)) {
        return;
    }
    static SEEN: std::sync::Mutex<Vec<(u8, u32)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.contains(&(slot, offset)) {
        return;
    }
    if snapshot_lit_output(list, resource, format!("577-input-s{slot}o{offset}")) {
        seen.push((slot, offset));
    }
}

fn trace_lit_path_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_TRACE_LIT_WRITES"))
}

pub(crate) fn trace_lit_writers_only_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_TRACE_LIT_WRITERS_ONLY"))
}

/// Include the first few submissions after the lighting resource is known
/// in the command trace, even before the title's world frame is numbered.
pub(crate) fn trace_lit_submission(submission: u64) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    if !trace_lit_path_on() || trace_lit_writers_only_on() || lit_path_resource() == 0 {
        return false;
    }
    static FIRST: AtomicU64 = AtomicU64::new(u64::MAX);
    let first = match FIRST.compare_exchange(u64::MAX, submission, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => submission,
        Err(first) => first,
    };
    submission < first.saturating_add(12)
}

pub(crate) fn after_lit_path_copy(
    list: &crate::host::HostList,
    destination: crate::object::This,
    source: crate::object::This,
) {
    if !trace_lit_path_on() || !lit_target_alias(destination as usize) {
        return;
    }
    tracing::info!(?destination, ?source, "copy writes lighting target or title-memory alias");
    after_lit_path_write(list, true, std::ptr::null_mut());
}

/// Snapshot early writes and logarithmically spaced later writes to the first lighting output.
/// Each snapshot is recorded immediately after its draw or dispatch, before
/// a later title command can overwrite the texture in the same list.
pub(crate) fn after_lit_path_write(
    list: &crate::host::HostList,
    bound_target: bool,
    pipeline: crate::object::This,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    note_early_lit_post(list);
    if !trace_lit_path_on() || !(bound_target || WRITES_LIT_PATH.with(|v| v.get())) {
        return;
    }
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let resource = LIT_OUTPUT_PATH.load(Ordering::Acquire) as crate::object::This;
    if resource.is_null() {
        if count == 1 {
            tracing::warn!("lighting write trace has no tracked output resource");
        }
        return;
    }
    let pipeline = if pipeline.is_null() {
        "copy".to_owned()
    } else {
        crate::pipeline::host_pipeline(pipeline)
            .map_or_else(|| crate::pipeline::shaders_of(pipeline), |p| p.name.clone())
    };
    if trace_lit_writers_only_on() {
        list.note_lit_write(resource as usize, pipeline);
        return;
    }
    if count <= 64 {
        tracing::info!(count, pipeline, ?resource, "lighting output write after first sky pass");
    }
    if count <= 20 || count.is_power_of_two() && count <= 2048 || (33..=48).contains(&count) {
        let _ = snapshot_lit_output(list, resource, format!("write{count}"));
    }
}

/// The first lighting output's title resource, for tracking draws to an RTV
/// that remains bound across commands without another descriptor bind.
pub(crate) fn lit_path_resource() -> usize {
    LIT_OUTPUT_PATH.load(std::sync::atomic::Ordering::Acquire)
}

/// Resource objects with the same title memory start can alias the lighting
/// target even when their title pointers differ. Match these writes too.
pub(crate) fn lit_target_alias(resource: usize) -> bool {
    let tracked = lit_path_resource();
    if tracked == 0 || resource == 0 {
        return false;
    }
    if tracked == resource {
        return true;
    }
    let Some((tracked_at, _)) = crate::resource::texture_memory(tracked as crate::object::This) else {
        return false;
    };
    let Some((candidate_at, _)) = crate::resource::texture_memory(resource as crate::object::This) else {
        return false;
    };
    if tracked_at != candidate_at {
        return false;
    }
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<usize>>> =
        std::sync::OnceLock::new();
    let mut seen = SEEN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() < 32 && seen.insert(resource) {
        let same_host_copy = crate::resource::made_copy(tracked as crate::object::This)
            .zip(crate::resource::made_copy(resource as crate::object::This))
            .is_some_and(|(a, b)| std::sync::Arc::ptr_eq(&a, &b));
        tracing::info!(tracked = format_args!("{tracked:#x}"),
            alias = format_args!("{resource:#x}"),
            address = format_args!("{:#x}", tracked_at as usize),
            same_host_copy, "lighting target title-memory alias");
    }
    true
}

pub(crate) fn lit_inputs_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        folder().is_some()
            && std::env::var("VICTORY_CAPTURE_348E_INPUTS").is_ok_and(|v| v == "1")
    })
}

pub(crate) fn lit_input_target(by: &str) -> bool {
    by.starts_with("348e-input-")
}

pub(crate) fn lit_packed_writer_target(by: &str) -> bool {
    by.contains("26b8bbb2e7ae4dc6")
}

/// Capture only the three small, non-BC texture inputs to the selected
/// 348e lighting dispatch. The two BC inputs have a separate bounded
/// detile/GPU-readback diagnostic. The list marker triggers a snapshot
/// immediately after this list's submission, before later passes change
/// the depth or packed lighting inputs.
pub(crate) fn note_lit_input(
    list: &crate::host::HostList,
    resource: crate::object::This,
    slot: u8,
    offset: u32,
) {
    if !lit_inputs_on() || !capture_348e_target() || slot != 1 || !matches!(offset, 40 | 48 | 64) {
        return;
    }
    let lighting = WRITER.with(|w| w.borrow().contains("348e1e8c414c7eca"));
    if !lighting {
        return;
    }
    let Some(format) = crate::resource::title_format(resource) else { return };
    let expected = match offset {
        40 | 48 => format == 67,
        64 => format == 19,
        _ => false,
    };
    if !expected {
        return;
    }
    let Some(copy) = crate::resource::host_copy(resource) else { return };
    // SAFETY: the host copy remains alive while `copy` is held.
    let host_desc = unsafe { copy.texture.GetDesc() };
    // Bound the mip-zero readbacks for these three sources. They fit well
    // below 64 MiB even at the title's larger resolution.
    let bytes = host_desc.Width.saturating_mul(u64::from(host_desc.Height))
        .saturating_mul(if format == 19 { 8 } else { 4 });
    if bytes > 64 << 20 {
        return;
    }
    static SEEN: std::sync::OnceLock<std::sync::Mutex<(std::collections::HashSet<u32>, u64)>> =
        std::sync::OnceLock::new();
    let mut seen = SEEN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if seen.0.contains(&offset) || seen.0.len() >= 3 || seen.1.saturating_add(bytes) > 64 << 20 {
        return;
    }
    seen.0.insert(offset);
    seen.1 += bytes;
    drop(seen);
    if matches!(offset, 40 | 48) {
        crate::resource::log_lit_packed_peers(resource);
    }
    let label = format!("348e-input-s{slot}o{offset}-f{format}");
    list.note_target(resource as usize, &label);
    tracing::info!(slot, offset, format, bytes, ?resource, "first 348e texture input queued for post-submission capture");
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
    if resource.is_null() {
        return;
    }
    if early_lit_on() && crate::resource::title_format(resource) == Some(10) {
        // Later lists may have been recorded on other threads before this
        // target was selected. Retain all format-10 bindings until GPU order
        // is known at submission time.
        let by = WRITER.with(|w| w.borrow().clone());
        list.note_target(resource as usize, &by);
        note_early_lit_target(list, resource);
    }
    if trace_lit_path_on() && lit_target_alias(resource as usize) {
        WRITES_LIT_PATH.with(|v| v.set(true));
    }
    if crate::switch::on("VICTORY_TRACE_LIT_WRITER_CHECKPOINTS")
        && crate::resource::title_format(resource) == Some(10)
    {
        let by = WRITER.with(|w| w.borrow().clone());
        list.note_target(resource as usize, &by);
    }
    if capture_731_on() && crate::resource::title_format(resource) == Some(10)
        && WRITER.with(|w| w.borrow().contains("731a39171b568298"))
    {
        LIT_731_OUTPUT.with(|o| o.set(resource as usize));
    }
    if !dump_on() {
        return;
    }
    let by = WRITER.with(|w| w.borrow().clone());
    if lit_inputs_on() {
        crate::resource::log_lit_packed_writer(resource, &by);
    }
    // Lists are recorded ahead of the frame that submits them.
    list.note_target(resource as usize, &by);
    if !listed(crate::queue::presented() + 1)
        || crate::switch::on("VICTORY_DUMP_WRITERS_ONLY")
    {
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

#[cfg(feature = "diagnostics")]
pub(crate) fn ffx_target(by: &str) -> bool {
    crate::switch::on("VICTORY_CAPTURE_FFX") && by.starts_with("ffx-")
}

#[cfg(feature = "diagnostics")]
static FFX_OUTPUT_ADDRESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
static FFX_INPUT_COPY: std::sync::Mutex<Option<(usize, std::sync::Arc<crate::resource::HostTexture>)>> =
    std::sync::Mutex::new(None);

#[cfg(feature = "diagnostics")]
pub(crate) fn retain_ffx_input(resource: crate::object::This) {
    if !crate::switch::on("VICTORY_CAPTURE_FFX") {
        return;
    }
    if let Some(copy) = crate::resource::host_copy(resource) {
        tracing::info!(?resource, host_copy = format_args!("{:p}", std::sync::Arc::as_ptr(&copy)),
            host_resource = format_args!("{:p}", windows::core::Interface::as_raw(&copy.texture)),
            title_memory = ?crate::resource::texture_memory(resource).map(|(at, _)| at as usize),
            "FidelityFX input capture identity");
        *FFX_INPUT_COPY.lock().unwrap_or_else(|e| e.into_inner()) = Some((resource as usize, copy));
    } else {
        tracing::warn!(?resource, "FidelityFX input has no host copy for capture");
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn mark_ffx_output(resource: crate::object::This) {
    if !crate::switch::on("VICTORY_CAPTURE_FFX") {
        return;
    }
    if let Some((address, _)) = crate::resource::texture_memory(resource) {
        FFX_OUTPUT_ADDRESS.store(address as u64, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(address = format_args!("{:#x}", address as usize), "FidelityFX output title address tagged");
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn ffx_read_target(resource: crate::object::This) -> bool {
    use std::sync::atomic::Ordering;
    let address = FFX_OUTPUT_ADDRESS.load(Ordering::Acquire);
    address != 0
        && crate::resource::texture_memory(resource).is_some_and(|(at, _)| at as u64 == address)
        && FFX_OUTPUT_ADDRESS
            .compare_exchange(address, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
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
        let copy = crate::resource::host_copy(*resource as crate::object::This)
            .or_else(|| (by == "ffx-color-input").then(|| {
                FFX_INPUT_COPY.lock().unwrap_or_else(|e| e.into_inner())
                    .as_ref().filter(|(at, _)| *at == *resource).map(|(_, copy)| copy.clone())
            }).flatten());
        let Some(copy) = copy else {
            if by.starts_with("ffx-") {
                tracing::warn!(resource = format_args!("{resource:#x}"), by, "FidelityFX target has no host copy for capture");
            }
            continue;
        };
        if by == "ffx-color-input" {
            tracing::info!(resource = format_args!("{resource:#x}"),
                host_copy = format_args!("{:p}", std::sync::Arc::as_ptr(&copy)),
                host_resource = format_args!("{:p}", windows::core::Interface::as_raw(&copy.texture)),
                "FidelityFX input dump identity");
        }
        let alias = (lit_input_target(by) && by.ends_with("-f67"))
            || (by == "ffx-packed-read" && copy.shared_exp_resource().is_some())
            || (lit_packed_writer_target(by) && copy.alias_owner());
        if alias && !copy.alias_owner() {
            tracing::warn!(?resource, by, "packed lighting input capture skipped: alias not active");
            continue;
        }
        let texture = if alias {
            copy.shared_exp_resource().unwrap_or_else(|| copy.texture.clone())
        } else {
            copy.texture.clone()
        };
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        if d.SampleDesc.Count > 1 || d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            continue;
        }
        let state = if alias { copy.alias_state() } else { copy.state.load(Relaxed) };
        let target = (i, *resource as crate::object::This, by.as_str());
        dump_one(host, &texture, state, &format!("frame-{frame}-s{sub:04}"), target, &d);
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
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy finished; the mapping spans the footprint's rows.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() {
            return;
        }
        let pitch = f.RowPitch as usize;
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * f.Height as usize);
        if crate::switch::on("VICTORY_DUMP_PACKED_SKY_RAW")
            && d.Width == 192 && d.Height == 108
            && ((d.Format.0 == 42 && by.contains("26b8bbb2e7ae4dc6"))
                || (d.Format.0 == 67 && (by.starts_with("348e-input-")
                    || by.contains("26b8bbb2e7ae4dc6"))))
            && let Some(folder) = folder()
        {
            let mut packed = Vec::with_capacity(192 * 108 * 4);
            for y in 0..108usize {
                packed.extend_from_slice(&rows[y * pitch..y * pitch + 192 * 4]);
            }
            let path = folder.join(format!("{prefix}-{i:02}-{resource:?}-f{}-raw.bin", d.Format.0));
            let title_address = crate::resource::texture_memory(resource).map(|(p, _)| p as u64);
            match std::fs::write(&path, &packed) {
                Ok(()) => tracing::info!(path = %path.display(), title_address = ?title_address,
                    format = d.Format.0, bytes = packed.len(), "packed sky raw readback saved"),
                Err(e) => tracing::warn!(error = %e, path = %path.display(), "packed sky raw readback not saved"),
            }
        }
        if crate::switch::on("VICTORY_CAPTURE_FFX")
            && d.Width == 1920 && d.Height == 1080
            && ((d.Format.0 == 42 && by == "ffx-output")
                || (d.Format.0 == 67 && by == "ffx-packed-read"))
            && let Some(folder) = folder()
        {
            let mut packed = Vec::with_capacity(1920 * 1080 * 4);
            for y in 0..1080usize {
                packed.extend_from_slice(&rows[y * pitch..y * pitch + 1920 * 4]);
            }
            let path = folder.join(format!("{prefix}-{i:02}-{resource:?}-f{}-raw.bin", d.Format.0));
            match std::fs::write(&path, &packed) {
                Ok(()) => tracing::info!(path = %path.display(), format = d.Format.0, bytes = packed.len(), "FidelityFX output/read raw readback saved"),
                Err(e) => tracing::warn!(error = %e, path = %path.display(), "FidelityFX raw readback not saved"),
            }
        }
        // Tiny targets (exposure, averages): their values as they are.
        if f.Width * f.Height <= 16 {
            let texel = crate::resource::element(f.Format.0 as u32).map_or(4, |(b, _)| b as usize);
            let words: Vec<String> = (0..f.Height as usize)
                .flat_map(|y| rows[y * pitch..][..f.Width as usize * texel].chunks(4))
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
            save_as(&name, f.Width, f.Height, f.Width as usize * 4,
                PngPixel::Rgba8, &rgba, false);
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

fn shared_exp(v: u32) -> [f32; 3] {
    let scale = 2f32.powi(((v >> 27) & 31) as i32 - 24);
    [
        (v & 511) as f32 * scale,
        ((v >> 9) & 511) as f32 * scale,
        ((v >> 18) & 511) as f32 * scale,
    ]
}

/// Rows of a host format as 8-bit RGBA: 8-bit color as is, everything else
/// as channel values scaled so the largest finite one is white (single
/// channels: lowest black, highest white, as grey). `None` for formats not
/// handled.
fn to_rgba(format: u32, w: usize, h: usize, pitch: usize, rows: &[u8]) -> Option<Vec<u8>> {
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
        67 => (4, Box::new(move |b| shared_exp(u32_at(b, 0)))),
        9..=10 => (8, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), half(u16_at(b, 2))])),
        11..=14 => (8, Box::new(move |b| [0, 1, 2].map(|i| f32::from(u16_at(b, i))))),
        33..=34 => (4, Box::new(move |b| [half(u16_at(b, 0)), half(u16_at(b, 1)), 0.0])),
        1..=2 => (16, Box::new(move |b| {
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
    fn sky_capture_offset_waits_for_the_first_eligible_dispatch() {
        use super::capture_348e_frame;

        assert!(capture_348e_frame(None, None, 10)); // legacy first-dispatch mode
        assert!(!capture_348e_frame(None, Some(1), 10));
        assert!(!capture_348e_frame(Some(10), Some(1), 10));
        assert!(capture_348e_frame(Some(10), Some(1), 11));
        assert!(capture_348e_frame(Some(10), Some(1), 12));
        assert!(!capture_348e_frame(Some(10), Some(3), 12));
        assert!(capture_348e_frame(Some(10), Some(3), 13));
        assert!(!capture_348e_frame(Some(u64::MAX), Some(1), u64::MAX));
    }

    #[test]
    fn shared_exp_input_capture_decodes_packed_rgb() {
        use super::shared_exp;

        assert_eq!(shared_exp(0), [0.0; 3]);
        let packed = (16 << 27) | 256 | (128 << 9) | (64 << 18);
        assert_eq!(shared_exp(packed), [1.0, 0.5, 0.25]);
    }

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

#[cfg(test)]
mod frame_png_tests {
    use super::{encode_rgb_png, PngPixel, DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R16G16B16A16_TYPELESS};

    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        let len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let rows = miniz_oxide::inflate::decompress_to_vec_zlib(&png[41..41 + len]).unwrap();
        (width, height, rows)
    }

    #[test]
    fn full_resolution_retains_one_pixel_features_and_ignores_padding() {
        let mut rgba = [0u8; 72];
        for row in rgba.as_chunks_mut::<24>().0 {
            row[20..].fill(241);
        }
        rgba[28..32].copy_from_slice(&[171, 63, 29, 11]);
        rgba[64..68].copy_from_slice(&[91, 10, 221, 17]);
        let mut bgra = rgba;
        for row in bgra.as_chunks_mut::<24>().0 {
            for pixel in row[..20].as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
        }

        for (rows, format) in [(&rgba, PngPixel::Rgba8), (&bgra, PngPixel::Bgra8)] {
            let thumbnail = encode_rgb_png(5, 3, 24, format, rows, false).unwrap();
            assert_eq!(decode(&thumbnail), (2, 1, vec![0; 7]));
            let full = encode_rgb_png(5, 3, 24, format, rows, true).unwrap();
            let mut expected = vec![0; 48];
            expected[20..23].copy_from_slice(&[171, 63, 29]);
            expected[45..48].copy_from_slice(&[91, 10, 221]);
            assert_eq!(decode(&full), (5, 3, expected));
        }
    }

    #[test]
    fn half_float_presentation_uses_a_fixed_scale() {
        let pitch = 48;
        let mut rows = vec![0xcc; 3 * pitch + 32];
        for (x, y, rgba) in [
            (0, 0, [0, 0x3800, 0x3c00, 0x4400]),
            (2, 0, [0x3e00, 0xb400, 0x3400, 0]),
            (0, 2, [0x7e00, 0, 0xfc00, 0x3c00]),
            (2, 2, [0x3800, 0x3400, 0x3a00, 0]),
        ] {
            let start = y * pitch + x * 8;
            let pixel = rgba.map(u16::to_le_bytes).concat();
            rows[start..start + 8].copy_from_slice(&pixel);
        }
        for dxgi in [DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16B16A16_TYPELESS] {
            let format = PngPixel::for_present_format(dxgi).unwrap();
            let png = encode_rgb_png(4, 4, pitch, format, &rows, false).unwrap();
            assert_eq!(decode(&png), (2, 2,
                vec![0, 0, 128, 255, 255, 0, 64, 0, 0, 0, 0, 128, 64, 191]));
        }
    }

    #[test]
    fn invalid_readback_rows_are_rejected() {
        assert!(encode_rgb_png(0, 1, 4, PngPixel::Rgba8, &[0; 4], false).is_none());
        assert!(encode_rgb_png(1, 0, 4, PngPixel::Rgba8, &[0; 4], false).is_none());
        assert!(encode_rgb_png(2, 1, 4, PngPixel::Rgba8, &[0; 8], false).is_none());
        assert!(encode_rgb_png(2, 2, 8, PngPixel::Rgba8, &[0; 15], true).is_none());
    }
}
