//! Depth textures the title also writes through unordered access (decision
//! 0007, "One host texture per texture memory"). The console allows both
//! uses of one texture (Grand Theft Auto VI's shadow-map copies and its
//! material depth, written by compute and then tested as depth); the host
//! does not: a depth-stencil resource takes no unordered access, in any
//! format. Such a texture gets a writable twin, a colour texture per plane
//! (depth, stencil) that its unordered access views write, and the newer
//! contents move between the two by copies where the use changes: within
//! a list where it records the change (`HostList::transition`), and at
//! submission for each list's first use, from the side the lists before it
//! left newer. A 16-bit depth plane (`118..=120`, whose host depth is
//! 32-bit floats) or a 24-bit depth plane (`44..=47`, whose twin plane 0
//! takes `R32_FLOAT` / `R32_UINT` for the host's 24-bit depth) is converted
//! on the way, so depth written through unordered access matches the same
//! depth rasterised. Only planes that have received an unordered access view
//! are transitioned and copied, and an unseeded plane is never copied back
//! over the depth texture.
//!
//! shortcut: uses the lists do not name (shaders indexing the heap
//! themselves) move nothing; active twin planes are copied whole for any
//! use of either plane, also when the next pass overwrites all of them.
//! Upgrade when a frame trace shows such a use, or the copies (counted on
//! the frame-rate line) cost.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::{HostList, host, transition};
use crate::object::This;
use crate::resource::HostTexture;

/// Which side holds the newer contents.
pub(crate) const BOTH: u8 = 0;
pub(crate) const DEPTH: u8 = 1;
pub(crate) const TWIN: u8 = 2;

const RENDER_TARGET: u32 = 0x4;
const UNORDERED_ACCESS: u32 = 0x8;
const DEPTH_WRITE: u32 = 0x10;
const COPY_DEST: u32 = 0x400;
const COPY_SOURCE: u32 = 0x800;
const RESOLVE_DEST: u32 = 0x1000;

/// Copies between a depth texture and its twin since the last frame-rate
/// line (either way, within lists and at submission).
pub(crate) static COPIES: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Twin {
    /// The colour texture of each plane the depth texture has (depth, then
    /// stencil), shader-writable.
    pub(crate) planes: Vec<Arc<HostTexture>>,
    /// Per-plane newer side (`BOTH`, `DEPTH`, or `TWIN`) as of submitted lists.
    plane_newer: [AtomicU8; 2],
    /// Bitmask of planes that have received a UAV (`1 << plane`).
    uav_planes: AtomicU8,
    /// Per plane and subresource: how its bytes go through `scratch`.
    routes: Vec<Route>,
    /// A shader-writable buffer the copies go through, in the common state
    /// between them.
    scratch: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded; the rest is atomics and data.
unsafe impl Send for Twin {}
// SAFETY: as above.
unsafe impl Sync for Twin {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Conversion {
    None,
    U16ToF32,
    F32ToD24,
}

/// One subresource of one plane.
struct Route {
    plane: usize,
    /// Its index in the twin's plane texture and in the depth texture.
    twin_sub: u32,
    depth_sub: u32,
    /// Its place in the scratch buffer as the twin's texture has it.
    twin: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    /// As the depth texture has it, where its footprint differs from `twin`
    /// (`U16ToF32`: 16-bit twin in `R16` vs 32-bit float host depth in
    /// `R32`). `None` when both sides share `twin`'s footprint (`None` or
    /// in-place `F32ToD24`).
    depth: Option<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    conversion: Conversion,
}

/// A depth texture's use in one list: per-plane sides its first use needs
/// (`0` when a plane has not been touched in this list) and the side the list
/// left newer (`None`: unchanged from where it started).
pub(crate) struct Use {
    first: [u8; 2],
    last: [Option<u8>; 2],
    depth: Arc<HostTexture>,
}

/// The twin's plane formats for a console depth format (resource or
/// view format): 16- or 32-bit depth (and `R32` for 24-bit depth, whose
/// twin plane 0 takes `R32_FLOAT` / `R32_UINT`), then 8-bit stencil if it
/// has one.
fn plane_formats(console: u32) -> Option<&'static [u32]> {
    const R16: u32 = 53;
    const R32: u32 = 39;
    const R8: u32 = 60;
    match console {
        53 | 55 => Some(&[R16]),
        39..=41 => Some(&[R32]),
        118..=120 => Some(&[R16, R8]),
        44..=47 | 19..=22 => Some(&[R32, R8]),
        _ => None,
    }
}

/// The twin of the depth texture `depth`, whose title resource has format
/// `console`; `None` if the host makes none.
fn make(console: u32, depth: &ID3D12Resource) -> Option<Twin> {
    let host = host()?;
    let Some(formats) = plane_formats(console) else {
        once(console, 0, 0, "a depth texture of this format gets no writable twin: its unordered writes are dropped");
        return None;
    };
    // SAFETY: a live resource.
    let dd = unsafe { depth.GetDesc() };
    let subs = u32::from(dd.MipLevels) * u32::from(dd.DepthOrArraySize).max(1);
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut planes = Vec::new();
    for &f in formats {
        let desc = D3D12_RESOURCE_DESC {
            Format: DXGI_FORMAT(f as i32),
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Alignment: 0,
            ..dd
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creation; our out pointer.
        let made = unsafe {
            host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut texture)
        };
        if let Err(e) = made {
            tracing::warn!(error = %e, ?desc, "no writable twin for a depth texture");
            return None;
        }
        let texture = texture?;
        crate::host::name(&texture, || format!("depth twin {}x{} f{f}", dd.Width, dd.Height));
        planes.push(Arc::new(HostTexture::new(texture, 0, Vec::new())));
    }
    // The depth texture's footprints, every plane, and the twin's.
    let n = subs * planes.len() as u32;
    let mut df = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n as usize];
    let mut drows = vec![0u32; n as usize];
    // SAFETY: outputs sized for `n` subresources.
    unsafe { host.device.GetCopyableFootprints(&dd, 0, n, 0, Some(df.as_mut_ptr()), Some(drows.as_mut_ptr()), None, None) };
    let mut routes = Vec::new();
    let mut at = 0u64;
    let mut place = |mut f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT, rows: u32| {
        f.Offset = at;
        let bytes = u64::from(f.Footprint.RowPitch) * u64::from(rows) * u64::from(f.Footprint.Depth);
        at = (at + bytes).next_multiple_of(512);
        f
    };
    for (p, plane) in planes.iter().enumerate() {
        // SAFETY: a live resource.
        let td = unsafe { plane.texture.GetDesc() };
        let conversion = match (console, p) {
            (118..=120, 0) => Conversion::U16ToF32,
            (44..=47, 0) => Conversion::F32ToD24,
            _ => Conversion::None,
        };
        let (tf, trows) = if conversion == Conversion::U16ToF32 {
            let mut tf = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); subs as usize];
            let mut trows = vec![0u32; subs as usize];
            // SAFETY: outputs sized for `subs` subresources.
            unsafe {
                host.device.GetCopyableFootprints(
                    &td,
                    0,
                    subs,
                    0,
                    Some(tf.as_mut_ptr()),
                    Some(trows.as_mut_ptr()),
                    None,
                    None,
                )
            };
            (tf, trows)
        } else {
            (Vec::new(), Vec::new())
        };
        for s in 0..subs {
            let d = (p as u32 * subs + s) as usize;
            let valid = match conversion {
                Conversion::None => df[d].Footprint.Format == td.Format,
                Conversion::U16ToF32 => {
                    td.Format == DXGI_FORMAT_R16_TYPELESS && df[d].Footprint.Format == DXGI_FORMAT_R32_TYPELESS
                }
                Conversion::F32ToD24 => {
                    td.Format == DXGI_FORMAT_R32_TYPELESS && df[d].Footprint.Format == DXGI_FORMAT_R32_TYPELESS
                }
            };
            if !valid {
                once(console, td.Format.0 as u32, df[d].Footprint.Format.0 as u32, "a depth twin plane with no conversion to the host's: not made");
                return None;
            }
            // Same bytes or in-place conversion (`F32ToD24`): both sides copy
            // through the depth texture's footprint; `U16ToF32` uses separate
            // footprints and converts between them.
            let route = match conversion {
                Conversion::None | Conversion::F32ToD24 => {
                    Route { plane: p, twin_sub: s, depth_sub: d as u32, twin: place(df[d], drows[d]), depth: None, conversion }
                }
                Conversion::U16ToF32 => {
                    let twin = place(tf[s as usize], trows[s as usize]);
                    Route { plane: p, twin_sub: s, depth_sub: d as u32, twin, depth: Some(place(df[d], drows[d])), conversion }
                }
            };
            routes.push(route);
        }
    }
    let scratch = shared_scratch(at)?;
    tracing::debug!(console, width = dd.Width, height = dd.Height, planes = planes.len(), "a writable twin for a depth texture");
    Some(Twin {
        planes,
        plane_newer: [AtomicU8::new(DEPTH), AtomicU8::new(DEPTH)],
        uav_planes: AtomicU8::new(0),
        routes,
        scratch,
    })
}

/// Scratch buffers shared by the twins, one per power-of-two size: a
/// buffer each held as much video memory as its twin's planes for good
/// (about 325 MB for Grand Theft Auto VI's 315 twins in the menu).
/// Sharing is safe: every copy through one ([`Twin::copy`]) starts in the
/// common state and returns the buffer to it, so copies in one list are
/// ordered by those transitions, and lists run one after another on the
/// host's single queue.
struct Scratch(Vec<(u64, ID3D12Resource)>);

// SAFETY: D3D12 resources are free-threaded.
unsafe impl Send for Scratch {}

static SCRATCH: std::sync::Mutex<Scratch> = std::sync::Mutex::new(Scratch(Vec::new()));

/// The shared scratch buffer of at least `size` bytes: the one of `size`'s
/// power-of-two class, made on first use. All of them together hold at
/// most twice the largest.
fn shared_scratch(size: u64) -> Option<ID3D12Resource> {
    let class = size.max(1 << 16).next_power_of_two();
    let mut all = SCRATCH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, buffer)) = all.0.iter().find(|(c, _)| *c == class) {
        return Some(buffer.clone());
    }
    let buffer = scratch_buffer(class)?;
    crate::host::name(&buffer, || format!("depth twin copies {class:#x}"));
    all.0.push((class, buffer.clone()));
    Some(buffer)
}

/// A default-heap buffer of `size` bytes shaders may write, in the common
/// state.
fn scratch_buffer(size: u64) -> Option<ID3D12Resource> {
    let host = host()?;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(256),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    let made = unsafe {
        host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut buffer)
    };
    made.ok().and(buffer)
}

/// Logs once per (console format, view format, plane) what happened.
fn once(console: u32, view: u32, plane: u32, what: &str) {
    static SEEN: std::sync::Mutex<Vec<(u32, u32, u32, String)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let key = (console, view, plane, what.to_owned());
    if seen.len() < 64 && !seen.contains(&key) {
        tracing::info!(console, view, plane, "{what}");
        seen.push(key);
    }
}

/// An unordered access view at `at` of `resource`, a depth texture whose
/// host copy is `copy`, made on its twin's plane the title's view names
/// (`words`: the view description), in a format of that plane's family
/// (the title's own if it is one, else the plane's integer or normalised
/// one, as the title's format is). The twin's plane texture, which the
/// slot then views.
pub(crate) fn uav(
    resource: This,
    copy: &Arc<HostTexture>,
    words: Option<&[u32; 10]>,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<ID3D12Resource> {
    let console = crate::resource::title_format(resource)?;
    let twin = copy.twin.get_or_init(|| make(console, &copy.texture)).as_ref()?;
    let mut words = *words?;
    const TEXTURE_2D: u32 = 4;
    const TEXTURE_2D_ARRAY: u32 = 5;
    // The plane: last of the 2D view's fields (mip, plane; mip, first
    // slice, slices, plane).
    let plane_word = match words[1] {
        TEXTURE_2D => 3,
        TEXTURE_2D_ARRAY => 5,
        other => {
            once(console, words[0], other, "an unordered view of a depth texture of this dimension: not made");
            return None;
        }
    };
    let plane = words[plane_word] as usize;
    let Some(texture) = twin.planes.get(plane) else {
        once(console, words[0], plane as u32, "an unordered view of a depth plane the twin does not have: not made");
        return None;
    };
    // SAFETY: a live resource.
    let own = unsafe { texture.texture.GetDesc() }.Format.0 as u32;
    let view = crate::resource::pc_format(words[0]);
    let format = if crate::resource::family(view) == own {
        view
    } else {
        // R16_TYPELESS: unorm or uint; R32_TYPELESS: float or uint; R8: uint.
        match (own, integer(view)) {
            (53, false) => 56,
            (53, true) => 57,
            (39, false) => 41,
            (39, true) => 42,
            _ => 62,
        }
    };
    let host = host()?;
    once(console, words[0], plane as u32, "an unordered view of a depth texture, made on its writable twin");
    words[0] = format;
    words[plane_word] = 0;
    // SAFETY: 40 bytes in the host's layout.
    let desc = unsafe { std::ptr::from_ref(&words).cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>().read_unaligned() };
    twin.uav_planes.fetch_or(1u8 << plane, Relaxed);
    // SAFETY: a live texture and a slot of our heap.
    unsafe { host.device.CreateUnorderedAccessView(&texture.texture, None, Some(&desc), at) };
    Some(texture.texture.clone())
}

/// Whether a format holds integers (`_UINT`, `_SINT`).
fn integer(format: u32) -> bool {
    matches!(format, 3 | 4 | 7 | 8 | 12 | 14 | 17 | 18 | 25 | 30 | 32 | 36 | 38 | 42 | 43 | 50 | 52 | 57 | 59 | 62 | 64)
}

impl Twin {
    fn all_planes_mask(&self) -> u8 {
        (1u8 << self.planes.len().min(2)) - 1
    }

    fn active_planes(&self) -> u8 {
        let mask = self.uav_planes.load(Relaxed) & self.all_planes_mask();
        if mask != 0 { mask } else { self.all_planes_mask() }
    }
}

fn depth_plane_count(depth: &HostTexture) -> usize {
    depth
        .twin
        .get()
        .and_then(Option::as_ref)
        .map_or(1, |twin| twin.planes.len().min(2))
}

/// Before a use in `list` of the depth texture `depth` (with `twin`) in
/// host state `wanted`: the side the use needs is brought up to date (a
/// copy recorded now, or by the list's prelude for its first use). For
/// unordered access the twin's active planes are moved there and true
/// returned: the depth texture itself is not used.
pub(crate) fn route(list: &HostList, depth: &Arc<HostTexture>, twin: &Twin, wanted: u32) -> bool {
    let side = if wanted & UNORDERED_ACCESS != 0 { TWIN } else { DEPTH };
    let writes = side == TWIN || wanted & (DEPTH_WRITE | RENDER_TARGET | COPY_DEST | RESOLVE_DEST) != 0;
    let active = twin.active_planes();
    if let Some((to_depth, mask)) = list.twin_use(depth, side, writes, active)
        && twin.copy(&list.list, depth, to_depth, mask, &mut |t, w| list.move_texture(t, w))
    {
        list.need_rebind();
    }
    if side == DEPTH {
        return false;
    }
    let barriers: Vec<_> = twin
        .planes
        .iter()
        .enumerate()
        .filter(|&(p, _)| (active >> p) & 1 != 0)
        .filter_map(|(_, p)| list.move_texture(p, UNORDERED_ACCESS))
        .collect();
    if !barriers.is_empty() {
        // SAFETY: an open list and live textures.
        unsafe { list.list.ResourceBarrier(&barriers) };
    }
    true
}

fn new_planes(planes: usize, active: u8, side: u8, writes: bool) -> ([u8; 2], [Option<u8>; 2]) {
    let mut first = [0u8; 2];
    let mut last = [None; 2];
    for p in 0..planes.min(2) {
        if side == DEPTH || (active >> p) & 1 != 0 {
            first[p] = side;
            last[p] = writes.then_some(side);
        }
    }
    (first, last)
}

fn again_planes(
    planes: usize,
    active: u8,
    first: &mut [u8; 2],
    last: &mut [Option<u8>; 2],
    side: u8,
    writes: bool,
) -> Option<(bool, u8)> {
    let mut copy_mask = 0u8;
    for p in 0..planes.min(2) {
        if side == TWIN && (active >> p) & 1 == 0 {
            continue;
        }
        if first[p] == 0 {
            first[p] = side;
            last[p] = writes.then_some(side);
        } else if again(first[p], &mut last[p], side, writes).is_some() {
            copy_mask |= 1 << p;
        }
    }
    (copy_mask != 0).then_some((side == DEPTH, copy_mask))
}

impl Use {
    pub(crate) fn new(depth: &Arc<HostTexture>, active: u8, side: u8, writes: bool) -> Self {
        let planes = depth_plane_count(depth);
        let (first, last) = new_planes(planes, active, side, writes);
        Use { first, last, depth: depth.clone() }
    }

    /// A later use in the list needing `side`: whether a copy must be
    /// recorded first (true: into the depth texture) and which planes.
    pub(crate) fn again(&mut self, active: u8, side: u8, writes: bool) -> Option<(bool, u8)> {
        let planes = depth_plane_count(&self.depth);
        again_planes(planes, active, &mut self.first, &mut self.last, side, writes)
    }
}

/// [`Use::again`] on its first side and the side it left newer.
fn again(first: u8, last: &mut Option<u8>, side: u8, writes: bool) -> Option<bool> {
    // Unchanged since the list's start: only the first use's side is
    // known to be current there.
    let current = match *last {
        None => first == side,
        Some(newer) => newer == BOTH || newer == side,
    };
    let copy = (!current).then_some(side == DEPTH);
    if copy.is_some() {
        *last = Some(BOTH);
    }
    if writes {
        *last = Some(side);
    }
    copy
}

/// Where a list's uses leave a twin's newer side, from `newer` before it:
/// the copy its prelude makes first (true: into the depth texture), and the
/// newer side after the list.
fn submitted(first: u8, last: Option<u8>, newer: u8) -> (Option<bool>, u8) {
    let copy = match (first, newer) {
        (DEPTH, TWIN) => Some(true),
        (TWIN, DEPTH) => Some(false),
        _ => None,
    };
    let start = if copy.is_some() { BOTH } else { newer };
    (copy, last.unwrap_or(start))
}

fn submit_planes(planes: usize, first: [u8; 2], last: [Option<u8>; 2], newer: [u8; 2]) -> (u8, u8, [u8; 2]) {
    let mut to_depth_mask = 0u8;
    let mut to_twin_mask = 0u8;
    let mut after = newer;
    for p in 0..planes.min(2) {
        if first[p] == 0 {
            continue;
        }
        let (copy, next) = submitted(first[p], last[p], newer[p]);
        after[p] = next;
        match copy {
            Some(true) => to_depth_mask |= 1 << p,
            Some(false) => to_twin_mask |= 1 << p,
            None => {}
        }
    }
    (to_depth_mask, to_twin_mask, after)
}

/// At submission, for a list's twin uses in order: the copies its prelude
/// records first (the depth texture, whether into it, and which planes),
/// the twins' newer sides moved on to where the list leaves them.
pub(crate) fn at_submission<'a>(uses: impl Iterator<Item = &'a Use>) -> Vec<(Arc<HostTexture>, bool, u8)> {
    let mut copies = Vec::new();
    for u in uses {
        let Some(Some(twin)) = u.depth.twin.get() else { continue };
        let planes = twin.planes.len().min(2);
        let newer = [twin.plane_newer[0].load(Relaxed), twin.plane_newer[1].load(Relaxed)];
        let (to_depth_mask, to_twin_mask, after) = submit_planes(planes, u.first, u.last, newer);
        for (p, next) in after.iter().enumerate().take(planes) {
            twin.plane_newer[p].store(*next, Relaxed);
        }
        if to_depth_mask != 0 {
            copies.push((u.depth.clone(), true, to_depth_mask));
        }
        if to_twin_mask != 0 {
            copies.push((u.depth.clone(), false, to_twin_mask));
        }
    }
    copies
}

/// Moves a texture to a state for a copy: the barrier, if any.
pub(crate) type Mover<'a> = dyn FnMut(&Arc<HostTexture>, u32) -> Option<D3D12_RESOURCE_BARRIER> + 'a;

impl Twin {
    /// Records into `l` the copy of the newer side into the other for the
    /// planes in `plane_mask`: from the twin into `depth` (`to_depth`) or
    /// back. `mv` moves textures to the copy states (tracked by the list,
    /// or from their real states in a prelude). True if it ran our
    /// conversion, which replaces the list's compute root signature and
    /// pipeline.
    pub(crate) fn copy(
        &self,
        l: &ID3D12GraphicsCommandList,
        depth: &Arc<HostTexture>,
        to_depth: bool,
        plane_mask: u8,
        mv: &mut Mover,
    ) -> bool {
        let mask = plane_mask & self.all_planes_mask();
        if mask == 0 {
            return false;
        }
        let (src, dst) = if to_depth { (COPY_SOURCE, COPY_DEST) } else { (COPY_DEST, COPY_SOURCE) };
        let mut barriers: Vec<_> = self
            .planes
            .iter()
            .enumerate()
            .filter(|&(p, _)| (mask >> p) & 1 != 0)
            .filter_map(|(_, p)| mv(p, src))
            .collect();
        barriers.extend(mv(depth, dst));
        let at = |r: &ID3D12Resource, sub: u32| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the copy does not release.
            pResource: unsafe { std::mem::transmute_copy(r) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub },
        };
        let placed = |f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&self.scratch) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
        };
        let selected = |r: &Route| (mask >> r.plane) & 1 != 0;
        let converts = self.routes.iter().any(|r| selected(r) && r.conversion != Conversion::None);
        let state = |from, to| transition(&self.scratch, D3D12_RESOURCE_STATES(from), D3D12_RESOURCE_STATES(to));
        let mut converted = false;
        // SAFETY: an open list; live resources in the states named (the
        // scratch buffer starts common: its first copy promotes it).
        unsafe {
            if !barriers.is_empty() {
                l.ResourceBarrier(&barriers);
            }
            for r in self.routes.iter().filter(|r| selected(r)) {
                let plane = &self.planes[r.plane].texture;
                if to_depth {
                    l.CopyTextureRegion(&placed(r.twin), 0, 0, 0, &at(plane, r.twin_sub), None);
                } else {
                    l.CopyTextureRegion(&placed(r.depth.unwrap_or(r.twin)), 0, 0, 0, &at(&depth.texture, r.depth_sub), None);
                }
            }
            if converts && let Some(c) = converter() {
                l.ResourceBarrier(&[state(COPY_DEST as i32, UNORDERED_ACCESS as i32)]);
                l.SetComputeRootSignature(&c.root);
                l.SetPipelineState(&c.pso);
                l.SetComputeRootUnorderedAccessView(1, self.scratch.GetGPUVirtualAddress());
                for r in self.routes.iter().filter(|r| selected(r) && r.conversion != Conversion::None) {
                    let t = r.twin.Footprint;
                    let d = r.depth.unwrap_or(r.twin);
                    let mode = u32::from(r.conversion == Conversion::F32ToD24);
                    let constants = [t.Width, t.Height, t.RowPitch, r.twin.Offset as u32, d.Footprint.RowPitch, d.Offset as u32, u32::from(to_depth), mode];
                    l.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
                    l.Dispatch(t.Width.div_ceil(2).div_ceil(64), t.Height, 1);
                }
                l.ResourceBarrier(&[uav_barrier(), state(UNORDERED_ACCESS as i32, COPY_SOURCE as i32)]);
                converted = true;
            } else {
                l.ResourceBarrier(&[state(COPY_DEST as i32, COPY_SOURCE as i32)]);
            }
            for r in self.routes.iter().filter(|r| selected(r)) {
                let plane = &self.planes[r.plane].texture;
                if to_depth {
                    l.CopyTextureRegion(&at(&depth.texture, r.depth_sub), 0, 0, 0, &placed(r.depth.unwrap_or(r.twin)), None);
                } else {
                    l.CopyTextureRegion(&at(plane, r.twin_sub), 0, 0, 0, &placed(r.twin), None);
                }
            }
            l.ResourceBarrier(&[state(COPY_SOURCE as i32, 0)]);
        }
        COPIES.fetch_add(1, Relaxed);
        converted
    }
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
        },
    }
}

/// Depth conversion in `scratch`: mode 0 converts 16-bit unorm depth (`R16`,
/// two per dword) to the host's 32-bit float depth plane (`u * 0x37800080`,
/// matching rasterised 16-bit depth EQUAL tests) and back; mode 1 converts
/// 32-bit float depth (`R32_FLOAT`) to the host's 24-bit unorm depth plane
/// (in the low 24 bits of 32) and back via bit-exact IEEE-754 integer
/// arithmetic.
const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), UAV(u0)"
cbuffer C : register(b0) { uint width; uint height; uint pitch16; uint at16; uint pitch32; uint at32; uint toDepth; uint mode; };
RWByteAddressBuffer buf : register(u0);
uint depth(uint u) { return asuint((float)u * asfloat(0x37800080)); }
uint u16(uint d) { return (uint)(saturate(asfloat(d)) * 65535.0 + 0.5); }
uint f32_to_d24(uint bits) {
    if (bits > 0x7f800000) return 0;
    if (bits >= 0x3f800000) return 0xffffff;
    uint exp = bits >> 23;
    if (exp < 102) return 0;
    uint m = (bits & 0x7fffff) | 0x800000;
    uint s = 126 - exp;
    if (s == 0) return max(m - 1, 0x800000);
    uint hi = m - 1;
    return ((hi >> (s - 1)) + 1) >> 1;
}
uint d24_to_f32(uint d) {
    d &= 0xffffff;
    if (d == 0) return 0;
    uint b = firstbithigh(d);
    uint m = d << (23 - b);
    return ((102 + b) << 23) + m + 1;
}
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x * 2;
    uint y = id.y;
    if (x >= width || y >= height) return;
    uint b = at32 + y * pitch32 + x * 4;
    bool two = x + 1 < width;
    if (mode != 0) {
        if (toDepth != 0) {
            buf.Store(b, f32_to_d24(buf.Load(b)));
            if (two) buf.Store(b + 4, f32_to_d24(buf.Load(b + 4)));
        } else {
            buf.Store(b, d24_to_f32(buf.Load(b)));
            if (two) buf.Store(b + 4, d24_to_f32(buf.Load(b + 4)));
        }
        return;
    }
    uint a = at16 + y * pitch16 + x * 2;
    if (toDepth != 0) {
        uint pair = buf.Load(a);
        buf.Store(b, depth(pair & 0xffff));
        if (two) buf.Store(b + 4, depth(pair >> 16));
    } else {
        uint lo = u16(buf.Load(b));
        uint hi = two ? u16(buf.Load(b + 4)) : 0;
        buf.Store(a, lo | hi << 16);
    }
}
"#;

struct Converter {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Converter {}
// SAFETY: as above.
unsafe impl Sync for Converter {}

fn converter() -> Option<&'static Converter> {
    static MADE: std::sync::OnceLock<Option<Converter>> = std::sync::OnceLock::new();
    MADE.get_or_init(|| match compile() {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "no depth conversion: converted depth written through unordered views does not reach the depth texture");
            None
        }
    })
    .as_ref()
}

fn compile() -> windows::core::Result<Converter> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("depth"), None, None, s!("main"), s!("cs_5_1"), 0, 0, &mut code, Some(&mut errors))
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe { std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize()) };
            tracing::warn!("depth conversion: {}", String::from_utf8_lossy(text));
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
        Ok(Converter { root, pso })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{HRESULT, S_OK, slot};

    static GPU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn f32_to_d24_ref(f: f32) -> u32 {
        let clamped = if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) };
        ((f64::from(clamped) * 16_777_215.0).round_ties_even() as u32) & 0x00ff_ffff
    }

    fn d24_to_f32_ref(d: u32) -> f32 {
        ((d & 0x00ff_ffff) as f32) / 16_777_215.0
    }

    struct TitleTex(This);

    impl Drop for TitleTex {
        fn drop(&mut self) {
            type Release = extern "system" fn(This) -> u32;
            slot::<Release>(self.0, 0x10)(self.0);
        }
    }

    fn make_r24g8_depth(width: u64, height: u32, clear: Option<[u32; 5]>) -> (TitleTex, Arc<HostTexture>) {
        // dimension=3 (Texture2D), format=44 (R24G8_TYPELESS), flags=DEPTH(2)|UNORDERED(4)
        let desc: [u32; 14] = [
            3,
            0,
            0,
            0,
            width as u32,
            (width >> 32) as u32,
            height,
            1 | (1 << 16),
            44,
            1,
            0,
            0,
            6,
            0,
        ];
        let heap = [1u32, 0, 0, 0, 0];
        let iid = crate::object::guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let mut out = std::ptr::null();
        let clear_ptr = clear.as_ref().map_or(std::ptr::null(), |c| c.as_ptr().cast());
        let hr: HRESULT = crate::resource::create_committed(
            std::ptr::null(),
            heap.as_ptr().cast(),
            0,
            desc.as_ptr().cast(),
            0,
            clear_ptr,
            &iid,
            &mut out,
        );
        assert_eq!(hr, S_OK);
        let copy = crate::resource::host_copy(out).unwrap();
        (TitleTex(out), copy)
    }

    fn uav_words(format: u32, plane: u32) -> [u32; 10] {
        [format, 4, 0, plane, 0, 0, 0, 0, 0, 0]
    }

    fn compile_test_cs(source: &str) -> (ID3D12RootSignature, ID3D12PipelineState) {
        let host = host().unwrap();
        let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
        // SAFETY: source and entrypoint names live through the call.
        let compiled = unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                s!("twin_test"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(e) = compiled {
            if let Some(errors) = errors {
                // SAFETY: valid error blob returned by D3DCompile.
                let text = unsafe {
                    std::slice::from_raw_parts(
                        errors.GetBufferPointer().cast::<u8>(),
                        errors.GetBufferSize(),
                    )
                };
                panic!("test shader compile failed ({e}): {}", String::from_utf8_lossy(text));
            }
            panic!("test shader compile failed: {e}");
        }
        let code = code.unwrap();
        // SAFETY: code blob returned by D3DCompile contains root signature and DXBC.
        unsafe {
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).unwrap();
            let pso = host
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: bytes.as_ptr().cast(),
                        BytecodeLength: bytes.len(),
                    },
                    ..Default::default()
                })
                .unwrap();
            (root, pso)
        }
    }

    fn make_buffer(heap_type: D3D12_HEAP_TYPE, bytes: u64, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let host = host().unwrap();
        let heap = D3D12_HEAP_PROPERTIES { Type: heap_type, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes.max(256),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buf: Option<ID3D12Resource> = None;
        // SAFETY: valid buffer desc and out pointer.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut buf)
                .unwrap();
        }
        buf.unwrap()
    }

    fn copy_subresource_to_readback(
        list: &HostList,
        tex: &ID3D12Resource,
        sub: u32,
    ) -> (ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT) {
        let host = host().unwrap();
        // SAFETY: live texture resource.
        let desc = unsafe { tex.GetDesc() };
        let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        // SAFETY: single-element output pointers for `sub`.
        unsafe {
            host.device.GetCopyableFootprints(
                &desc,
                sub,
                1,
                0,
                Some(&mut fp),
                None,
                None,
                Some(&mut total),
            );
        }
        let readback = make_buffer(D3D12_HEAP_TYPE_READBACK, total, D3D12_RESOURCE_STATE_COPY_DEST);
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed COM pointer lives through CopyTextureRegion.
            pResource: unsafe { std::mem::transmute_copy(&readback) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed COM pointer lives through CopyTextureRegion.
            pResource: unsafe { std::mem::transmute_copy(tex) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: sub },
        };
        // SAFETY: open list, live resources in COPY_SOURCE / COPY_DEST.
        unsafe { list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None) };
        (readback, fp)
    }

    fn read_mapped_rows<T: Copy>(
        buf: &ID3D12Resource,
        fp: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    ) -> Vec<T> {
        let mut ptr = std::ptr::null_mut();
        // SAFETY: readback buffer finished executing.
        unsafe {
            buf.Map(0, None, Some(&mut ptr)).unwrap();
            let base = ptr.cast::<u8>().add(fp.Offset as usize);
            let mut out = Vec::with_capacity((fp.Footprint.Width * fp.Footprint.Height) as usize);
            for y in 0..fp.Footprint.Height {
                let row = base.add((y * fp.Footprint.RowPitch) as usize).cast::<T>();
                out.extend_from_slice(std::slice::from_raw_parts(row, fp.Footprint.Width as usize));
            }
            buf.Unmap(0, None);
            out
        }
    }

    fn move_and_barrier(list: &HostList, tex: &Arc<HostTexture>, state: u32) {
        if let Some(b) = list.move_texture(tex, state) {
            // SAFETY: open list and live texture.
            unsafe { list.list.ResourceBarrier(&[b]) };
        }
    }

    fn transition_title_barrier(list: &HostList, res: This, state: u32) {
        if let Some((tex, before)) = list.transition(res, state) {
            let wanted = crate::host::legal_state(&tex, state);
            let b = transition(
                &tex,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(wanted as i32),
            );
            // SAFETY: open list and live texture.
            unsafe { list.list.ResourceBarrier(&[b]) };
        }
    }

    fn make_view_heap(
        count: u32,
    ) -> (ID3D12DescriptorHeap, usize, usize, D3D12_GPU_DESCRIPTOR_HANDLE) {
        let host = host().unwrap();
        // SAFETY: valid descriptor heap creation and handle queries.
        unsafe {
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: count,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .unwrap();
            let step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let cpu = heap.GetCPUDescriptorHandleForHeapStart().ptr;
            let gpu = heap.GetGPUDescriptorHandleForHeapStart();
            (heap, cpu, step, gpu)
        }
    }

    fn readback_twin_planes(
        list: &HostList,
        twin: &Twin,
    ) -> Vec<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)> {
        let mut rbs = Vec::with_capacity(twin.planes.len());
        for p in &twin.planes {
            move_and_barrier(list, p, COPY_SOURCE);
            rbs.push(copy_subresource_to_readback(list, &p.texture, 0));
            move_and_barrier(list, p, 0);
        }
        rbs
    }

    fn upload_zero_plane(list: &HostList, plane: &Arc<HostTexture>) -> ID3D12Resource {
        let host = host().unwrap();
        // SAFETY: live texture resource.
        let desc = unsafe { plane.texture.GetDesc() };
        let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        // SAFETY: single-element output pointers.
        unsafe {
            host.device.GetCopyableFootprints(
                &desc,
                0,
                1,
                0,
                Some(&mut fp),
                None,
                None,
                Some(&mut total),
            );
        }
        let upload = make_buffer(D3D12_HEAP_TYPE_UPLOAD, total, D3D12_RESOURCE_STATE_GENERIC_READ);
        let mut ptr = std::ptr::null_mut();
        let empty = D3D12_RANGE { Begin: 0, End: 0 };
        // SAFETY: upload buffer sized to `total` bytes.
        unsafe {
            upload.Map(0, Some(&empty), Some(&mut ptr)).unwrap();
            ptr.cast::<u8>().write_bytes(0, total as usize);
            upload.Unmap(0, None);
        }
        move_and_barrier(list, plane, COPY_DEST);
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed COM pointer lives through CopyTextureRegion.
            pResource: unsafe { std::mem::transmute_copy(&plane.texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed COM pointer lives through CopyTextureRegion.
            pResource: unsafe { std::mem::transmute_copy(&upload) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp },
        };
        // SAFETY: open list and live buffers/textures.
        unsafe { list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None) };
        upload
    }

    /// The material-depth frame of Grand Theft Auto VI: written through
    /// unordered views, read as a shader resource in the same list, written
    /// again; read in the next list; tested as depth in a later one; the
    /// next frame writes it through unordered views again.
    #[test]
    fn copies_follow_the_side_each_use_needs() {
        // List 1: unordered write first (the prelude copies the depth side
        // over, newer since the last frame), a read (copy back now), a
        // write again.
        let mut last = Some(TWIN);
        assert_eq!(again(TWIN, &mut last, DEPTH, false), Some(true));
        assert_eq!(last, Some(BOTH));
        assert_eq!(again(TWIN, &mut last, TWIN, true), None);
        assert_eq!(submitted(TWIN, last, DEPTH), (Some(false), TWIN));
        // List 2 only reads: its prelude copies into the depth texture.
        assert_eq!(submitted(DEPTH, None, TWIN), (Some(true), BOTH));
        // List 3 renders to it: nothing to copy, the depth side is newer.
        assert_eq!(submitted(DEPTH, Some(DEPTH), BOTH), (None, DEPTH));
        // A list that starts reading the depth side, unchanged since, does
        // not know the twin is current: a use of the twin copies.
        let mut last = None;
        assert_eq!(again(DEPTH, &mut last, DEPTH, false), None);
        assert_eq!(again(DEPTH, &mut last, TWIN, true), Some(false));
        assert_eq!(last, Some(TWIN));
    }

    /// A list that writes a depth texture through its twin and then reads
    /// its depth side: its prelude owes a copy into the twin first. Every
    /// barrier the prelude and the list issue starts from the state the
    /// texture is in, as the debug layer checks (run light21: the prelude's
    /// copy started from the states the list leaves).
    #[test]
    fn prelude_copies_start_from_the_real_states() {
        use crate::host::tests::{issued, replay};
        use windows::core::Interface;
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 64,
            Height: 64,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R16_TYPELESS,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creation; our out pointer.
        unsafe { host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_DEPTH_WRITE, None, &mut texture) }.unwrap();
        let depth = Arc::new(HostTexture::new(texture.unwrap(), DEPTH_WRITE, Vec::new()));
        let twin = depth.twin.get_or_init(|| make(53, &depth.texture)).as_ref().unwrap();
        let list = host.new_list().unwrap();
        issued();
        assert!(route(&list, &depth, twin, UNORDERED_ACCESS));
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        assert!(!route(&list, &depth, twin, PIXEL_SHADER_RESOURCE));
        if let Some(b) = list.move_texture(&depth, PIXEL_SHADER_RESOURCE) {
            // SAFETY: an open list and a live texture.
            unsafe { list.list.ResourceBarrier(&[b]) };
        }
        list.close();
        let recorded = issued();
        host.execute(&[&list]);
        // The GPU runs the prelude, then the list.
        let mut run = issued();
        run.extend(recorded);
        let key = |t: &HostTexture| t.texture.as_raw() as usize;
        let mut states = std::collections::HashMap::from([(key(&depth), DEPTH_WRITE), (key(&twin.planes[0]), 0)]);
        replay(&run, &mut states).unwrap();
        // What the list left is what the next prelude starts from.
        assert_eq!(states[&key(&depth)], depth.state.load(Relaxed));
        assert_eq!(states[&key(&twin.planes[0])], twin.planes[0].state.load(Relaxed));
    }

    #[test]
    fn f32_d24_round_trips_every_24bit_value() {
        for f in 44..=47 {
            assert_eq!(plane_formats(f), Some([39u32, 60].as_slice()));
        }
        for f in 118..=120 {
            assert_eq!(plane_formats(f), Some([53u32, 60].as_slice()));
        }
        for f in 19..=22 {
            assert_eq!(plane_formats(f), Some([39u32, 60].as_slice()));
        }
        for f in 39..=41 {
            assert_eq!(plane_formats(f), Some([39u32].as_slice()));
        }
        assert_eq!(plane_formats(53), Some([53u32].as_slice()));
        assert_eq!(plane_formats(55), Some([53u32].as_slice()));
        assert_eq!(plane_formats(28), None);

        for d in 0..=0x00ff_ffff_u32 {
            assert_eq!(f32_to_d24_ref(d24_to_f32_ref(d)), d, "mismatch at {d:#x}");
        }
        assert_eq!(f32_to_d24_ref(-0.0), 0);
        assert_eq!(f32_to_d24_ref(-0.25), 0);
        assert_eq!(f32_to_d24_ref(1.5), 0x00ff_ffff);
        assert_eq!(f32_to_d24_ref(f32::INFINITY), 0x00ff_ffff);
        assert_eq!(f32_to_d24_ref(f32::NEG_INFINITY), 0);
        assert_eq!(f32_to_d24_ref(f32::NAN), 0);
        assert_eq!(f32_to_d24_ref(f32::from_bits(0x3300_0000)), 0);
        assert_eq!(f32_to_d24_ref(f32::from_bits(0x3300_0001)), 1);
        assert_eq!(f32_to_d24_ref(f32::from_bits(0x3e80_0002)), 0x40_0001);
        assert_eq!(f32_to_d24_ref(0.5), 0x80_0000);
        assert_eq!(f32_to_d24_ref(f32::from_bits(0x3f00_0004)), 0x80_0003);
    }

    #[test]
    fn per_plane_tracking_never_clobbers_inactive_or_unseeded_planes() {
        // Case A: uav(p0) -> TWIN -> uav(p1) -> DEPTH read (copies p0 only).
        let (mut first, mut last) = new_planes(2, 0b01, TWIN, true);
        assert_eq!(first, [TWIN, 0]);
        assert_eq!(last, [Some(TWIN), None]);
        let newer = [DEPTH, DEPTH];
        let (to_depth, to_twin, newer) = submit_planes(2, first, last, newer);
        assert_eq!((to_depth, to_twin, newer), (0b00, 0b01, [TWIN, DEPTH]));

        let (first_d, last_d) = new_planes(2, 0b11, DEPTH, false);
        assert_eq!(first_d, [DEPTH, DEPTH]);
        let (to_depth, to_twin, newer) = submit_planes(2, first_d, last_d, newer);
        assert_eq!((to_depth, to_twin, newer), (0b01, 0b00, [BOTH, DEPTH]));

        // Case B: continuing from Case A -> TWIN seeds only p1 (DEPTH -> TWIN).
        let (first_t, last_t) = new_planes(2, 0b11, TWIN, true);
        let (to_depth, to_twin, newer) = submit_planes(2, first_t, last_t, newer);
        assert_eq!((to_depth, to_twin, newer), (0b00, 0b10, [TWIN, TWIN]));

        // Case C: uav(p0) -> TWIN -> uav(p1) -> TWIN immediately before DEPTH.
        let (to_depth, to_twin, newer_c) = submit_planes(2, first, last, [DEPTH, DEPTH]);
        assert_eq!((to_depth, to_twin, newer_c), (0b00, 0b01, [TWIN, DEPTH]));
        let (to_depth, to_twin, newer_c) = submit_planes(2, first_t, last_t, newer_c);
        assert_eq!((to_depth, to_twin, newer_c), (0b00, 0b10, [TWIN, TWIN]));

        // Case D: mid-list TWIN(p0) -> uav(p1) -> DEPTH copies only p0.
        assert_eq!(again_planes(2, 0b11, &mut first, &mut last, DEPTH, false), Some((true, 0b01)));
        assert_eq!(first, [TWIN, DEPTH]);
        assert_eq!(last, [Some(BOTH), None]);

        // Case E: mid-list DEPTH(write) -> TWIN(p0) -> uav(p1) -> TWIN copies only p1.
        let (mut f_e, mut l_e) = new_planes(2, 0b01, DEPTH, true);
        assert_eq!(again_planes(2, 0b01, &mut f_e, &mut l_e, TWIN, true), Some((false, 0b01)));
        assert_eq!(again_planes(2, 0b11, &mut f_e, &mut l_e, TWIN, true), Some((false, 0b10)));
        assert_eq!(l_e, [Some(TWIN), Some(TWIN)]);

        // Case F: plane-1-only UAV (0b10) -> TWIN -> DEPTH copies only p1.
        let (f_f, l_f) = new_planes(2, 0b10, TWIN, true);
        let (to_depth, to_twin, newer_f) = submit_planes(2, f_f, l_f, [DEPTH, DEPTH]);
        assert_eq!((to_depth, to_twin, newer_f), (0b00, 0b10, [DEPTH, TWIN]));
        let (f_f2, l_f2) = new_planes(2, 0b10, DEPTH, false);
        let (to_depth, to_twin, newer_f) = submit_planes(2, f_f2, l_f2, newer_f);
        assert_eq!((to_depth, to_twin, newer_f), (0b10, 0b00, [DEPTH, BOTH]));
    }

    #[test]
    fn r24g8_depth_twin_round_trips_depth_and_stencil_on_gpu() {
        let _guard = GPU_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = host() else { return };
        host.log_messages();

        const W: u32 = 9;
        const H: u32 = 4;
        const TEXELS: usize = (W * H) as usize;
        let (tex, copy) = make_r24g8_depth(u64::from(W), H, None);

        // Shader-visible descriptor heap: t0 (input pairs), u0 (depth float), u1 (stencil uint).
        let (heap, cpu_start, step, gpu_start) = make_view_heap(3);
        let cpu_at = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu_start + i * step };

        // Populate input upload buffer with 36 (f32, stencil) pairs covering tricky 24-bit values.
        let depth_inputs: Vec<f32> = (0..TEXELS)
            .map(|i| match i {
                0 => 0.0,
                1 => d24_to_f32_ref(1),
                2 => d24_to_f32_ref(0x7f_ffff),
                3 => d24_to_f32_ref(0x80_0000),
                4 => d24_to_f32_ref(0xff_fffd),
                5 => d24_to_f32_ref(0xff_fffe),
                6 => 1.0,
                7 => -0.25,
                8 => 1.5,
                _ => d24_to_f32_ref(((i as u32) * 466_033 + 17) & 0x00ff_ffff),
            })
            .collect();
        let stencil_inputs: Vec<u8> = (0..TEXELS).map(|i| ((i * 73 + 19) & 0xff) as u8).collect();
        let input_buf = make_buffer(
            D3D12_HEAP_TYPE_UPLOAD,
            (TEXELS * 8) as u64,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        // SAFETY: `input_buf` holds `TEXELS * 8` bytes and slot 0 belongs to `heap`.
        unsafe {
            let mut ptr = std::ptr::null_mut();
            let empty = D3D12_RANGE { Begin: 0, End: 0 };
            input_buf.Map(0, Some(&empty), Some(&mut ptr)).unwrap();
            let dst = std::slice::from_raw_parts_mut(ptr.cast::<u32>(), TEXELS * 2);
            for i in 0..TEXELS {
                dst[i * 2] = depth_inputs[i].to_bits();
                dst[i * 2 + 1] = u32::from(stencil_inputs[i]);
            }
            input_buf.Unmap(0, None);
            host.device.CreateShaderResourceView(
                &input_buf,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV {
                            NumElements: (TEXELS * 2) as u32,
                            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                            ..Default::default()
                        },
                    },
                }),
                cpu_at(0),
            );
        }

        assert!(uav(tex.0, &copy, Some(&uav_words(46, 0)), cpu_at(1)).is_some());
        let twin = copy.twin.get().unwrap().as_ref().unwrap();
        assert_eq!(twin.uav_planes.load(Relaxed), 0b01);
        assert!(uav(tex.0, &copy, Some(&uav_words(47, 1)), cpu_at(2)).is_some());
        assert_eq!(twin.uav_planes.load(Relaxed), 0b11);

        let cs = r#"
#define RS "RootConstants(num32BitConstants=2, b0), DescriptorTable(SRV(t0), UAV(u0, numDescriptors=2))"
cbuffer C : register(b0) { uint width; uint writeStencil; };
ByteAddressBuffer src : register(t0);
RWTexture2D<float> u_depth : register(u0);
RWTexture2D<uint> u_stencil : register(u1);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= width) return;
    uint idx = id.y * width + id.x;
    u_depth[id.xy] = asfloat(src.Load(idx * 8));
    if (writeStencil != 0) u_stencil[id.xy] = src.Load(idx * 8 + 4);
}
"#;
        let (root, pso) = compile_test_cs(cs);

        // List 1: write twin via compute, convert mid-list TWIN -> DEPTH, read back depth planes.
        let list = host.new_list().unwrap();
        assert!(list.transition(tex.0, UNORDERED_ACCESS).is_none());
        // SAFETY: open list with live root signature, PSO, descriptor heap, and twin UAVs.
        unsafe {
            list.list.SetComputeRootSignature(&root);
            list.list.SetPipelineState(&pso);
            list.list.SetDescriptorHeaps(&[Some(heap.clone())]);
            list.list.SetComputeRoot32BitConstants(0, 2, [W, 1u32].as_ptr().cast(), 0);
            list.list.SetComputeRootDescriptorTable(1, gpu_start);
            list.list.Dispatch(W.div_ceil(8), H.div_ceil(8), 1);
        }
        transition_title_barrier(&list, tex.0, COPY_SOURCE);
        assert!(list.take_rebind(), "mid-list TWIN->DEPTH conversion should request rebind");
        let (rb_depth0, fp_depth0) = copy_subresource_to_readback(&list, &copy.texture, 0);
        let (rb_depth1, fp_depth1) = copy_subresource_to_readback(&list, &copy.texture, 1);
        transition_title_barrier(&list, tex.0, 0);
        for p in &twin.planes {
            move_and_barrier(&list, p, 0);
        }
        list.close();
        host.execute(&[&list]);

        let got_d24 = read_mapped_rows::<u32>(&rb_depth0, &fp_depth0);
        let got_s8 = read_mapped_rows::<u8>(&rb_depth1, &fp_depth1);
        for i in 0..TEXELS {
            let expected = f32_to_d24_ref(depth_inputs[i]);
            let got = got_d24[i] & 0x00ff_ffff;
            if i <= 8 {
                assert_eq!(got, expected, "depth texel {i}");
            } else {
                // AMD RDNA backs `R24G8_TYPELESS` with `D32_FLOAT` internally, so
                // `CopyTextureRegion` can shift 13 half-ULP tie values (including
                // texel 18 = `0x80_0003`) by 1 LSB; `f32_d24_converter_is_bit_exact_on_gpu`
                // verifies `SOURCE` directly with 0-LSB tolerance.
                assert!(
                    got.abs_diff(expected) <= 1,
                    "depth texel {i}: got {got} ({got:#x}), expected {expected} ({expected:#x})"
                );
            }
            assert_eq!(got_s8[i], stencil_inputs[i], "stencil texel {i}");
        }

        // List 2: mark DEPTH newer, overwrite twin planes with zeros, convert mid-list DEPTH -> TWIN.
        list.reset();
        assert!(list.transition(tex.0, DEPTH_WRITE).is_none());
        let _up0 = upload_zero_plane(&list, &twin.planes[0]);
        let _up1 = upload_zero_plane(&list, &twin.planes[1]);
        assert!(list.transition(tex.0, UNORDERED_ACCESS).is_none());
        assert!(list.take_rebind(), "mid-list DEPTH->TWIN conversion should request rebind");
        let twin_rbs = readback_twin_planes(&list, twin);
        move_and_barrier(&list, &copy, 0);
        list.close();
        host.execute(&[&list]);

        let got_f32 = read_mapped_rows::<f32>(&twin_rbs[0].0, &twin_rbs[0].1);
        let got_twin_s8 = read_mapped_rows::<u8>(&twin_rbs[1].0, &twin_rbs[1].1);
        for i in 0..TEXELS {
            let expected_f = d24_to_f32_ref(got_d24[i] & 0x00ff_ffff);
            assert_eq!(
                got_f32[i].to_bits(),
                expected_f.to_bits(),
                "twin plane 0 texel {i}"
            );
            assert_eq!(got_twin_s8[i], stencil_inputs[i], "twin plane 1 texel {i}");
        }
        assert_eq!(
            host.log_messages(),
            None,
            "D3D12 debug layer reported errors"
        );
    }

    #[test]
    fn f32_d24_converter_is_bit_exact_on_gpu() {
        let _guard = GPU_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = host() else { return };
        let conv = converter().expect("depth converter compiles");
        host.log_messages();

        // Odd width exercises both `x` parities, `two = false` at `x = W - 1`, and row pitch.
        const W: u32 = 65;
        const H: u32 = 2;
        const PITCH: u32 = 512;
        const TEXELS: usize = (W * H) as usize;
        const BYTES: u64 = (PITCH * H) as u64;

        let mut tricky = vec![
            0.0,
            -0.0,
            f32::NAN,
            f32::from_bits(0x7f80_0001),
            f32::from_bits(0xffc0_0000),
            -0.25,
            f32::NEG_INFINITY,
            1.5,
            f32::INFINITY,
            f32::from_bits(0x32ff_ffff),
            f32::from_bits(0x3300_0000),
            f32::from_bits(0x3300_0001),
            f32::from_bits(0x3e80_0002),
            f32::from_bits(0x3eff_fffe),
            f32::from_bits(0x3eff_ffff),
            0.5,
            f32::from_bits(0x3f00_0001),
            f32::from_bits(0x3f00_0004),
            f32::from_bits(0x3f7f_fffe),
            f32::from_bits(0x3f7f_ffff),
            1.0,
            d24_to_f32_ref(0x40_0001),
            d24_to_f32_ref(0x40_0003),
            d24_to_f32_ref(0x40_0005),
        ];
        for d in 0x80_0001..=0x80_000a_u32 {
            tricky.push(d24_to_f32_ref(d));
        }
        for d in [
            0x60_0301, 0x60_2a01, 0x63_8e3f, 0x71_c721, 0x7f_ffff, 0xc7_1c6d, 0xff_fffd, 0xff_fffe,
        ] {
            tricky.push(d24_to_f32_ref(d));
        }

        let mut inputs = Vec::with_capacity(TEXELS);
        inputs.extend_from_slice(&tricky);
        // Repeat shifted by 1 column so every edge case runs on both even and odd `x`.
        inputs.push(0.5);
        inputs.extend_from_slice(&tricky);
        while inputs.len() < TEXELS {
            let idx = inputs.len() as u32;
            inputs.push(d24_to_f32_ref((idx * 199_999 + 123) & 0x00ff_ffff));
        }
        inputs[W as usize - 1] = f32::from_bits(0x3f00_0004);
        inputs[W as usize] = d24_to_f32_ref(0x40_0003);

        let upload = make_buffer(
            D3D12_HEAP_TYPE_UPLOAD,
            BYTES,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        // SAFETY: `upload` holds `BYTES` bytes.
        unsafe {
            let mut ptr = std::ptr::null_mut();
            let empty = D3D12_RANGE { Begin: 0, End: 0 };
            upload.Map(0, Some(&empty), Some(&mut ptr)).unwrap();
            ptr.cast::<u8>().write_bytes(0, BYTES as usize);
            for y in 0..H {
                let row = ptr.cast::<u8>().add((y * PITCH) as usize).cast::<u32>();
                let slice = std::slice::from_raw_parts_mut(row, W as usize);
                for x in 0..W {
                    slice[x as usize] = inputs[(y * W + x) as usize].to_bits();
                }
            }
            upload.Unmap(0, None);
        }

        let uav_buf = scratch_buffer(BYTES).unwrap();
        let rb_d24 = make_buffer(
            D3D12_HEAP_TYPE_READBACK,
            BYTES,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let rb_f32 = make_buffer(
            D3D12_HEAP_TYPE_READBACK,
            BYTES,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        let list = host.new_list().unwrap();
        let groups_x = W.div_ceil(2).div_ceil(64);
        let to_depth = [W, H, PITCH, 0, PITCH, 0, 1, 1];
        let to_twin = [W, H, PITCH, 0, PITCH, 0, 0, 1];
        // SAFETY: open list, live resources in matching barrier states, and valid UAV address.
        unsafe {
            list.list.CopyBufferRegion(&uav_buf, 0, &upload, 0, BYTES);
            list.list.ResourceBarrier(&[transition(
                &uav_buf,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            list.list.SetComputeRootSignature(&conv.root);
            list.list.SetPipelineState(&conv.pso);
            list.list
                .SetComputeRootUnorderedAccessView(1, uav_buf.GetGPUVirtualAddress());
            list.list
                .SetComputeRoot32BitConstants(0, 8, to_depth.as_ptr().cast(), 0);
            list.list.Dispatch(groups_x, H, 1);
            list.list.ResourceBarrier(&[transition(
                &uav_buf,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.list.CopyBufferRegion(&rb_d24, 0, &uav_buf, 0, BYTES);
            list.list.ResourceBarrier(&[transition(
                &uav_buf,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            list.list
                .SetComputeRoot32BitConstants(0, 8, to_twin.as_ptr().cast(), 0);
            list.list.Dispatch(groups_x, H, 1);
            list.list.ResourceBarrier(&[transition(
                &uav_buf,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.list.CopyBufferRegion(&rb_f32, 0, &uav_buf, 0, BYTES);
            list.list.ResourceBarrier(&[transition(
                &uav_buf,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }
        list.close();
        host.execute(&[&list]);

        let fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: DXGI_FORMAT_R32_TYPELESS,
                Width: W,
                Height: H,
                Depth: 1,
                RowPitch: PITCH,
            },
        };
        let got_d24 = read_mapped_rows::<u32>(&rb_d24, &fp);
        let got_f32 = read_mapped_rows::<u32>(&rb_f32, &fp);
        for i in 0..TEXELS {
            let expected_d24 = f32_to_d24_ref(inputs[i]);
            assert_eq!(got_d24[i], expected_d24, "f32_to_d24 mismatch at texel {i}");
            let expected_f32 = d24_to_f32_ref(expected_d24).to_bits();
            assert_eq!(got_f32[i], expected_f32, "d24_to_f32 mismatch at texel {i}");
        }
        assert_eq!(
            host.log_messages(),
            None,
            "D3D12 debug layer reported errors"
        );
    }

    #[test]
    fn active_planes_skip_inactive_stencil_and_preserve_depth_stencil_on_late_uav() {
        let _guard = GPU_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = host() else { return };
        host.log_messages();

        const W: u32 = 4;
        const H: u32 = 4;
        const TEXELS: usize = (W * H) as usize;
        let clear = [45u32, 0.25f32.to_bits(), 0x5A, 0, 0];
        let (tex, copy) = make_r24g8_depth(u64::from(W), H, Some(clear));

        // List 0: clear depth=0.25, stencil=0x5A, then return to COMMON.
        let list = host.new_list().unwrap();
        assert!(list.transition(tex.0, DEPTH_WRITE).is_none());
        let dsv = host.dsv(
            &copy.texture,
            Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: DXGI_FORMAT_D24_UNORM_S8_UINT,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                ..Default::default()
            }),
        );
        // SAFETY: open list and live depth-stencil texture in DEPTH_WRITE state.
        unsafe {
            list.list.ClearDepthStencilView(
                dsv,
                D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
                0.25,
                0x5A,
                None,
            );
        }
        transition_title_barrier(&list, tex.0, 0);
        list.close();
        host.execute(&[&list]);

        // Descriptor heap with plane 0 UAV only at first.
        let (heap, cpu_start, step, gpu_start) = make_view_heap(2);
        let cpu_at = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu_start + i * step };

        assert!(uav(tex.0, &copy, Some(&uav_words(46, 0)), cpu_at(0)).is_some());
        let twin = copy.twin.get().unwrap().as_ref().unwrap();
        assert_eq!(twin.uav_planes.load(Relaxed), 0b01);

        let cs = r#"
#define RS "DescriptorTable(UAV(u0))"
RWTexture2D<float> u_depth : register(u0);
[RootSignature(RS)]
[numthreads(4, 4, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    u_depth[id.xy] = 0.75;
}
"#;
        let (root, pso) = compile_test_cs(cs);

        // List 1: UAV dispatch writes 0.75 to plane 0 only; plane 1 stays COMMON and untouched.
        list.reset();
        assert!(list.transition(tex.0, UNORDERED_ACCESS).is_none());
        assert_eq!(twin.planes[1].state.load(Relaxed), 0, "inactive plane 1 must stay COMMON");
        // SAFETY: open list with live root signature, PSO, descriptor heap, and plane 0 UAV.
        unsafe {
            list.list.SetComputeRootSignature(&root);
            list.list.SetPipelineState(&pso);
            list.list.SetDescriptorHeaps(&[Some(heap.clone())]);
            list.list.SetComputeRootDescriptorTable(0, gpu_start);
            list.list.Dispatch(1, 1, 1);
        }
        move_and_barrier(&list, &twin.planes[0], 0);
        list.close();
        host.execute(&[&list]);

        assert_eq!(twin.planes[1].state.load(Relaxed), 0);
        assert_eq!(
            [twin.plane_newer[0].load(Relaxed), twin.plane_newer[1].load(Relaxed)],
            [TWIN, DEPTH]
        );

        // Late UAV on plane 1 while plane 0 is TWIN and plane 1 is still DEPTH (unseeded on twin).
        assert!(uav(tex.0, &copy, Some(&uav_words(47, 1)), cpu_at(1)).is_some());
        assert_eq!(twin.uav_planes.load(Relaxed), 0b11);

        // List 2: prelude TWIN -> DEPTH must copy plane 0 only and NOT clobber stencil 0x5A!
        list.reset();
        transition_title_barrier(&list, tex.0, COPY_SOURCE);
        let (rb_d0, fp_d0) = copy_subresource_to_readback(&list, &copy.texture, 0);
        let (rb_d1, fp_d1) = copy_subresource_to_readback(&list, &copy.texture, 1);
        transition_title_barrier(&list, tex.0, 0);
        list.close();
        host.execute(&[&list]);

        let got_d24 = read_mapped_rows::<u32>(&rb_d0, &fp_d0);
        let got_s8 = read_mapped_rows::<u8>(&rb_d1, &fp_d1);
        let expected_d24 = f32_to_d24_ref(0.75);
        for i in 0..TEXELS {
            assert_eq!(got_d24[i] & 0x00ff_ffff, expected_d24, "depth texel {i}");
            assert_eq!(got_s8[i], 0x5A, "stencil texel {i} must not be clobbered");
        }
        assert_eq!(
            [twin.plane_newer[0].load(Relaxed), twin.plane_newer[1].load(Relaxed)],
            [BOTH, DEPTH]
        );

        // List 3: route(TWIN) prelude DEPTH -> TWIN copies plane 1 (seeding stencil 0x5A onto twin).
        list.reset();
        assert!(list.transition(tex.0, UNORDERED_ACCESS).is_none());
        let twin_rbs = readback_twin_planes(&list, twin);
        list.close();
        host.execute(&[&list]);

        let got_f32 = read_mapped_rows::<f32>(&twin_rbs[0].0, &twin_rbs[0].1);
        let got_twin_s8 = read_mapped_rows::<u8>(&twin_rbs[1].0, &twin_rbs[1].1);
        let expected_f32 = d24_to_f32_ref(expected_d24);
        for i in 0..TEXELS {
            assert_eq!(got_f32[i].to_bits(), expected_f32.to_bits(), "twin plane 0 texel {i}");
            assert_eq!(got_twin_s8[i], 0x5A, "twin plane 1 texel {i} seeded from depth");
        }
        assert_eq!(
            [twin.plane_newer[0].load(Relaxed), twin.plane_newer[1].load(Relaxed)],
            [TWIN, TWIN]
        );
        assert_eq!(host.log_messages(), None, "D3D12 debug layer reported errors");
    }
}
