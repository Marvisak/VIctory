//! The GPU frame: the scene of [`crate::scene`]
//! drawn as a title draws, through the console's D3D12 (`d3d12_xs.dll`):
//! device, queue, allocators and lists, descriptor heaps, root signatures,
//! a graphics pipeline from a stream and a compute pipeline (their shaders
//! the hand-assembled GFX10 programs of [`crate::gfx10`]), vertex, index
//! and constant buffers in title memory, a texture uploaded with a copy, a
//! depth buffer, four indexed draws, a compute pass, a fence; then the
//! compute pass's output copied to a read-back buffer and read.
//!
//! The device's methods are reached by the byte offsets of their slots,
//! the layout VIctory implements (`crates/victory-gpu/src/device.rs` and
//! beside it): the PC interface's order (public D3D12 reference), console
//! additions after it. Structures are the public D3D12 ones.
//!
//! What is checked: each call's result, then the frame against the same
//! scene drawn natively on WARP by the harness from equivalent HLSL
//! (within a small tolerance), and pixels whose value is exact by
//! construction (the clear colour; the glowing pyramid, where it is in
//! front, and not where a cube hides it).

use std::ffi::c_void;

use crate::rt::{Guid, Hresult, S_OK};
use crate::scene::{self, HEIGHT, WIDTH};
use crate::tap::Tap;

type Obj = *mut c_void;

#[link(name = "d3d12_xs", kind = "raw-dylib")]
unsafe extern "system" {
    fn D3D12XboxCreateDevice(
        adapter: *const c_void,
        parameters: *const u32,
        iid: *const Guid,
        out: *mut Obj,
    ) -> Hresult;
    fn D3D12SerializeRootSignature(
        desc: *const RootSignatureDesc,
        version: u32,
        blob: *mut Obj,
        errors: *mut Obj,
    ) -> Hresult;
}

/// Calls the method at byte offset `$at` of `$obj`'s table:
/// `call!(obj, 0x40, fn(u32) -> Hresult, 7)`.
macro_rules! call {
    ($obj:expr, $at:expr, fn($($t:ty),* $(,)?) $(-> $r:ty)?, $($arg:expr),* $(,)?) => {{
        let obj: Obj = $obj;
        // SAFETY: (the caller) `obj` is a live object whose table has this
        // method at this offset, with this signature.
        unsafe {
            let table = *obj.cast::<*const *const c_void>();
            let f: extern "system" fn(Obj $(, $t)*) $(-> $r)? =
                std::mem::transmute(*table.add($at / 8));
            f(obj $(, $arg)*)
        }
    }};
}

// Interface ids (public D3D12 headers).
const DEVICE: Guid = Guid(
    0x189819f1,
    0x1db6,
    0x4b57,
    [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
);
const QUEUE: Guid = Guid(
    0x0ec870a6,
    0x5d7e,
    0x4c22,
    [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
);
const ALLOCATOR: Guid = Guid(
    0x6102dee4,
    0xaf59,
    0x4b09,
    [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
);
const LIST: Guid = Guid(
    0x5b160d0f,
    0xac1b,
    0x4185,
    [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
);
const HEAP: Guid = Guid(
    0x8efb471d,
    0x616c,
    0x4f49,
    [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
);
const ROOT: Guid = Guid(
    0xc54a6b66,
    0x72df,
    0x4ee8,
    [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
);
const RESOURCE: Guid = Guid(
    0x696442be,
    0xa72e,
    0x4059,
    [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
);
const FENCE: Guid = Guid(
    0x0a753dcf,
    0xc4d8,
    0x4b91,
    [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
);
const PIPELINE: Guid = Guid(
    0x765a30f3,
    0xf624,
    0x4c6f,
    [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
);

/// The device interface version VIctory's layouts follow (2.4).
const VERSION: u32 = 0x0002_0004;

// DXGI formats.
const RGBA8: u32 = 28;
const D32: u32 = 40;
const RG32F: u32 = 16;
const RGB32F: u32 = 6;
const R16U: u32 = 57;
const R32_TYPELESS: u32 = 39;

// Resource states.
const COMMON: u32 = 0;
const UNORDERED: u32 = 0x8;
const RENDER_TARGET: u32 = 0x4;
const DEPTH_WRITE: u32 = 0x10;
const NON_PIXEL: u32 = 0x40;
const PIXEL: u32 = 0x80;
const COPY_DEST: u32 = 0x400;
const COPY_SOURCE: u32 = 0x800;
const GENERIC_READ: u32 = 0xAC3;

/// Bytes the read-back buffer holds: four floats per pixel.
/// The compute output's bytes for a `width` x `height` frame (four floats a
/// pixel).
fn frame_bytes(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * 16
}

/// Draws, and one submission, each step of the demo's stress scale adds.
pub const DRAWS_A_STEP: usize = 64;
/// Views the checked frame's heap holds; the stress scale's come after.
const VIEWS: u64 = 16;
/// The terrain's view (constants slot 5), in the demo's window only.
const TERRAIN_VIEW: u64 = 9;

/// The draws the stress scale adds to the checked frame's four.
pub fn extra_draws(scale: u32) -> usize {
    DRAWS_A_STEP * (scale.max(1) as usize - 1)
}

#[repr(C)]
struct HeapProperties {
    kind: u32,
    cpu_page: u32,
    pool: u32,
    creation_nodes: u32,
    visible_nodes: u32,
}

#[repr(C)]
struct ResourceDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth: u16,
    mips: u16,
    format: u32,
    samples: u32,
    quality: u32,
    layout: u32,
    flags: u32,
}

#[repr(C)]
struct ClearValue {
    format: u32,
    value: [f32; 4],
}

#[repr(C)]
struct Range {
    kind: u32,
    count: u32,
    base: u32,
    space: u32,
    offset: u32,
}

/// A root parameter: its type, then (a descriptor table) the ranges'
/// count and address, at 8 and 16 (the union's pointer alignment).
#[repr(C)]
struct Param {
    kind: u32,
    _pad: u32,
    ranges_count: u32,
    _pad2: u32,
    ranges: *const Range,
    visibility: u32,
}

#[repr(C)]
struct RootSignatureDesc {
    count: u32,
    params: *const Param,
    statics_count: u32,
    statics: *const c_void,
    flags: u32,
}

#[repr(C)]
struct Barrier {
    kind: u32,
    flags: u32,
    resource: Obj,
    subresource: u32,
    before: u32,
    after: u32,
}

#[repr(C)]
struct CopyLocation {
    resource: Obj,
    kind: u32,
    offset: u64,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    row_pitch: u32,
}

#[repr(C)]
struct BufferView {
    address: u64,
    size: u32,
    stride_or_format: u32,
}

#[repr(C)]
struct InputElement {
    name: *const u8,
    index: u32,
    format: u32,
    slot: u32,
    offset: u32,
    class: u32,
    step: u32,
}

#[repr(C)]
struct Plane {
    token: u64,
    count: u32,
    resources: *const Obj,
}

#[repr(C)]
struct ComputeDesc {
    root: Obj,
    code: *const u8,
    size: usize,
    nodes: u32,
    cached: *const u8,
    cached_size: usize,
    flags: u32,
}

/// A pipeline stream (public `D3D12_PIPELINE_STATE_STREAM_DESC`): each
/// subobject a type then its data, aligned to pointers.
#[derive(Default)]
struct Stream(Vec<u8>);

impl Stream {
    fn push(&mut self, kind: u32, data: &[u8], pointers: bool) {
        self.0.extend(kind.to_le_bytes());
        if pointers {
            self.0.extend([0; 4]);
        }
        self.0.extend(data);
        self.0.resize(self.0.len().next_multiple_of(8), 0);
    }
}

fn words(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn pointer(p: *const u8, size: usize) -> Vec<u8> {
    let mut out = (p as u64).to_le_bytes().to_vec();
    out.extend((size as u64).to_le_bytes());
    out
}

/// What the run hands back: the checked frame read back (RGBA floats per
/// pixel), and the window's last frame if it ran for a time (empty if not).
#[derive(Default)]
pub struct Run {
    pub image: Vec<f32>,
    pub window_image: Vec<f32>,
}

/// Everything the frame uses.
struct Gpu {
    /// The frame's size in pixels.
    width: u32,
    height: u32,
    /// The demo's stress scale (1: the checked frame's work).
    scale: u32,
    /// Whether the terrain is drawn (the demo's window; never the checked
    /// frame, which the reference draws without it).
    scenery: bool,
    /// Whether the texture shown was last left in the state for presenting.
    shown_common: bool,
    /// Whether the copy list has been closed once (it starts open).
    copy_closed: bool,
    device: Obj,
    queue: Obj,
    allocators: [Obj; 2],
    lists: [Obj; 2],
    fence: Obj,
    fence_value: u64,
    views: Obj,
    samplers: Obj,
    targets: Obj,
    depths: Obj,
    view_cpu: u64,
    view_gpu: u64,
    sampler_gpu: u64,
    target_cpu: u64,
    depth_cpu: u64,
    graphics_root: Obj,
    compute_root: Obj,
    graphics: Obj,
    compute: Obj,
    vertices: Obj,
    indices: Obj,
    constants: Obj,
    constants_cpu: *mut u8,
    texture: Obj,
    upload: Obj,
    target: Obj,
    depth: Obj,
    shown: Obj,
    output: Obj,
    readback: Obj,
    geometry: scene::Geometry,
    /// The shader containers (pipelines point at them while created).
    _shaders: [Vec<u8>; 3],
}

const VIEW_SIZE: u64 = 32;

impl Gpu {
    fn heap(&self, t: &mut Tap, kind: u32, count: u32, visible: bool, what: &str) -> Obj {
        // The description, then an empty placement: newer titles follow it
        // with memory of their own for the descriptors (address, size), and
        // the runtime reads those 16 bytes too.
        let desc = [kind, count, u32::from(visible), 0, 0, 0, 0, 0];
        let mut heap = std::ptr::null_mut();
        let hr = call!(
            self.device,
            0x70,
            fn(*const u32, *const Guid, *mut Obj) -> Hresult,
            desc.as_ptr(),
            &HEAP,
            &mut heap
        );
        t.eq(hr, S_OK, &format!("gpu: CreateDescriptorHeap ({what})"));
        heap
    }

    /// A committed resource: `(heap type, description, state, clear)`.
    fn resource(
        &self,
        t: &mut Tap,
        heap: u32,
        desc: ResourceDesc,
        state: u32,
        clear: Option<ClearValue>,
        what: &str,
    ) -> Obj {
        let props = HeapProperties {
            kind: heap,
            cpu_page: 0,
            pool: 0,
            creation_nodes: 0,
            visible_nodes: 0,
        };
        let clear_ptr = clear
            .as_ref()
            .map_or(std::ptr::null(), |c| c as *const ClearValue);
        let mut out = std::ptr::null_mut();
        let hr = call!(
            self.device,
            0xd8,
            fn(
                *const HeapProperties,
                u32,
                *const ResourceDesc,
                u32,
                *const ClearValue,
                *const Guid,
                *mut Obj,
            ) -> Hresult,
            &props,
            0,
            &desc,
            state,
            clear_ptr,
            &RESOURCE,
            &mut out
        );
        t.eq(hr, S_OK, &format!("gpu: CreateCommittedResource ({what})"));
        out
    }

    fn map(&self, resource: Obj) -> *mut u8 {
        let mut at = std::ptr::null_mut();
        let hr = call!(
            resource,
            0x40,
            fn(u32, *const [usize; 2], *mut *mut u8) -> Hresult,
            0,
            std::ptr::null(),
            &mut at
        );
        if hr != S_OK { std::ptr::null_mut() } else { at }
    }

    fn address(resource: Obj) -> u64 {
        call!(resource, 0x58, fn() -> u64,)
    }

    fn barrier(&self, list: Obj, resource: Obj, before: u32, after: u32) {
        let b = Barrier {
            kind: 0,
            flags: 0,
            resource,
            subresource: u32::MAX,
            before,
            after,
        };
        call!(list, 0xd0, fn(u32, *const Barrier) -> u32, 1, &b);
    }

    /// Closes list `n`, runs it and waits for it with the fence; whether the
    /// fence reached the value signalled.
    fn submit(&mut self, n: usize) -> (Hresult, bool) {
        let list = self.lists[n];
        let hr = call!(list, 0x48, fn() -> Hresult,);
        call!(self.queue, 0x50, fn(u32, *const Obj), 1, &list);
        self.fence_value += 1;
        call!(
            self.queue,
            0x70,
            fn(Obj, u64) -> Hresult,
            self.fence,
            self.fence_value
        );
        // A null event: wait here until the fence reaches the value.
        call!(
            self.fence,
            0x48,
            fn(u64, *mut c_void) -> Hresult,
            self.fence_value,
            std::ptr::null_mut()
        );
        let done = call!(self.fence, 0x40, fn() -> u64,);
        (hr, done >= self.fence_value)
    }

    fn reset(&self, n: usize, pipeline: Obj) -> Hresult {
        call!(self.allocators[n], 0x40, fn() -> Hresult,);
        call!(
            self.lists[n],
            0x50,
            fn(Obj, Obj) -> Hresult,
            self.allocators[n],
            pipeline
        )
    }
}

fn root_signature(t: &mut Tap, device: Obj, params: &[Param], flags: u32, what: &str) -> Obj {
    let desc = RootSignatureDesc {
        count: params.len() as u32,
        params: params.as_ptr(),
        statics_count: 0,
        statics: std::ptr::null(),
        flags,
    };
    let (mut blob, mut errors) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: a valid description and out pointers.
    let hr = unsafe { D3D12SerializeRootSignature(&desc, 1, &mut blob, &mut errors) };
    t.eq(
        hr,
        S_OK,
        &format!("gpu: D3D12SerializeRootSignature ({what})"),
    );
    if hr != S_OK || blob.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = call!(blob, 0x18, fn() -> *const u8,);
    let size = call!(blob, 0x20, fn() -> usize,);
    let mut root = std::ptr::null_mut();
    let hr = call!(
        device,
        0x80,
        fn(u32, *const u8, usize, *const Guid, *mut Obj) -> Hresult,
        0,
        bytes,
        size,
        &ROOT,
        &mut root
    );
    t.eq(hr, S_OK, &format!("gpu: CreateRootSignature ({what})"));
    call!(blob, 0x10, fn() -> u32,);
    root
}

fn table(ranges: &[Range]) -> Param {
    Param {
        kind: 0,
        _pad: 0,
        _pad2: 0,
        ranges_count: ranges.len() as u32,
        ranges: ranges.as_ptr(),
        visibility: 0,
    }
}

fn range(kind: u32, base: u32) -> Range {
    Range {
        kind,
        count: 1,
        base,
        space: 0,
        offset: u32::MAX,
    }
}

fn buffer(width: u64, flags: u32) -> ResourceDesc {
    ResourceDesc {
        dimension: 1,
        alignment: 0,
        width,
        height: 1,
        depth: 1,
        mips: 1,
        format: 0,
        samples: 1,
        quality: 0,
        layout: 1,
        flags,
    }
}

fn texture(width: u32, height: u32, format: u32, flags: u32) -> ResourceDesc {
    ResourceDesc {
        dimension: 3,
        alignment: 0,
        width: u64::from(width),
        height,
        depth: 1,
        mips: 1,
        format,
        samples: 1,
        quality: 0,
        layout: 0,
        flags,
    }
}

fn setup(t: &mut Tap, width: u32, height: u32, scale: u32, scenery: bool) -> Option<Gpu> {
    let mut parameters = [0u32; 32];
    parameters[0] = VERSION;
    let mut device = std::ptr::null_mut();
    // SAFETY: parameters and out pointers outlive the call.
    let hr = unsafe {
        D3D12XboxCreateDevice(std::ptr::null(), parameters.as_ptr(), &DEVICE, &mut device)
    };
    t.eq(hr, S_OK, "gpu: D3D12XboxCreateDevice");
    if hr != S_OK || device.is_null() {
        return None;
    }
    let mut queue = std::ptr::null_mut();
    let desc = [0u32; 4]; // direct, normal priority
    let hr = call!(
        device,
        0x40,
        fn(*const u32, *const Guid, *mut Obj) -> Hresult,
        desc.as_ptr(),
        &QUEUE,
        &mut queue
    );
    t.eq(hr, S_OK, "gpu: CreateCommandQueue");
    let mut allocators = [std::ptr::null_mut(); 2];
    let mut lists = [std::ptr::null_mut(); 2];
    let mut made = 0;
    for n in 0..2 {
        made += i32::from(
            call!(
                device,
                0x48,
                fn(u32, *const Guid, *mut Obj) -> Hresult,
                0,
                &ALLOCATOR,
                &mut allocators[n]
            ) == S_OK,
        );
        made += i32::from(
            call!(
                device,
                0x60,
                fn(u32, u32, Obj, Obj, *const Guid, *mut Obj) -> Hresult,
                0,
                0,
                allocators[n],
                std::ptr::null_mut(),
                &LIST,
                &mut lists[n]
            ) == S_OK,
        );
    }
    t.eq(
        made,
        4,
        "gpu: CreateCommandAllocator and CreateCommandList, twice",
    );
    let mut fence = std::ptr::null_mut();
    let hr = call!(
        device,
        0x120,
        fn(u64, u32, *const Guid, *mut Obj) -> Hresult,
        0,
        0,
        &FENCE,
        &mut fence
    );
    t.eq(hr, S_OK, "gpu: CreateFence");
    if queue.is_null() || fence.is_null() || made != 4 {
        return None;
    }

    // Root signatures: per draw the scene's constants (b0), the draw's
    // (b1), the texture (t0) and its sampler (s0), each a table; for the
    // compute pass one table of the frame (t0), the texture shown (u0) and
    // the read-back buffer (u1).
    let ranges = [range(2, 0), range(2, 1), range(0, 0), range(3, 0)];
    let params: Vec<Param> = ranges
        .iter()
        .map(|r| table(std::slice::from_ref(r)))
        .collect();
    // Input-assembler input layout allowed.
    let graphics_root = root_signature(t, device, &params, 1, "graphics");
    let compute_ranges = [
        Range {
            kind: 0,
            count: 1,
            base: 0,
            space: 0,
            offset: 0,
        },
        Range {
            kind: 1,
            count: 1,
            base: 0,
            space: 0,
            offset: 1,
        },
        Range {
            kind: 1,
            count: 1,
            base: 1,
            space: 0,
            offset: 2,
        },
    ];
    let compute_root = root_signature(t, device, &[table(&compute_ranges)], 0, "compute");

    let shaders = [
        crate::gfx10::vertex_container(),
        crate::gfx10::pixel_container(),
        crate::gfx10::compute_container(width),
    ];
    // The graphics pipeline, as a stream.
    let names = [c"POSITION", c"NORMAL", c"TEXCOORD"];
    let elements: Vec<InputElement> = [(RGB32F, 0), (RGB32F, 12), (RG32F, 24)]
        .iter()
        .zip(names)
        .map(|(&(format, offset), name)| InputElement {
            name: name.as_ptr().cast(),
            index: 0,
            format,
            slot: 0,
            offset,
            class: 0,
            step: 0,
        })
        .collect();
    let mut s = Stream::default();
    s.push(0, &(graphics_root as u64).to_le_bytes(), true);
    s.push(1, &pointer(shaders[0].as_ptr(), shaders[0].len()), true);
    s.push(2, &pointer(shaders[1].as_ptr(), shaders[1].len()), true);
    // Rasterizer: solid, cull back faces (clockwise is front), depth clip.
    s.push(10, &words(&[3, 3, 0, 0, 0, 0, 1, 0, 0, 0, 0]), false);
    // Depth: test less, write; no stencil.
    let keep = [1, 1, 1, 8];
    let mut depth = vec![1, 1, 2, 0, 0xffff];
    depth.extend(keep);
    depth.extend(keep);
    s.push(11, &words(&depth), false);
    let mut layout = pointer(elements.as_ptr().cast(), 0);
    layout[8..16].copy_from_slice(&(elements.len() as u64).to_le_bytes());
    s.push(12, &layout, true);
    s.push(14, &3u32.to_le_bytes(), false); // triangles
    s.push(15, &words(&[RGBA8, 0, 0, 0, 0, 0, 0, 0, 1]), false);
    s.push(16, &D32.to_le_bytes(), false);
    s.push(17, &words(&[1, 0]), false);
    let stream = [s.0.len() as u64, s.0.as_ptr() as u64];
    let mut graphics = std::ptr::null_mut();
    let hr = call!(
        device,
        0x2b0,
        fn(*const [u64; 2], *const Guid, *mut Obj) -> Hresult,
        &stream,
        &PIPELINE,
        &mut graphics
    );
    t.eq(
        hr,
        S_OK,
        "gpu: CreatePipelineState (vertex and pixel shaders, from a stream)",
    );
    let desc = ComputeDesc {
        root: compute_root,
        code: shaders[2].as_ptr(),
        size: shaders[2].len(),
        nodes: 0,
        cached: std::ptr::null(),
        cached_size: 0,
        flags: 0,
    };
    let mut compute = std::ptr::null_mut();
    let hr = call!(
        device,
        0x58,
        fn(*const ComputeDesc, *const Guid, *mut Obj) -> Hresult,
        &desc,
        &PIPELINE,
        &mut compute
    );
    t.eq(hr, S_OK, "gpu: CreateComputePipelineState");

    let null = std::ptr::null_mut();
    let mut g = Gpu {
        width,
        height,
        scale,
        scenery,
        shown_common: false,
        copy_closed: false,
        device,
        queue,
        allocators,
        lists,
        fence,
        fence_value: 0,
        views: null,
        samplers: null,
        targets: null,
        depths: null,
        view_cpu: 0,
        view_gpu: 0,
        sampler_gpu: 0,
        target_cpu: 0,
        depth_cpu: 0,
        graphics_root,
        compute_root,
        graphics,
        compute,
        vertices: null,
        indices: null,
        constants: null,
        constants_cpu: std::ptr::null_mut(),
        texture: null,
        upload: null,
        target: null,
        depth: null,
        shown: null,
        output: null,
        readback: null,
        geometry: scene::Geometry::new(),
        _shaders: shaders,
    };

    // Descriptor heaps: views (0 scene constants, 1-4 the draws', 5 the
    // texture, 6 the frame, 7 the texture shown, 8 the read-back buffer),
    // one sampler, one render target, one depth buffer.
    g.views = g.heap(
        t,
        0,
        VIEWS as u32 + extra_draws(scale) as u32,
        true,
        "views",
    );
    g.samplers = g.heap(t, 1, 1, true, "samplers");
    g.targets = g.heap(t, 2, 1, false, "render targets");
    g.depths = g.heap(t, 3, 1, false, "depth");
    if [g.views, g.samplers, g.targets, g.depths]
        .iter()
        .any(|h| h.is_null())
    {
        return None;
    }
    let increment = |kind| call!(device, 0x78, fn(u32) -> u32, kind);
    t.eq(
        increment(0),
        VIEW_SIZE as u32,
        "gpu: a view descriptor takes 32 bytes",
    );
    // The console's: the handles themselves, not through a pointer.
    let start = |heap: Obj, at: usize| call!(heap, at, fn() -> u64,);
    g.view_cpu = start(g.views, 0x48);
    g.view_gpu = start(g.views, 0x50);
    g.sampler_gpu = start(g.samplers, 0x50);
    g.target_cpu = start(g.targets, 0x48);
    g.depth_cpu = start(g.depths, 0x48);

    // Buffers in upload memory, written by the CPU.
    let geometry = &g.geometry;
    let vertex_bytes = (geometry.vertices.len() * size_of::<scene::Vertex>()) as u64;
    let index_bytes = (geometry.indices.len() * 2).next_multiple_of(4) as u64;
    g.vertices = g.resource(
        t,
        2,
        buffer(vertex_bytes, 0),
        GENERIC_READ,
        None,
        "vertex buffer",
    );
    g.indices = g.resource(
        t,
        2,
        buffer(index_bytes, 0),
        GENERIC_READ,
        None,
        "index buffer",
    );
    g.constants = g.resource(
        t,
        2,
        buffer(256 * (6 + extra_draws(scale) as u64), 0),
        GENERIC_READ,
        None,
        "constant buffer",
    );
    let texel_bytes = u64::from(scene::TEXTURE * scene::TEXTURE * 4);
    g.upload = g.resource(
        t,
        2,
        buffer(texel_bytes, 0),
        GENERIC_READ,
        None,
        "texture upload buffer",
    );
    g.texture = g.resource(
        t,
        1,
        texture(scene::TEXTURE, scene::TEXTURE, RGBA8, 0),
        COPY_DEST,
        None,
        "texture",
    );
    let clear = scene::CLEAR.map(|c| f32::from(c) / 255.0);
    g.target = g.resource(
        t,
        1,
        texture(width, height, RGBA8, 1),
        RENDER_TARGET,
        Some(ClearValue {
            format: RGBA8,
            value: clear,
        }),
        "render target",
    );
    g.depth = g.resource(
        t,
        1,
        texture(width, height, D32, 2),
        DEPTH_WRITE,
        Some(ClearValue {
            format: D32,
            value: [1.0, 0.0, 0.0, 0.0],
        }),
        "depth buffer",
    );
    g.shown = g.resource(
        t,
        1,
        texture(width, height, RGBA8, 4),
        UNORDERED,
        None,
        "texture shown",
    );
    g.output = g.resource(
        t,
        1,
        buffer(frame_bytes(width, height), 4),
        UNORDERED,
        None,
        "compute output buffer",
    );
    g.readback = g.resource(
        t,
        3,
        buffer(frame_bytes(width, height), 0),
        COPY_DEST,
        None,
        "read-back buffer",
    );
    let all = [
        g.vertices,
        g.indices,
        g.constants,
        g.upload,
        g.texture,
        g.target,
        g.depth,
        g.shown,
        g.output,
        g.readback,
    ];
    if all.iter().any(|r| r.is_null()) {
        return None;
    }
    let write = |r: Obj, bytes: &[u8]| {
        let at = g.map(r);
        if !at.is_null() {
            // SAFETY: the mapping holds the resource's bytes, at least these.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len()) };
        }
        !at.is_null()
    };
    // SAFETY: plain data.
    let vertex_data = unsafe {
        std::slice::from_raw_parts(
            geometry.vertices.as_ptr().cast::<u8>(),
            vertex_bytes as usize,
        )
    };
    let index_data: Vec<u8> = geometry
        .indices
        .iter()
        .flat_map(|i| i.to_le_bytes())
        .collect();
    let mapped = write(g.vertices, vertex_data)
        && write(g.indices, &index_data)
        && write(g.upload, &scene::texels());
    g.constants_cpu = g.map(g.constants);
    t.check(
        mapped && !g.constants_cpu.is_null(),
        "gpu: upload buffers map",
        || "Map failed".into(),
    );
    if !mapped || g.constants_cpu.is_null() {
        return None;
    }

    // Views.
    let view = |n: u64| g.view_cpu + n * VIEW_SIZE;
    let constants = Gpu::address(g.constants);
    for n in 0..5 {
        let desc = [constants + 256 * n, 256];
        call!(device, 0x88, fn(*const [u64; 2], u64), &desc, view(n));
    }
    // The terrain's constants (slot 5) at view 9, for the demo's window.
    if scenery {
        let desc = [constants + 256 * 5, 256];
        call!(
            device,
            0x88,
            fn(*const [u64; 2], u64),
            &desc,
            view(TERRAIN_VIEW)
        );
    }
    // Format, 2D, the default component mapping, then (from 16 bytes) the
    // most detailed level 0 and one level.
    let srv = [RGBA8, 4, 0x1688, 0, 0, 1, 0, 0, 0, 0];
    call!(
        device,
        0x90,
        fn(Obj, *const [u32; 10], u64),
        g.texture,
        &srv,
        view(5)
    );
    call!(
        device,
        0x90,
        fn(Obj, *const [u32; 10], u64),
        g.target,
        &srv,
        view(6)
    );
    let uav = [RGBA8, 4, 0, 0, 0, 0, 0, 0, 0, 0];
    call!(
        device,
        0x98,
        fn(Obj, Obj, *const [u32; 10], u64),
        g.shown,
        std::ptr::null_mut(),
        &uav,
        view(7)
    );
    // Raw: R32 elements, the raw flag.
    let raw = [
        R32_TYPELESS,
        1,
        0,
        0,
        (frame_bytes(width, height) / 4) as u32,
        0,
        0,
        0,
        1,
        0,
    ];
    call!(
        device,
        0x98,
        fn(Obj, Obj, *const [u32; 10], u64),
        g.output,
        std::ptr::null_mut(),
        &raw,
        view(8)
    );
    // Linear filtering, wrapping.
    let mut sampler = [0x15u32, 1, 1, 1, 0, 1, 1, 0, 0, 0, 0, 0, f32::MAX.to_bits()];
    sampler[4] = 0f32.to_bits();
    call!(
        device,
        0xb0,
        fn(*const [u32; 13], u64),
        &sampler,
        start(g.samplers, 0x48)
    );
    call!(
        device,
        0xa0,
        fn(Obj, *const u8, u64),
        g.target,
        std::ptr::null(),
        g.target_cpu
    );
    call!(
        device,
        0xa8,
        fn(Obj, *const u8, u64),
        g.depth,
        std::ptr::null(),
        g.depth_cpu
    );
    Some(g)
}

impl Gpu {
    /// Binds what the frame's draws use on `list`: heaps, root signature,
    /// targets, viewport, pipeline, vertex and index buffers, and the
    /// tables every draw shares (the scene's constants, the texture, the
    /// sampler).
    fn bind_pass(&self, list: Obj) {
        let heaps = [self.views, self.samplers];
        call!(list, 0x100, fn(u32, *const Obj), 2, heaps.as_ptr());
        call!(list, 0x110, fn(Obj), self.graphics_root);
        call!(
            list,
            0x190,
            fn(u32, *const u64, i32, *const u64),
            1,
            &self.target_cpu,
            0,
            &self.depth_cpu
        );
        let viewport = [0.0, 0.0, self.width as f32, self.height as f32, 0.0, 1.0f32];
        call!(list, 0xa8, fn(u32, *const [f32; 6]), 1, &viewport);
        let scissor = [0, 0, self.width as i32, self.height as i32];
        call!(list, 0xb0, fn(u32, *const [i32; 4]), 1, &scissor);
        call!(list, 0xa0, fn(u32), 4); // triangle list
        call!(list, 0xc8, fn(Obj), self.graphics);
        let vertices = BufferView {
            address: Gpu::address(self.vertices),
            size: (self.geometry.vertices.len() * 32) as u32,
            stride_or_format: scene::STRIDE,
        };
        call!(
            list,
            0x180,
            fn(u32, u32, *const BufferView),
            0,
            1,
            &vertices
        );
        let indices = BufferView {
            address: Gpu::address(self.indices),
            size: (self.geometry.indices.len() * 2) as u32,
            stride_or_format: R16U,
        };
        call!(list, 0x178, fn(*const BufferView), &indices);
        let table = |n: u64| self.view_gpu + n * VIEW_SIZE;
        call!(list, 0x120, fn(u32, u64), 0, table(0));
        call!(list, 0x120, fn(u32, u64), 2, table(5));
        call!(list, 0x120, fn(u32, u64), 3, self.sampler_gpu);
    }

    /// Records and runs one frame at time `at`: the draws, the compute pass
    /// (and the texture's upload if `first`); then copies the compute
    /// output to the read-back buffer, unless `present`, which leaves the
    /// texture shown in the state for presenting instead.
    fn frame(&mut self, at: f32, first: bool, present: bool) -> (Hresult, Hresult, bool) {
        let (scene, draws) = scene::frame(at);
        // SAFETY: the constant buffer is mapped and at least 1,536 bytes long.
        unsafe {
            let base = self.constants_cpu;
            std::ptr::copy_nonoverlapping((&raw const scene).cast::<u8>(), base, 256);
            for (n, (_, mesh)) in draws.iter().enumerate() {
                std::ptr::copy_nonoverlapping(
                    (&raw const *mesh).cast::<u8>(),
                    base.add(256 * (n + 1)),
                    256,
                );
            }
            let terrain = scene::terrain();
            std::ptr::copy_nonoverlapping(
                (&raw const terrain).cast::<u8>(),
                base.add(256 * 5),
                256,
            );
        }
        let list = self.lists[0];
        if !first {
            self.reset(0, std::ptr::null_mut());
        }
        if first {
            let dst = CopyLocation {
                resource: self.texture,
                kind: 0,
                offset: 0,
                format: 0,
                width: 0,
                height: 0,
                depth: 0,
                row_pitch: 0,
            };
            let src = CopyLocation {
                resource: self.upload,
                kind: 1,
                offset: 0,
                format: RGBA8,
                width: scene::TEXTURE,
                height: scene::TEXTURE,
                depth: 1,
                row_pitch: scene::TEXTURE * 4,
            };
            call!(
                list,
                0x80,
                fn(*const CopyLocation, u32, u32, u32, *const CopyLocation, *const u32),
                &dst,
                0,
                0,
                0,
                &src,
                std::ptr::null()
            );
            self.barrier(list, self.texture, COPY_DEST, PIXEL);
        }
        if self.shown_common {
            self.barrier(list, self.shown, COMMON, UNORDERED);
            self.shown_common = false;
        }
        let clear = scene::CLEAR.map(|c| f32::from(c) / 255.0);
        call!(
            list,
            0x1a0,
            fn(u64, *const [f32; 4], u32, *const [i32; 4]),
            self.target_cpu,
            &clear,
            0,
            std::ptr::null()
        );
        call!(
            list,
            0x198,
            fn(u64, u32, f32, u8, u32, *const [i32; 4]),
            self.depth_cpu,
            1,
            1.0,
            0,
            0,
            std::ptr::null()
        );
        self.bind_pass(list);
        let view_gpu = self.view_gpu;
        let table = move |n: u64| view_gpu + n * VIEW_SIZE;
        for (n, &(mesh, _)) in draws.iter().enumerate() {
            let m = self.geometry.meshes[mesh];
            call!(list, 0x120, fn(u32, u64), 1, table(n as u64 + 1));
            call!(
                list,
                0x68,
                fn(u32, u32, u32, i32, u32),
                m.indices,
                1,
                m.first_index,
                m.base_vertex,
                0
            );
        }
        if self.scenery {
            let m = self.geometry.meshes[3];
            call!(list, 0x120, fn(u32, u64), 1, table(TERRAIN_VIEW));
            call!(
                list,
                0x68,
                fn(u32, u32, u32, i32, u32),
                m.indices,
                1,
                m.first_index,
                m.base_vertex,
                0
            );
        }
        // The stress scale's objects (none in the checked frame): each a
        // draw whose constants are written into title memory and whose view
        // is made in the heap this frame, DRAWS_A_STEP to a submission, each
        // submission binding the pass again as a title's lists do.
        let crowd = scene::crowd(at, extra_draws(self.scale));
        let mut closed = S_OK;
        let mut done = true;
        let constants = Gpu::address(self.constants);
        for (k, chunk) in crowd.chunks(DRAWS_A_STEP).enumerate() {
            let (hr, reached) = self.submit(0);
            if hr != S_OK {
                closed = hr;
            }
            done &= reached;
            self.reset(0, std::ptr::null_mut());
            self.bind_pass(list);
            for (j, (mesh, mesh_constants)) in chunk.iter().enumerate() {
                let n = k * DRAWS_A_STEP + j;
                let slot = 6 + n;
                let view = VIEWS + n as u64;
                // SAFETY: the constant buffer is mapped, 256 bytes a slot
                // for each of the scale's draws after the first 6.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        (&raw const *mesh_constants).cast::<u8>(),
                        self.constants_cpu.add(256 * slot),
                        256,
                    );
                }
                let desc = [constants + 256 * slot as u64, 256];
                call!(
                    self.device,
                    0x88,
                    fn(*const [u64; 2], u64),
                    &desc,
                    self.view_cpu + view * VIEW_SIZE
                );
                let m = self.geometry.meshes[*mesh];
                call!(list, 0x120, fn(u32, u64), 1, table(view));
                call!(
                    list,
                    0x68,
                    fn(u32, u32, u32, i32, u32),
                    m.indices,
                    1,
                    m.first_index,
                    m.base_vertex,
                    0
                );
            }
        }
        // The compute pass reads the frame and writes the texture shown and
        // the buffer.
        self.barrier(list, self.target, RENDER_TARGET, NON_PIXEL);
        call!(list, 0x108, fn(Obj), self.compute_root);
        call!(list, 0xc8, fn(Obj), self.compute);
        call!(list, 0x118, fn(u32, u64), 0, table(6));
        call!(
            list,
            0x70,
            fn(u32, u32, u32),
            self.width / scene::GROUP,
            self.height / scene::GROUP,
            1
        );
        self.barrier(list, self.target, NON_PIXEL, RENDER_TARGET);
        if present {
            self.barrier(list, self.shown, UNORDERED, COMMON);
            self.shown_common = true;
        }
        let (last, reached) = self.submit(0);
        let closed = if last == S_OK { closed } else { last };
        let done = done && reached;
        if present {
            return (closed, S_OK, done);
        }

        // The compute output to the read-back buffer (not while only
        // showing frames).
        let copy = self.lists[1];
        if self.copy_closed {
            self.reset(1, std::ptr::null_mut());
        }
        self.barrier(copy, self.output, UNORDERED, COPY_SOURCE);
        call!(
            copy,
            0x78,
            fn(Obj, u64, Obj, u64, u64),
            self.readback,
            0,
            self.output,
            0,
            frame_bytes(self.width, self.height)
        );
        self.barrier(copy, self.output, COPY_SOURCE, UNORDERED);
        let (copied, copy_done) = self.submit(1);
        self.copy_closed = true;
        (closed, copied, done && copy_done)
    }

    fn read(&self) -> Vec<f32> {
        let at = self.map(self.readback);
        if at.is_null() {
            return Vec::new();
        }
        let bytes = frame_bytes(self.width, self.height);
        let mut out = vec![0f32; (bytes / 4) as usize];
        // SAFETY: the mapping holds the buffer's bytes.
        unsafe { std::ptr::copy_nonoverlapping(at, out.as_mut_ptr().cast::<u8>(), bytes as usize) };
        out
    }

    /// Begins a frame (waits for the next frame tick): its token.
    fn begin(&self) -> u64 {
        let mut token = 0;
        call!(
            self.device,
            0x280,
            fn(u32, u32, *const u8, u32, *mut u64) -> Hresult,
            0,
            100,
            std::ptr::null(),
            0,
            &mut token
        );
        token
    }

    fn present(&self, token: u64) -> Hresult {
        let plane = Plane {
            token,
            count: 1,
            resources: &self.shown,
        };
        call!(
            self.queue,
            0xf0,
            fn(u32, *const Plane, *const u8) -> Hresult,
            1,
            &plane,
            std::ptr::null()
        )
    }
}

/// A pixel of `image` as RGBA8.
fn pixel(image: &[f32], x: u32, y: u32) -> [u8; 4] {
    let at = ((y * WIDTH + x) * 4) as usize;
    std::array::from_fn(|c| (image[at + c].clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Runs the frame; `reference` is the WARP image (RGBA floats per pixel),
/// `window_seconds` how long to keep presenting it, animated, afterwards,
/// `window_size` the size it is drawn at there (each a multiple of
/// [`scene::GROUP`]; the checks are of a [`WIDTH`] x [`HEIGHT`] frame) and
/// `scale` how much work each frame there gives VIctory (1: the checked
/// frame's; each step more [`DRAWS_A_STEP`] draws, as many views made and
/// constants written, and a submission).
pub fn run(
    t: &mut Tap,
    reference: Option<&[f32]>,
    window_seconds: u32,
    window_size: (u32, u32),
    scale: u32,
) -> Run {
    let Some(mut g) = setup(t, WIDTH, HEIGHT, 1, false) else {
        return Run::default();
    };
    // Host pipelines are made on first use, by workers; a draw whose
    // pipeline is not made within the frame's time is left out (VIctory's
    // own budget: a title's frames go on meanwhile). Frames are drawn
    // until two in a row read back the same, with the scene in them.
    let mut image = Vec::new();
    let mut frames = 0;
    let mut calls = (S_OK, S_OK, true);
    let background = scene::CLEAR;
    while frames < 30 {
        g.begin();
        let r = g.frame(0.0, frames == 0, false);
        if r != (S_OK, S_OK, true) {
            calls = r;
        }
        frames += 1;
        let next = g.read();
        let drawn = !next.is_empty() && pixel(&next, WIDTH / 2, HEIGHT - 8) != background;
        let same = next == image;
        image = next;
        if drawn && same {
            break;
        }
    }
    t.eq(calls.0, S_OK, "gpu: Close (draw list)");
    t.eq(calls.1, S_OK, "gpu: Close (copy list)");
    t.check(
        calls.2,
        "gpu: the fence reaches each value signalled after ExecuteCommandLists",
        || "it did not".into(),
    );
    t.check(
        frames < 30,
        "gpu: the frame is drawn (pipelines made) and the same in two frames in a row",
        || format!("not after {frames} frames"),
    );
    compare(t, &image, reference);
    exact(t, &image);
    let mut window_image = Vec::new();
    if window_seconds == 0 {
        t.check(
            true,
            "gpu: PresentX # SKIP no window asked for (VICTORY_CONFORMANCE_WINDOW)",
            String::new,
        );
    } else {
        // The window's frames are drawn at its own size, with the terrain
        // and the stress scale's work, by a second set of resources and
        // pipelines (the compute pass knows its row width); their calls
        // were checked above, so they go unreported.
        let (width, height) = window_size;
        let mut quiet = Tap::default();
        let mut sized = setup(&mut quiet, width, height, scale, true);
        // New resources need the texture uploaded on their first frame.
        let fresh = sized.is_some();
        let g = sized.as_mut().unwrap_or(&mut g);
        let started = std::time::Instant::now();
        let mut shown = 0u64;
        let mut presented = S_OK;
        let draws = 4 + usize::from(g.scenery) + extra_draws(g.scale);
        let submissions = 1 + extra_draws(g.scale) / DRAWS_A_STEP;
        let per_frame = format!(
            "{}x{}, scale {}: {draws} draws and {submissions} submissions a frame",
            g.width, g.height, g.scale
        );
        println!("demo: {per_frame}");
        // A line every 5 seconds: frames shown and time per frame since the
        // last (a run until the window closes never reaches the report).
        let (mut mark, mut marked) = (std::time::Instant::now(), 0u64);
        while started.elapsed().as_secs() < u64::from(window_seconds) {
            if mark.elapsed().as_secs() >= 5 {
                let frames = shown - marked;
                let ms = mark.elapsed().as_secs_f64() * 1000.0 / frames.max(1) as f64;
                println!(
                    "demo: {:.1} fps, {ms:.2} ms a frame ({per_frame})",
                    1000.0 / ms
                );
                (mark, marked) = (std::time::Instant::now(), shown);
            }
            let token = g.begin();
            g.frame(started.elapsed().as_secs_f32(), fresh && shown == 0, true);
            let hr = g.present(token);
            if hr != S_OK {
                presented = hr;
            }
            shown += 1;
        }
        // The last frame once more, read back (`VICTORY_CONFORMANCE_DUMP`).
        g.begin();
        g.frame(started.elapsed().as_secs_f32(), fresh && shown == 0, false);
        window_image = g.read();
        t.eq(
            presented,
            S_OK,
            &format!(
                "gpu: PresentX ({shown} frames, {:.2} ms a frame, at {per_frame})",
                started.elapsed().as_secs_f64() * 1000.0 / shown.max(1) as f64
            ),
        );
    }
    Run {
        image,
        window_image,
    }
}

/// The frame against the WARP reference: channels within 4/255, except
/// on a few edge pixels (rasterisation rules leave the GPUs free to
/// differ by a pixel along edges, and filtering by a little).
fn compare(t: &mut Tap, image: &[f32], reference: Option<&[f32]>) {
    let name = "gpu: the frame matches WARP's drawing of the same scene from HLSL";
    let Some(reference) = reference else {
        t.check(true, &format!("{name} # SKIP no reference"), String::new);
        return;
    };
    if reference.len() != image.len() {
        t.check(false, name, || {
            format!("{} values, the reference {}", image.len(), reference.len())
        });
        return;
    }
    let (mut off, mut worst, mut sum) = (0usize, 0f32, 0f64);
    let mut first = None;
    for (p, (a, b)) in image.chunks(4).zip(reference.chunks(4)).enumerate() {
        let d = (0..4).map(|c| (a[c] - b[c]).abs()).fold(0f32, f32::max);
        sum += f64::from(d);
        worst = worst.max(d);
        if d > 4.0 / 255.0 || d.is_nan() {
            off += 1;
            first.get_or_insert((p, a.to_vec(), b.to_vec()));
        }
    }
    let pixels = image.len() / 4;
    let mean = sum / pixels as f64 * 255.0;
    t.check(off * 1000 <= pixels && mean < 0.5, name, || {
        format!("{off} of {pixels} pixels differ by more than 4/255 (largest {:.1}/255, mean {mean:.3}/255); first {first:?}", worst * 255.0)
    });
}

/// Pixels exact by construction: the clear colour above the ground (the
/// blur of equal neighbours is the colour itself), the pyramid's glow
/// where it is nearest, and no glow where a cube in front hides it.
fn exact(t: &mut Tap, image: &[f32]) {
    if image.is_empty() {
        return;
    }
    let clear = scene::CLEAR;
    let glow = scene::GLOW;
    for (x, y) in [(1, 0), (WIDTH - 2, 0), (WIDTH / 2, 2)] {
        t.eq(
            pixel(image, x, y),
            clear,
            &format!("gpu: pixel ({x}, {y}) is the clear colour"),
        );
    }
    let (gx, gy) = scene::GLOW_PIXEL;
    t.eq(
        pixel(image, gx, gy),
        glow,
        &format!("gpu: pixel ({gx}, {gy}) is the pyramid's glow"),
    );
    let (hx, hy) = scene::HIDDEN_PIXEL;
    let hidden = pixel(image, hx, hy);
    t.check(
        hidden != glow && hidden != clear,
        &format!("gpu: pixel ({hx}, {hy}), the pyramid behind a cube, is the cube"),
        || format!("seen {hidden:?}"),
    );
}
