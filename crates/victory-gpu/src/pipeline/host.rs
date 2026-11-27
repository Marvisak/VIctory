//! Host pipelines for title pipelines (decision 0007, step 3): the
//! title's graphics state with the translated shaders, under a root
//! signature of our own that the translated shaders' bindings define
//! (`victory_shader::emit`): per stage one block of root constants for the
//! user data, root views for the title's root views, static samplers for
//! the immediate samplers, and directly indexed heaps.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_SPACE, ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR};

use super::{Shader, Translated};
use crate::host::host;

/// A stream's graphics state, owned (the title's stream lives only for the
/// creation call). Subobjects the stream leaves out keep D3D12's defaults.
pub(crate) struct GraphicsState {
    blend: D3D12_BLEND_DESC,
    sample_mask: u32,
    rasterizer: D3D12_RASTERIZER_DESC,
    depth: D3D12_DEPTH_STENCIL_DESC,
    depth_bounds: bool,
    /// Input elements, their names pointing into static/interned strings or `names`.
    inputs: Vec<D3D12_INPUT_ELEMENT_DESC>,
    /// Owned fallback storage when the bounded semantic-name interner is full.
    names: Vec<CString>,
    strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE,
    pub(crate) topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    pub(crate) targets: D3D12_RT_FORMAT_ARRAY,
    depth_format: DXGI_FORMAT,
    samples: DXGI_SAMPLE_DESC,
}

impl Default for GraphicsState {
    fn default() -> Self {
        let target = D3D12_RENDER_TARGET_BLEND_DESC {
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let keep = D3D12_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        };
        GraphicsState {
            blend: D3D12_BLEND_DESC {
                RenderTarget: [target; 8],
                ..Default::default()
            },
            sample_mask: u32::MAX,
            rasterizer: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_BACK,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            depth: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                StencilReadMask: 0xff,
                StencilWriteMask: 0xff,
                FrontFace: keep,
                BackFace: keep,
                ..Default::default()
            },
            depth_bounds: false,
            inputs: Vec::new(),
            names: Vec::new(),
            strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
            topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE_UNDEFINED,
            targets: D3D12_RT_FORMAT_ARRAY::default(),
            depth_format: DXGI_FORMAT_UNKNOWN,
            samples: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
        }
    }
}

/// Integer render target formats: D3D12 cannot blend into them.
fn integer_format(f: DXGI_FORMAT) -> bool {
    matches!(
        f,
        DXGI_FORMAT_R32G32B32A32_UINT
            | DXGI_FORMAT_R32G32B32A32_SINT
            | DXGI_FORMAT_R32G32B32_UINT
            | DXGI_FORMAT_R32G32B32_SINT
            | DXGI_FORMAT_R16G16B16A16_UINT
            | DXGI_FORMAT_R16G16B16A16_SINT
            | DXGI_FORMAT_R32G32_UINT
            | DXGI_FORMAT_R32G32_SINT
            | DXGI_FORMAT_R10G10B10A2_UINT
            | DXGI_FORMAT_R8G8B8A8_UINT
            | DXGI_FORMAT_R8G8B8A8_SINT
            | DXGI_FORMAT_R16G16_UINT
            | DXGI_FORMAT_R16G16_SINT
            | DXGI_FORMAT_R32_UINT
            | DXGI_FORMAT_R32_SINT
            | DXGI_FORMAT_R8G8_UINT
            | DXGI_FORMAT_R8G8_SINT
            | DXGI_FORMAT_R16_UINT
            | DXGI_FORMAT_R16_SINT
            | DXGI_FORMAT_R8_UINT
            | DXGI_FORMAT_R8_SINT
    )
}

/// The title's blend state as the host takes it: blending off on integer
/// render targets. The console ignores blending there, and titles leave it
/// on (the world sets it on an `R8_UINT` target), but D3D12 does not allow
/// it: vkd3d-proton refuses such a pipeline with `E_INVALIDARG` ("using
/// integer format is not supported"), and its draws are lost. With one
/// description for every target, each target gets its own copy so the
/// others keep theirs.
fn host_blend(blend: &D3D12_BLEND_DESC, targets: &D3D12_RT_FORMAT_ARRAY) -> D3D12_BLEND_DESC {
    let count = (targets.NumRenderTargets as usize).min(8);
    let per_target = blend.IndependentBlendEnable.as_bool();
    let blends = |i: usize| {
        blend.RenderTarget[if per_target { i } else { 0 }]
            .BlendEnable
            .as_bool()
    };
    if !(0..count).any(|i| integer_format(targets.RTFormats[i]) && blends(i)) {
        return *blend;
    }
    let mut host = *blend;
    if !per_target {
        host.IndependentBlendEnable = true.into();
        host.RenderTarget = [blend.RenderTarget[0]; 8];
    }
    for i in 0..count {
        if integer_format(targets.RTFormats[i]) {
            host.RenderTarget[i].BlendEnable = false.into();
        }
    }
    host
}

/// Standard HLSL semantic names matched without locking or allocation.
const COMMON_SEMANTICS: [&CStr; 16] = [
    c"POSITION",
    c"NORMAL",
    c"TANGENT",
    c"BINORMAL",
    c"TEXCOORD",
    c"COLOR",
    c"BLENDINDICES",
    c"BLENDWEIGHT",
    c"POSITIONT",
    c"PSIZE",
    c"FOG",
    c"TESSFACTOR",
    c"SV_Position",
    c"SV_POSITION",
    c"SV_VertexID",
    c"SV_InstanceID",
];

/// Cap on process-wide interned custom semantic names so adversarial streams
/// cannot grow global memory without bound.
const MAX_INTERNED_SEMANTICS: usize = 256;

/// Returns a `'static` pointer for `name` when it is a common HLSL semantic
/// or fits in the bounded interner; `None` once the interner is full (the
/// caller then keeps an owned `CString` on the `GraphicsState`).
fn intern_semantic(name: &CStr) -> Option<&'static CStr> {
    let bytes = name.to_bytes();
    for &common in &COMMON_SEMANTICS {
        if common.to_bytes() == bytes {
            return Some(common);
        }
    }
    static INTERNED: Mutex<Option<HashSet<&'static CStr>>> = Mutex::new(None);
    let mut set = INTERNED.lock().unwrap_or_else(|e| e.into_inner());
    let set = set.get_or_insert_with(HashSet::new);
    if let Some(&existing) = set.get(name) {
        return Some(existing);
    }
    if set.len() >= MAX_INTERNED_SEMANTICS {
        return None;
    }
    let leaked: &'static CStr = Box::leak(name.to_owned().into_boxed_c_str());
    set.insert(leaked);
    Some(leaked)
}

/// Sink for streaming [`GraphicsState::write_describe`] into either a `String`
/// or directly into a `blake3::Hasher` without intermediate allocations.
trait DescSink {
    fn push_str(&mut self, s: &str);

    fn push_u32(&mut self, mut v: u32) {
        let mut buf = [0u8; 10];
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        // SAFETY: `buf[i..]` holds ASCII digits.
        self.push_str(unsafe { std::str::from_utf8_unchecked(&buf[i..]) });
    }

    fn push_i32(&mut self, v: i32) {
        if v < 0 {
            self.push_str("-");
        }
        self.push_u32(v.unsigned_abs());
    }

    fn push_f32(&mut self, v: f32) {
        if v.to_bits() == 0 {
            self.push_str("0.0");
            return;
        }
        let mut tmp = String::new();
        let _ = write!(tmp, "{v:?}");
        self.push_str(&tmp);
    }

    fn push_cstr_quoted(&mut self, name: &CStr) {
        let bytes = name.to_bytes();
        if bytes
            .iter()
            .all(|&b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\' && b != b'\'')
        {
            self.push_str("\\\"");
            // SAFETY: checked printable ASCII above.
            self.push_str(unsafe { std::str::from_utf8_unchecked(bytes) });
            self.push_str("\\\"");
            return;
        }
        // Fallback matching `format!("{:?}", format!("{name:?}"))` without outer quotes.
        let inner = format!("{name:?}");
        let escaped = format!("{inner:?}");
        self.push_str(&escaped[1..escaped.len() - 1]);
    }
}

impl DescSink for String {
    fn push_str(&mut self, s: &str) {
        String::push_str(self, s);
    }
}

/// Stack-buffered sink that feeds `blake3::Hasher` in 512-byte chunks.
struct HashSink<'a> {
    hasher: &'a mut blake3::Hasher,
    buf: [u8; 512],
    len: usize,
}

impl<'a> HashSink<'a> {
    fn new(hasher: &'a mut blake3::Hasher) -> Self {
        Self {
            hasher,
            buf: [0; 512],
            len: 0,
        }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            self.hasher.update(&self.buf[..self.len]);
            self.len = 0;
        }
    }
}

impl DescSink for HashSink<'_> {
    fn push_str(&mut self, s: &str) {
        let mut bytes = s.as_bytes();
        while !bytes.is_empty() {
            let room = self.buf.len() - self.len;
            if bytes.len() <= room {
                self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
                self.len += bytes.len();
                return;
            }
            if self.len == 0 {
                self.hasher.update(bytes);
                return;
            }
            self.buf[self.len..].copy_from_slice(&bytes[..room]);
            self.hasher.update(&self.buf);
            self.len = 0;
            bytes = &bytes[room..];
        }
    }
}

fn write_stencil_op(out: &mut impl DescSink, op: &D3D12_DEPTH_STENCILOP_DESC) {
    out.push_str("D3D12_DEPTH_STENCILOP_DESC { StencilFailOp: D3D12_STENCIL_OP(");
    out.push_i32(op.StencilFailOp.0);
    out.push_str("), StencilDepthFailOp: D3D12_STENCIL_OP(");
    out.push_i32(op.StencilDepthFailOp.0);
    out.push_str("), StencilPassOp: D3D12_STENCIL_OP(");
    out.push_i32(op.StencilPassOp.0);
    out.push_str("), StencilFunc: D3D12_COMPARISON_FUNC(");
    out.push_i32(op.StencilFunc.0);
    out.push_str(") }");
}

impl GraphicsState {
    /// Takes in one subobject of a stream: its type, then its data at +4
    /// (+8 for those holding pointers).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind`.
    pub(crate) unsafe fn read(&mut self, kind: u32, at: *const u8) {
        // SAFETY: the caller passes a whole subobject.
        unsafe {
            let data = at.add(4);
            match kind {
                8 => self.blend = data.cast::<D3D12_BLEND_DESC>().read_unaligned(),
                9 => self.sample_mask = data.cast::<u32>().read_unaligned(),
                10 => self.rasterizer = data.cast::<D3D12_RASTERIZER_DESC>().read_unaligned(),
                11 => {
                    self.depth = data.cast::<D3D12_DEPTH_STENCIL_DESC>().read_unaligned();
                    self.depth_bounds = false;
                }
                21 => {
                    // As before, read this console subobject with the PC layout.
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC1>().read_unaligned();
                    self.depth_bounds = d.DepthBoundsTestEnable.as_bool();
                    self.depth = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.StencilReadMask,
                        StencilWriteMask: d.StencilWriteMask,
                        FrontFace: d.FrontFace,
                        BackFace: d.BackFace,
                    };
                }
                12 => {
                    let layout = at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned();
                    if layout.pInputElementDescs.is_null() {
                        return;
                    }
                    let elements = std::slice::from_raw_parts(
                        layout.pInputElementDescs,
                        layout.NumElements as usize,
                    );
                    let mut names = Vec::new();
                    let mut inputs = Vec::with_capacity(elements.len());
                    for e in elements {
                        let cstr = CStr::from_ptr(e.SemanticName.0.cast());
                        let ptr = match intern_semantic(cstr) {
                            Some(s) => s.as_ptr(),
                            None => {
                                names.push(cstr.to_owned());
                                names.last().expect("just pushed").as_ptr()
                            }
                        };
                        inputs.push(D3D12_INPUT_ELEMENT_DESC {
                            SemanticName: PCSTR(ptr.cast()),
                            Format: DXGI_FORMAT(
                                crate::resource::pc_format(e.Format.0 as u32) as i32
                            ),
                            ..*e
                        });
                    }
                    self.names = names;
                    self.inputs = inputs;
                }
                13 => {
                    self.strip_cut =
                        D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(data.cast::<i32>().read_unaligned())
                }
                14 => {
                    self.topology =
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE(data.cast::<i32>().read_unaligned())
                }
                15 => {
                    self.targets = data.cast::<D3D12_RT_FORMAT_ARRAY>().read_unaligned();
                    for f in &mut self.targets.RTFormats {
                        f.0 = crate::resource::pc_format(f.0 as u32) as i32;
                    }
                }
                16 => {
                    let f = data.cast::<u32>().read_unaligned();
                    self.depth_format = DXGI_FORMAT(crate::resource::pc_format(f) as i32);
                }
                17 => self.samples = data.cast::<DXGI_SAMPLE_DESC>().read_unaligned(),
                _ => {}
            }
        }
    }

    /// Streams the exact text of [`GraphicsState::describe`] into `out`
    /// without intermediate heap allocations.
    fn write_describe(&self, out: &mut impl DescSink) {
        out.push_str("D3D12_BLEND_DESC { AlphaToCoverageEnable: BOOL(");
        out.push_i32(self.blend.AlphaToCoverageEnable.0);
        out.push_str("), IndependentBlendEnable: BOOL(");
        out.push_i32(self.blend.IndependentBlendEnable.0);
        out.push_str("), RenderTarget: [");
        for (i, rt) in self.blend.RenderTarget.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str("D3D12_RENDER_TARGET_BLEND_DESC { BlendEnable: BOOL(");
            out.push_i32(rt.BlendEnable.0);
            out.push_str("), LogicOpEnable: BOOL(");
            out.push_i32(rt.LogicOpEnable.0);
            out.push_str("), SrcBlend: D3D12_BLEND(");
            out.push_i32(rt.SrcBlend.0);
            out.push_str("), DestBlend: D3D12_BLEND(");
            out.push_i32(rt.DestBlend.0);
            out.push_str("), BlendOp: D3D12_BLEND_OP(");
            out.push_i32(rt.BlendOp.0);
            out.push_str("), SrcBlendAlpha: D3D12_BLEND(");
            out.push_i32(rt.SrcBlendAlpha.0);
            out.push_str("), DestBlendAlpha: D3D12_BLEND(");
            out.push_i32(rt.DestBlendAlpha.0);
            out.push_str("), BlendOpAlpha: D3D12_BLEND_OP(");
            out.push_i32(rt.BlendOpAlpha.0);
            out.push_str("), LogicOp: D3D12_LOGIC_OP(");
            out.push_i32(rt.LogicOp.0);
            out.push_str("), RenderTargetWriteMask: ");
            out.push_u32(u32::from(rt.RenderTargetWriteMask));
            out.push_str(" }");
        }
        out.push_str("] } ");
        out.push_u32(self.sample_mask);

        out.push_str(" D3D12_RASTERIZER_DESC { FillMode: D3D12_FILL_MODE(");
        out.push_i32(self.rasterizer.FillMode.0);
        out.push_str("), CullMode: D3D12_CULL_MODE(");
        out.push_i32(self.rasterizer.CullMode.0);
        out.push_str("), FrontCounterClockwise: BOOL(");
        out.push_i32(self.rasterizer.FrontCounterClockwise.0);
        out.push_str("), DepthBias: ");
        out.push_i32(self.rasterizer.DepthBias);
        out.push_str(", DepthBiasClamp: ");
        out.push_f32(self.rasterizer.DepthBiasClamp);
        out.push_str(", SlopeScaledDepthBias: ");
        out.push_f32(self.rasterizer.SlopeScaledDepthBias);
        out.push_str(", DepthClipEnable: BOOL(");
        out.push_i32(self.rasterizer.DepthClipEnable.0);
        out.push_str("), MultisampleEnable: BOOL(");
        out.push_i32(self.rasterizer.MultisampleEnable.0);
        out.push_str("), AntialiasedLineEnable: BOOL(");
        out.push_i32(self.rasterizer.AntialiasedLineEnable.0);
        out.push_str("), ForcedSampleCount: ");
        out.push_u32(self.rasterizer.ForcedSampleCount);
        out.push_str(", ConservativeRaster: D3D12_CONSERVATIVE_RASTERIZATION_MODE(");
        out.push_i32(self.rasterizer.ConservativeRaster.0);
        out.push_str(") } ");

        out.push_str("D3D12_DEPTH_STENCIL_DESC { DepthEnable: BOOL(");
        out.push_i32(self.depth.DepthEnable.0);
        out.push_str("), DepthWriteMask: D3D12_DEPTH_WRITE_MASK(");
        out.push_i32(self.depth.DepthWriteMask.0);
        out.push_str("), DepthFunc: D3D12_COMPARISON_FUNC(");
        out.push_i32(self.depth.DepthFunc.0);
        out.push_str("), StencilEnable: BOOL(");
        out.push_i32(self.depth.StencilEnable.0);
        out.push_str("), StencilReadMask: ");
        out.push_u32(u32::from(self.depth.StencilReadMask));
        out.push_str(", StencilWriteMask: ");
        out.push_u32(u32::from(self.depth.StencilWriteMask));
        out.push_str(", FrontFace: ");
        write_stencil_op(out, &self.depth.FrontFace);
        out.push_str(", BackFace: ");
        write_stencil_op(out, &self.depth.BackFace);
        out.push_str(" } [");

        for (i, e) in self.inputs.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str("\"");
            // SAFETY: every input element's `SemanticName` points to an
            // interned `&'static CStr` or an entry kept in `self.names`.
            let name = unsafe { CStr::from_ptr(e.SemanticName.0.cast()) };
            out.push_cstr_quoted(name);
            out.push_str(" ");
            out.push_u32(e.SemanticIndex);
            out.push_str(" ");
            out.push_i32(e.Format.0);
            out.push_str(" ");
            out.push_u32(e.InputSlot);
            out.push_str(" ");
            out.push_u32(e.AlignedByteOffset);
            out.push_str(" ");
            out.push_i32(e.InputSlotClass.0);
            out.push_str(" ");
            out.push_u32(e.InstanceDataStepRate);
            out.push_str("\"");
        }
        out.push_str("] ");
        out.push_i32(self.strip_cut.0);
        out.push_str(" ");
        out.push_i32(self.topology.0);
        out.push_str(" D3D12_RT_FORMAT_ARRAY { RTFormats: [");
        for (i, f) in self.targets.RTFormats.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str("DXGI_FORMAT(");
            out.push_i32(f.0);
            out.push_str(")");
        }
        out.push_str("], NumRenderTargets: ");
        out.push_u32(self.targets.NumRenderTargets);
        out.push_str(" } ");
        out.push_i32(self.depth_format.0);
        out.push_str(" DXGI_SAMPLE_DESC { Count: ");
        out.push_u32(self.samples.Count);
        out.push_str(", Quality: ");
        out.push_u32(self.samples.Quality);
        out.push_str(" }");
        if self.depth_bounds {
            out.push_str(" depth bounds");
        }
    }

    /// Feeds the exact bytes of [`GraphicsState::describe`] into `hasher`
    /// without allocating.
    pub(crate) fn hash_describe(&self, hasher: &mut blake3::Hasher) {
        let mut sink = HashSink::new(hasher);
        self.write_describe(&mut sink);
        sink.flush();
    }

    /// Everything the state holds, as text: part of a pipeline's name in
    /// the library and of the key a title pipeline is started early by.
    pub(crate) fn describe(&self) -> String {
        let mut out = String::with_capacity(4096);
        self.write_describe(&mut out);
        out
    }

    fn depth_with_bounds(&self, host: &crate::host::Host) -> Option<D3D12_DEPTH_STENCIL_DESC1> {
        if !self.depth_bounds {
            return None;
        }
        if !host.depth_bounds_supported {
            crate::host::warn_depth_bounds();
            return None;
        }
        let d = self.depth;
        Some(D3D12_DEPTH_STENCIL_DESC1 {
            DepthEnable: d.DepthEnable,
            DepthWriteMask: d.DepthWriteMask,
            DepthFunc: d.DepthFunc,
            StencilEnable: d.StencilEnable,
            StencilReadMask: d.StencilReadMask,
            StencilWriteMask: d.StencilWriteMask,
            FrontFace: d.FrontFace,
            BackFace: d.BackFace,
            DepthBoundsTestEnable: true.into(),
        })
    }

    /// The vertex buffer slots the input layout reads.
    fn slots(&self) -> u32 {
        self.inputs
            .iter()
            .fold(0, |m, e| m | 1 << e.InputSlot.min(31))
    }

    /// The input elements with their byte offsets worked out.
    fn elements(&self) -> Vec<Input> {
        let mut next = [0u32; 32];
        self.inputs
            .iter()
            .map(|e| {
                let slot = e.InputSlot.min(31) as usize;
                let format = e.Format.0 as u32;
                let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                // An appended element starts at the next multiple of
                // min(4, its size) after the previous one, where D3D12 (and
                // vkd3d) place it; the host pipeline gets the title's
                // descriptions, so the offsets read here must match.
                let offset = if e.AlignedByteOffset == D3D12_APPEND_ALIGNED_ELEMENT {
                    next[slot]
                        .checked_next_multiple_of(bytes.clamp(1, 4))
                        .unwrap_or(u32::MAX)
                } else {
                    e.AlignedByteOffset
                };
                next[slot] = offset.saturating_add(bytes);
                Input {
                    slot: slot as u32,
                    offset,
                    format,
                    per_instance: e.InputSlotClass == D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                }
            })
            .collect()
    }
}

/// One input element: vertex buffer slot, byte offset, format.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input {
    pub(crate) slot: u32,
    pub(crate) offset: u32,
    pub(crate) format: u32,
    pub(crate) per_instance: bool,
}

/// The console's own primitive topology types.
const RECTANGLES: i32 = 19;
const QUADS: i32 = 32;

/// One stage of a host pipeline and where its bindings go.
pub(crate) struct Stage {
    pub(crate) shader: Arc<Shader>,
    /// The root parameter of its user data, and its size in dwords.
    pub(crate) user_data: Option<(u32, u32)>,
    /// Root parameters of its root views: (title root parameter, ours).
    pub(crate) root_cbvs: Vec<(u8, u32)>,
    pub(crate) root_srvs: Vec<(u8, u32)>,
    pub(crate) root_uavs: Vec<(u8, u32)>,
    /// Our root parameters of the title-memory page table and of the
    /// written-pages bitmap (`crate::memory`), if it reads or writes there.
    pub(crate) memory: Option<u32>,
    pub(crate) memory_dirty: Option<u32>,
    /// Our root parameter of the heap's view addresses (`crate::memory`).
    pub(crate) view_addresses: Option<u32>,
}

impl Stage {
    pub(crate) fn translated(&self) -> &Translated {
        self.shader
            .translated()
            .expect("host pipelines are made from translated shaders")
    }
}

pub(crate) struct HostPipeline {
    /// A compute pipeline (else graphics).
    pub(crate) compute: bool,
    /// A mesh pipeline (graphics without vertex input).
    pub(crate) mesh: bool,
    pub(crate) pso: ID3D12PipelineState,
    pub(crate) root: ID3D12RootSignature,
    /// Vertex, then pixel stage, or the compute stage.
    pub(crate) stages: Vec<Stage>,
    /// A graphics pipeline of the console's rectangle lists whose geometry
    /// shader completes each rectangle (`victory_shader::emit::rectangle_gs`):
    /// it draws them as triangles, one per rectangle.
    pub(crate) rectangles: bool,
    /// The vertex buffer slots it reads (bit per slot).
    pub(crate) slots: u32,
    pub(crate) inputs: Vec<Input>,
    /// Its shaders, for reports of slow submissions.
    pub(crate) name: String,
    /// Our root parameters of the view-mark table and the buffer views'
    /// written-pages bitmap, if a stage marks its writes through views
    /// (`victory_shader::emit`, "Write marks").
    pub(crate) marks: Option<(u32, u32)>,
}

/// Logs a pipeline that gets no host pipeline, once per reason.
fn refuse(reason: &'static str, shaders: &[Arc<Shader>]) -> Option<HostPipeline> {
    static SEEN: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&reason) {
        seen.push(reason);
        tracing::warn!(?shaders, "no host pipeline: {reason} (logged once)");
    }
    None
}

impl HostPipeline {
    /// A host pipeline for vertex and pixel shaders with the graphics
    /// state `g`, or for one compute shader.
    pub(crate) fn new(shaders: &[Arc<Shader>], g: Option<&GraphicsState>) -> Option<HostPipeline> {
        let find = |stage| shaders.iter().find(|s| s.stage == stage);
        let compute = match shaders {
            [cs] if cs.stage == "compute" => Some(cs),
            _ => None,
        };
        if compute.is_none()
            && shaders
                .iter()
                .any(|s| !matches!(s.stage, "vertex" | "pixel" | "mesh"))
        {
            return refuse(
                "only vertex, mesh, pixel and compute stages are translated",
                shaders,
            );
        }
        let mesh = find("mesh");
        let stages = if let Some(cs) = compute {
            vec![(cs, D3D12_SHADER_VISIBILITY_ALL)]
        } else {
            let first = match (mesh, find("vertex")) {
                (Some(ms), _) => (ms, D3D12_SHADER_VISIBILITY_MESH),
                (None, Some(vs)) => (vs, D3D12_SHADER_VISIBILITY_VERTEX),
                (None, None) => return refuse("no vertex or mesh shader", shaders),
            };
            let mut stages = vec![first];
            stages.extend(find("pixel").map(|ps| (ps, D3D12_SHADER_VISIBILITY_PIXEL)));
            stages
        };
        if mesh.is_some() && !host()?.mesh_shaders() {
            return refuse("the host GPU has no mesh shaders (or no shader model 6.6)", shaders);
        }
        for (s, _) in &stages {
            s.translated()?;
        }
        let host = host()?;
        // `SFI0`: 64-bit atomics on typed resources and on heap resources.
        let atomics64 = stages.iter().any(|(s, _)| {
            let Some(t) = s.translated() else {
                return false;
            };
            victory_shader::Container::parse(&t.host.dxil)
                .ok()
                .and_then(|c| {
                    c.part(victory_shader::FourCC::SFI0)?
                        .data
                        .get(..4)?
                        .try_into()
                        .ok()
                })
                .is_some_and(|b: [u8; 4]| u32::from_le_bytes(b) & 0x1040_0000 != 0)
        });
        if atomics64 && !host.heap_atomics64() {
            return refuse(
                "the host GPU has no 64-bit atomics on typed heap resources",
                shaders,
            );
        }

        let aniso_point_mip_supported = crate::sampler::aniso_point_mip(&host.device);

        // The root signature the translated shaders' bindings define.
        let mut params: Vec<D3D12_ROOT_PARAMETER1> = Vec::new();
        let mut samplers = Vec::new();
        let mut bound = Vec::new();
        for &(shader, visibility) in &stages {
            let t = &shader.translated()?.host;
            let mut stage = Stage {
                shader: shader.clone(),
                user_data: None,
                root_cbvs: Vec::new(),
                root_srvs: Vec::new(),
                root_uavs: Vec::new(),
                memory: None,
                memory_dirty: None,
                view_addresses: None,
            };
            let dwords = t.user_data_dwords();
            if dwords > 0 {
                stage.user_data = Some((params.len() as u32, dwords));
                params.push(D3D12_ROOT_PARAMETER1 {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                    Anonymous: D3D12_ROOT_PARAMETER1_0 {
                        Constants: D3D12_ROOT_CONSTANTS {
                            ShaderRegister: 0,
                            RegisterSpace: USER_DATA_SPACE,
                            Num32BitValues: dwords,
                        },
                    },
                    ShaderVisibility: visibility,
                });
            }
            for (slots, kind, out) in [
                (
                    &t.root_cbvs,
                    D3D12_ROOT_PARAMETER_TYPE_CBV,
                    &mut stage.root_cbvs,
                ),
                (
                    &t.root_srvs,
                    D3D12_ROOT_PARAMETER_TYPE_SRV,
                    &mut stage.root_srvs,
                ),
                (
                    &t.root_uavs,
                    D3D12_ROOT_PARAMETER_TYPE_UAV,
                    &mut stage.root_uavs,
                ),
            ] {
                for &slot in slots {
                    out.push((slot, params.len() as u32));
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: kind,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                                ShaderRegister: u32::from(slot),
                                RegisterSpace: ROOT_CBV_SPACE,
                                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                            },
                        },
                        ShaderVisibility: visibility,
                    });
                }
            }
            for (used, kind, register, out) in [
                (t.memory, D3D12_ROOT_PARAMETER_TYPE_SRV, 0, &mut stage.memory),
                (t.memory_writes, D3D12_ROOT_PARAMETER_TYPE_UAV, 0, &mut stage.memory_dirty),
                (t.view_addresses, D3D12_ROOT_PARAMETER_TYPE_SRV, 1, &mut stage.view_addresses),
            ] {
                if used {
                    *out = Some(params.len() as u32);
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: kind,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                                ShaderRegister: register,
                                RegisterSpace: MEMORY_SPACE,
                                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                            },
                        },
                        ShaderVisibility: visibility,
                    });
                }
            }
            for (n, s) in t.static_samplers.iter().enumerate() {
                samplers.push(D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER(s.filter_for_host(aniso_point_mip_supported) as i32),
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.address[0] as i32),
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.address[1] as i32),
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.address[2] as i32),
                    MipLODBias: s.mip_lod_bias,
                    MaxAnisotropy: s.max_anisotropy,
                    ComparisonFunc: D3D12_COMPARISON_FUNC(s.comparison as i32),
                    BorderColor: D3D12_STATIC_BORDER_COLOR(s.border as i32),
                    MinLOD: s.min_lod,
                    MaxLOD: s.max_lod,
                    ShaderRegister: n as u32,
                    RegisterSpace: SAMPLER_SPACE,
                    ShaderVisibility: visibility,
                });
            }
            bound.push(stage);
        }
        // One pair for all stages: they share the heap and the bitmap.
        let marks = stages
            .iter()
            .any(|(s, _)| s.translated().is_some_and(|t| t.host.view_marks))
            .then(|| {
                let first = params.len() as u32;
                for (kind, register) in [
                    (D3D12_ROOT_PARAMETER_TYPE_SRV, victory_shader::emit::VIEW_MARKS),
                    (D3D12_ROOT_PARAMETER_TYPE_UAV, victory_shader::emit::VIEW_WRITES),
                ] {
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: kind,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                                ShaderRegister: register,
                                RegisterSpace: MEMORY_SPACE,
                                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                            },
                        },
                        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                    });
                }
                (first, first + 1)
            });
        let (root, root_blob) = match root_signature(&host.device, &params, &samplers, compute.is_some()) {
            Ok(root) => root,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host root signature refused");
                return None;
            }
        };
        let code = |i: usize| {
            stages.get(i).and_then(|(s, _)| s.translated()).map_or(
                D3D12_SHADER_BYTECODE::default(),
                |t| D3D12_SHADER_BYTECODE {
                    pShaderBytecode: t.host.dxil.as_ptr().cast(),
                    BytecodeLength: t.host.dxil.len(),
                },
            )
        };
        if compute.is_some() {
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                // SAFETY: a borrowed pointer the description never releases.
                pRootSignature: unsafe { std::mem::transmute_copy(&root) },
                CS: code(0),
                ..Default::default()
            };
            let name = super::library::name("compute", &root_blob, &[&code_bytes(&code(0))], "");
            let made = super::library::pipeline(
                &name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateComputePipelineState(&desc) },
            );
            let pso = match made {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(?shaders, "host pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: true,
                mesh: false,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                rectangles: false,
                slots: 0,
                inputs: Vec::new(),
                marks,
            });
        }
        let Some(g) = g else {
            return refuse("graphics state not kept (plain description)", shaders);
        };
        let state = g.describe();
        let depth_with_bounds = g.depth_with_bounds(host);
        if mesh.is_some() {
            let name = super::library::name(
                "mesh",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                &state,
            );
            let ms = Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, code(0));
            let made = match depth_with_bounds {
                Some(depth) => stream_pipeline(
                    &host.device, &root, ms, code(1), g,
                    Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1, depth),
                    &name,
                ),
                None => stream_pipeline(
                    &host.device, &root, ms, code(1), g,
                    Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, g.depth),
                    &name,
                ),
            };
            let pso = match made {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host mesh pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(?shaders, "host mesh pipeline");
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: false,
                mesh: true,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                rectangles: false,
                slots: 0,
                inputs: Vec::new(),
                marks,
            });
        }

        // Rectangle lists: a geometry shader made for the vertex shader adds
        // each rectangle's fourth corner (else rectangles are expanded on
        // the CPU from vertex inputs, see `draw`).
        let gs = match (g.topology.0, stages.first().and_then(|(s, _)| s.translated())) {
            (RECTANGLES, Some(vs)) => match victory_shader::emit::rectangle_gs(&vs.host.dxil) {
                Ok(gs) => gs,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "no rectangle geometry shader");
                    Vec::new()
                }
            },
            _ => Vec::new(),
        };
        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(0),
            PS: code(1),
            GS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: if gs.is_empty() { std::ptr::null() } else { gs.as_ptr().cast() },
                BytecodeLength: gs.len(),
            },
            BlendState: host_blend(&g.blend, &g.targets),
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: g.inputs.as_ptr(),
                NumElements: g.inputs.len() as u32,
            },
            IBStripCutValue: g.strip_cut,
            PrimitiveTopologyType: host_topology(g),
            NumRenderTargets: g.targets.NumRenderTargets,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        let name = super::library::name(
            "graphics",
            &root_blob,
            &[&code_bytes(&code(0)), &code_bytes(&code(1)), &gs],
            &state,
        );
        let started = std::time::Instant::now();
        let made = if let Some(depth) = depth_with_bounds {
            let vertex = VertexStream {
                vs: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS, desc.VS),
                gs: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS, desc.GS),
                input: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT, desc.InputLayout),
                strip_cut: Sub(
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE,
                    desc.IBStripCutValue,
                ),
            };
            stream_pipeline(
                &host.device, &root, vertex, desc.PS, g,
                Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1, depth),
                &name,
            )
        } else {
            super::library::pipeline(
                &name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadGraphicsPipeline(name, &desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateGraphicsPipelineState(&desc) },
            )
        };
        let pso = match made {
            Ok(pso) => pso,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                host.log_messages();
                return None;
            }
        };
        tracing::info!(?shaders, ms = started.elapsed().as_millis(), rectangles = !gs.is_empty(), "host pipeline");
        crate::host::name(&pso, || format!("{shaders:?}"));
        tracing::debug!(
            ?shaders,
            depth = ?g.depth,
            dsv = g.depth_format.0,
            targets = ?&g.targets.RTFormats[..g.targets.NumRenderTargets.min(8) as usize],
            blend = ?g.blend.RenderTarget[0],
            cull = g.rasterizer.CullMode.0,
            "host graphics state"
        );
        Some(HostPipeline {
            compute: false,
            mesh: false,
            pso,
            root,
            stages: bound,
            name: format!("{shaders:?}"),
            rectangles: !gs.is_empty(),
            slots: g.slots(),
            inputs: g.elements(),
            marks,
        })
    }
}

/// The host's topology type: the console's rectangle and quad types draw
/// as triangles on the host (see `draw`).
fn host_topology(g: &GraphicsState) -> D3D12_PRIMITIVE_TOPOLOGY_TYPE {
    match g.topology.0 {
        RECTANGLES | QUADS => D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        _ => g.topology,
    }
}

/// A stream subobject: its type, then its description, pointer-aligned.
#[repr(C, align(8))]
struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);

/// What a vertex pipeline's stream has in place of the mesh shader (the
/// geometry shader is the rectangle lists' one, or empty).
#[repr(C)]
struct VertexStream {
    vs: Sub<D3D12_SHADER_BYTECODE>,
    gs: Sub<D3D12_SHADER_BYTECODE>,
    input: Sub<D3D12_INPUT_LAYOUT_DESC>,
    strip_cut: Sub<D3D12_INDEX_BUFFER_STRIP_CUT_VALUE>,
}

/// The stream form used by mesh pipelines and vertex pipelines with depth bounds.
fn stream_pipeline<S, D>(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    stages: S,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    depth: Sub<D>,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    #[repr(C)]
    struct Stream<S, D> {
        root: Sub<*mut std::ffi::c_void>,
        stages: S,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
        topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
    }
    let mut stream = Stream {
        root: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE, root.as_raw()),
        stages,
        ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps),
        blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, host_blend(&g.blend, &g.targets)),
        mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, g.sample_mask),
        raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer),
        depth,
        targets: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS, g.targets),
        dsv: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT, g.depth_format),
        samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples),
        // A vertex pipeline needs its type. A mesh pipeline's, which the
        // host derives from the mesh shader anyway, is given too: a
        // pipeline library compares descriptions with it, and loads none
        // made from a stream without it.
        topology: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY, host_topology(g)),
    };
    let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: size_of_val(&stream),
        pPipelineStateSubobjectStream: (&raw mut stream).cast(),
    };
    let device = device.cast::<ID3D12Device2>()?;
    super::library::pipeline(
        name,
        // SAFETY: the stream holds whole subobjects pointing at live data.
        |library, name| unsafe { library.LoadPipeline(name, &desc) },
        // SAFETY: as above.
        || unsafe { device.CreatePipelineState(&desc) },
    )
}

/// The bytes of shader code (none for an absent stage).
fn code_bytes(code: &D3D12_SHADER_BYTECODE) -> Vec<u8> {
    if code.pShaderBytecode.is_null() {
        return Vec::new();
    }
    // SAFETY: a translation's own bytes, alive while its shader is.
    unsafe { std::slice::from_raw_parts(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) }.to_vec()
}

fn root_signature(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
) -> windows::core::Result<(ID3D12RootSignature, Vec<u8>)> {
    let input = if compute {
        D3D12_ROOT_SIGNATURE_FLAG_NONE
    } else {
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: input
                    | D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                    | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
            },
        },
    };
    // SAFETY: the description points at live arrays; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut error: Option<ID3DBlob> = None;
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) {
            if let Some(error) = error {
                let text = std::slice::from_raw_parts(
                    error.GetBufferPointer().cast::<u8>(),
                    error.GetBufferSize(),
                );
                tracing::warn!("root signature: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        Ok((device.CreateRootSignature(0, bytes)?, bytes.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(formats: &[DXGI_FORMAT]) -> D3D12_RT_FORMAT_ARRAY {
        let mut t = D3D12_RT_FORMAT_ARRAY {
            NumRenderTargets: formats.len() as u32,
            ..Default::default()
        };
        t.RTFormats[..formats.len()].copy_from_slice(formats);
        t
    }

    fn blending(independent: bool) -> D3D12_BLEND_DESC {
        let mut b = GraphicsState::default().blend;
        b.IndependentBlendEnable = independent.into();
        for t in &mut b.RenderTarget {
            t.BlendEnable = true.into();
            t.SrcBlend = D3D12_BLEND_SRC_ALPHA;
            t.DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
        }
        b
    }

    #[test]
    fn integer_targets_do_not_blend() {
        let t = targets(&[
            DXGI_FORMAT_R8G8B8A8_UNORM,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            DXGI_FORMAT_R8_UINT,
        ]);
        for independent in [false, true] {
            let host = host_blend(&blending(independent), &t);
            assert!(host.IndependentBlendEnable.as_bool());
            assert!(host.RenderTarget[0].BlendEnable.as_bool());
            assert!(host.RenderTarget[1].BlendEnable.as_bool());
            assert!(!host.RenderTarget[2].BlendEnable.as_bool());
            // The rest of the target's state is the title's.
            assert_eq!(host.RenderTarget[2].SrcBlend, D3D12_BLEND_SRC_ALPHA);
            assert_eq!(host.RenderTarget[0].DestBlend, D3D12_BLEND_INV_SRC_ALPHA);
        }
    }

    #[test]
    fn blend_states_without_integer_blending_are_kept() {
        // No integer target.
        let b = blending(false);
        let t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R11G11B10_FLOAT]);
        assert_eq!(host_blend(&b, &t), b);
        // An integer target that does not blend.
        let mut b = blending(true);
        b.RenderTarget[1].BlendEnable = false.into();
        let t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R32_UINT]);
        assert_eq!(host_blend(&b, &t), b);
        // Integer formats past the targets in use.
        let mut t = targets(&[DXGI_FORMAT_R8G8B8A8_UNORM]);
        t.RTFormats[1] = DXGI_FORMAT_R8_UINT;
        assert_eq!(host_blend(&blending(false), &t), blending(false));
    }
    fn element(format: DXGI_FORMAT, offset: u32) -> D3D12_INPUT_ELEMENT_DESC {
        D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(c"TEXCOORD".as_ptr().cast()),
            SemanticIndex: 0,
            Format: format,
            InputSlot: 0,
            AlignedByteOffset: offset,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        }
    }

    fn offsets(inputs: &[D3D12_INPUT_ELEMENT_DESC]) -> Vec<u32> {
        let state = GraphicsState {
            inputs: inputs.to_vec(),
            ..GraphicsState::default()
        };
        state.elements().iter().map(|e| e.offset).collect()
    }

    /// An appended element after one of 1 or 2 bytes started right after
    /// it; D3D12 aligns it to min(4, its size), so the rectangle expansion
    /// read its vertices from the wrong bytes.
    #[test]
    fn appended_elements_are_aligned() {
        const APPEND: u32 = D3D12_APPEND_ALIGNED_ELEMENT;
        // RG8 (2 bytes) then RG32 float: 4, not 2.
        let a = [
            element(DXGI_FORMAT_R8G8_UNORM, 0),
            element(DXGI_FORMAT_R32G32_FLOAT, APPEND),
        ];
        assert_eq!(offsets(&a), [0, 4]);
        // R8 then R16 float: 2, not 1; then R8 packs right after it.
        let b = [
            element(DXGI_FORMAT_R8_UNORM, 0),
            element(DXGI_FORMAT_R16_FLOAT, APPEND),
            element(DXGI_FORMAT_R8_UNORM, APPEND),
        ];
        assert_eq!(offsets(&b), [0, 2, 4]);
        // Already aligned layouts are unchanged.
        let c = [
            element(DXGI_FORMAT_R8G8B8A8_UNORM, 0),
            element(DXGI_FORMAT_R32G32_FLOAT, APPEND),
            element(DXGI_FORMAT_R32G32B32_FLOAT, APPEND),
        ];
        assert_eq!(offsets(&c), [0, 4, 12]);
    }

    /// An explicit offset near 4 GiB overflowed the next appended offset
    /// (a panic in debug builds).
    #[test]
    fn a_huge_explicit_offset_does_not_overflow() {
        let inputs = [
            element(DXGI_FORMAT_R32G32B32A32_FLOAT, u32::MAX - 8),
            element(DXGI_FORMAT_R32_FLOAT, D3D12_APPEND_ALIGNED_ELEMENT),
        ];
        let _ = offsets(&inputs);
    }

    /// The streaming formatter and direct hasher must produce byte-for-byte
    /// identical output to `Debug` formatting so cached pipeline keys never drift.
    #[test]
    fn write_describe_matches_debug_format() {
        fn debug_describe(g: &GraphicsState) -> String {
            let inputs: Vec<String> = g
                .inputs
                .iter()
                .map(|e| {
                    // SAFETY: `e.SemanticName` is NUL-terminated.
                    let name = unsafe { CStr::from_ptr(e.SemanticName.0.cast()) };
                    format!(
                        "{name:?} {} {} {} {} {} {}",
                        e.SemanticIndex,
                        e.Format.0,
                        e.InputSlot,
                        e.AlignedByteOffset,
                        e.InputSlotClass.0,
                        e.InstanceDataStepRate
                    )
                })
                .collect();
            let depth_bounds = if g.depth_bounds { " depth bounds" } else { "" };
            format!(
                "{:?} {} {:?} {:?} {inputs:?} {} {} {:?} {} {:?}{depth_bounds}",
                g.blend,
                g.sample_mask,
                g.rasterizer,
                g.depth,
                g.strip_cut.0,
                g.topology.0,
                g.targets,
                g.depth_format.0,
                g.samples
            )
        }

        let check = |g: &GraphicsState| {
            let expected = debug_describe(g);
            assert_eq!(g.describe(), expected);
            let mut h1 = blake3::Hasher::new();
            h1.update(expected.as_bytes());
            let mut h2 = blake3::Hasher::new();
            g.hash_describe(&mut h2);
            assert_eq!(h1.finalize(), h2.finalize());
        };

        check(&GraphicsState::default());

        let mut custom = GraphicsState::default();
        custom.blend.AlphaToCoverageEnable = true.into();
        custom.blend.IndependentBlendEnable = true.into();
        custom.blend.RenderTarget[0].BlendEnable = true.into();
        custom.blend.RenderTarget[7].RenderTargetWriteMask = 0x5;
        custom.sample_mask = 0x1234_5678;
        custom.rasterizer.DepthBias = -42;
        custom.rasterizer.DepthBiasClamp = -0.0;
        custom.rasterizer.SlopeScaledDepthBias = 1.25;
        custom.depth.StencilEnable = true.into();
        custom.depth_bounds = true;
        custom.strip_cut = D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFF;
        custom.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        custom.targets.NumRenderTargets = 2;
        custom.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        custom.targets.RTFormats[1] = DXGI_FORMAT_R16G16B16A16_FLOAT;
        custom.depth_format = DXGI_FORMAT_D32_FLOAT;
        custom.samples = DXGI_SAMPLE_DESC { Count: 4, Quality: 1 };
        let weird = CString::new("A\"B\\C'D\tE\nF").unwrap();
        let elems = [
            element(DXGI_FORMAT_R32G32B32_FLOAT, 0),
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(weird.as_ptr().cast()),
                SemanticIndex: 3,
                Format: DXGI_FORMAT_R16G16_UNORM,
                InputSlot: 2,
                AlignedByteOffset: 12,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                InstanceDataStepRate: 1,
            },
        ];
        #[repr(C)]
        struct InputSub(u32, u32, D3D12_INPUT_LAYOUT_DESC);
        let sub = InputSub(
            12,
            0,
            D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: elems.as_ptr(),
                NumElements: elems.len() as u32,
            },
        );
        // SAFETY: `sub` is a valid kind-12 subobject.
        unsafe { custom.read(12, std::ptr::from_ref(&sub).cast()) };
        check(&custom);

        custom.rasterizer.DepthBiasClamp = f32::NAN;
        custom.rasterizer.SlopeScaledDepthBias = f32::INFINITY;
        check(&custom);
    }

    #[test]
    fn semantic_interning_reuses_common_and_caps_custom_names() {
        #[repr(C)]
        struct InputSub(u32, u32, D3D12_INPUT_LAYOUT_DESC);
        let mut g = GraphicsState::default();
        let std_elems = [
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"POSITION".as_ptr().cast()),
                ..element(DXGI_FORMAT_R32G32B32_FLOAT, 0)
            },
            element(DXGI_FORMAT_R32G32_FLOAT, 12),
        ];
        let sub = InputSub(
            12,
            0,
            D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: std_elems.as_ptr(),
                NumElements: std_elems.len() as u32,
            },
        );
        // SAFETY: valid kind-12 subobject.
        unsafe { g.read(12, std::ptr::from_ref(&sub).cast()) };
        assert!(g.names.is_empty(), "standard semantics need no owned CString");

        // Exceeding MAX_INTERNED_SEMANTICS falls back to owned storage on `self.names`.
        let many: Vec<CString> = (0..MAX_INTERNED_SEMANTICS + 8)
            .map(|i| CString::new(format!("TEST_OVERFLOW_SEM_{i}")).unwrap())
            .collect();
        let elems: Vec<D3D12_INPUT_ELEMENT_DESC> = many
            .iter()
            .map(|n| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(n.as_ptr().cast()),
                ..element(DXGI_FORMAT_R32_FLOAT, 0)
            })
            .collect();
        let sub = InputSub(
            12,
            0,
            D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: elems.as_ptr(),
                NumElements: elems.len() as u32,
            },
        );
        // SAFETY: valid kind-12 subobject.
        unsafe { g.read(12, std::ptr::from_ref(&sub).cast()) };
        assert!(g.names.len() >= 8);
        assert_eq!(g.inputs.len(), many.len());
        for (e, expect) in g.inputs.iter().zip(&many) {
            // SAFETY: every element's pointer stays valid while `g` lives.
            assert_eq!(unsafe { CStr::from_ptr(e.SemanticName.0.cast()) }, expect.as_c_str());
        }
    }
}
