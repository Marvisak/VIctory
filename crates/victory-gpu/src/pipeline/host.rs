//! Host pipelines for title pipelines (decision 0007, step 3): the
//! title's graphics state with the translated shaders, under a root
//! signature of our own that the translated shaders' bindings define
//! (`victory_shader::emit`): per stage one block of root constants for the
//! user data, root views for the title's root views, static samplers for
//! the immediate samplers, and directly indexed heaps.

use std::ffi::{CStr, CString};
use std::sync::Arc;

use victory_shader::emit::{ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::{Shader, Translated};
use crate::host::host;

/// A stream's graphics state, owned (the title's stream lives only for the
/// creation call). Subobjects the stream leaves out keep D3D12's defaults.
pub(crate) struct GraphicsState {
    blend: D3D12_BLEND_DESC,
    sample_mask: u32,
    rasterizer: D3D12_RASTERIZER_DESC,
    depth: D3D12_DEPTH_STENCIL_DESC,
    /// The stream's `DEPTH_STENCIL2` description as given (separate front
    /// and back stencil masks, the depth bounds flag); `depth` holds it
    /// narrowed to the front masks.
    depth2: Option<D3D12_DEPTH_STENCIL_DESC2>,
    /// Input elements, their names pointing into `names`.
    inputs: Vec<D3D12_INPUT_ELEMENT_DESC>,
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
            depth2: None,
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

impl GraphicsState {
    /// Copies owned input layout elements and semantic names from a title
    /// `D3D12_INPUT_LAYOUT_DESC`.
    ///
    /// # Safety
    /// If `layout.pInputElementDescs` is non-null, it must point to
    /// `layout.NumElements` valid `D3D12_INPUT_ELEMENT_DESC` entries whose
    /// non-null `SemanticName` pointers are NUL-terminated C strings.
    unsafe fn copy_input_layout(
        layout: D3D12_INPUT_LAYOUT_DESC,
    ) -> (Vec<D3D12_INPUT_ELEMENT_DESC>, Vec<CString>) {
        if layout.pInputElementDescs.is_null() || layout.NumElements == 0 {
            return (Vec::new(), Vec::new());
        }
        // SAFETY: caller guarantees `NumElements` valid input element entries.
        let elements = unsafe {
            std::slice::from_raw_parts(layout.pInputElementDescs, layout.NumElements as usize)
        };
        let names: Vec<CString> = elements
            .iter()
            .map(|e| {
                if e.SemanticName.0.is_null() {
                    CString::default()
                } else {
                    // SAFETY: caller guarantees valid NUL-terminated C string.
                    unsafe { CStr::from_ptr(e.SemanticName.0.cast()).to_owned() }
                }
            })
            .collect();
        let inputs = elements
            .iter()
            .zip(&names)
            .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(name.as_ptr().cast()),
                Format: DXGI_FORMAT(crate::resource::pc_format(e.Format.0 as u32) as i32),
                ..*e
            })
            .collect();
        (inputs, names)
    }

    /// Captures owned graphics state from a plain
    /// `D3D12_GRAPHICS_PIPELINE_STATE_DESC`.
    ///
    /// # Safety
    /// `desc` must be a valid `D3D12_GRAPHICS_PIPELINE_STATE_DESC` whose
    /// `InputLayout` pointers (if non-null) are valid for the duration of
    /// this call.
    pub(crate) unsafe fn from_graphics_desc(desc: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> Self {
        // SAFETY: caller guarantees `desc.InputLayout` is valid.
        let (inputs, names) = unsafe { Self::copy_input_layout(desc.InputLayout) };
        let mut rt_formats = desc.RTVFormats;
        for f in &mut rt_formats {
            f.0 = crate::resource::pc_format(f.0 as u32) as i32;
        }
        let samples = if desc.SampleDesc.Count == 0 {
            DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            }
        } else {
            desc.SampleDesc
        };
        GraphicsState {
            blend: desc.BlendState,
            sample_mask: desc.SampleMask,
            rasterizer: desc.RasterizerState,
            depth: desc.DepthStencilState,
            depth2: None,
            inputs,
            names,
            strip_cut: desc.IBStripCutValue,
            topology: desc.PrimitiveTopologyType,
            targets: D3D12_RT_FORMAT_ARRAY {
                RTFormats: rt_formats,
                NumRenderTargets: desc.NumRenderTargets.min(8),
            },
            depth_format: DXGI_FORMAT(crate::resource::pc_format(desc.DSVFormat.0 as u32) as i32),
            samples,
        }
    }

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
                    self.depth2 = None;
                }
                21 => {
                    self.depth2 = None;
                    // shortcut: the depth bounds test is dropped (the host
                    // pipeline uses the plain description); bring it back
                    // with a stream if a title enables it.
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC1>().read_unaligned();
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
                26 => {
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC2>().read_unaligned();
                    self.depth2 = Some(d);
                    self.depth = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.FrontFace.StencilReadMask,
                        StencilWriteMask: d.FrontFace.StencilWriteMask,
                        FrontFace: D3D12_DEPTH_STENCILOP_DESC {
                            StencilFailOp: d.FrontFace.StencilFailOp,
                            StencilDepthFailOp: d.FrontFace.StencilDepthFailOp,
                            StencilPassOp: d.FrontFace.StencilPassOp,
                            StencilFunc: d.FrontFace.StencilFunc,
                        },
                        BackFace: D3D12_DEPTH_STENCILOP_DESC {
                            StencilFailOp: d.BackFace.StencilFailOp,
                            StencilDepthFailOp: d.BackFace.StencilDepthFailOp,
                            StencilPassOp: d.BackFace.StencilPassOp,
                            StencilFunc: d.BackFace.StencilFunc,
                        },
                    };
                }
                27 => {
                    let d = data.cast::<D3D12_RASTERIZER_DESC1>().read_unaligned();
                    self.rasterizer = D3D12_RASTERIZER_DESC {
                        FillMode: d.FillMode,
                        CullMode: d.CullMode,
                        FrontCounterClockwise: d.FrontCounterClockwise,
                        DepthBias: d.DepthBias as i32,
                        DepthBiasClamp: d.DepthBiasClamp,
                        SlopeScaledDepthBias: d.SlopeScaledDepthBias,
                        DepthClipEnable: d.DepthClipEnable,
                        MultisampleEnable: d.MultisampleEnable,
                        AntialiasedLineEnable: d.AntialiasedLineEnable,
                        ForcedSampleCount: d.ForcedSampleCount,
                        ConservativeRaster: d.ConservativeRaster,
                    };
                }
                28 => {
                    let d = data.cast::<D3D12_RASTERIZER_DESC2>().read_unaligned();
                    let multisample = matches!(
                        d.LineRasterizationMode,
                        D3D12_LINE_RASTERIZATION_MODE_QUADRILATERAL_WIDE
                            | D3D12_LINE_RASTERIZATION_MODE_QUADRILATERAL_NARROW
                    );
                    let antialiased =
                        d.LineRasterizationMode == D3D12_LINE_RASTERIZATION_MODE_ALPHA_ANTIALIASED;
                    self.rasterizer = D3D12_RASTERIZER_DESC {
                        FillMode: d.FillMode,
                        CullMode: d.CullMode,
                        FrontCounterClockwise: d.FrontCounterClockwise,
                        DepthBias: d.DepthBias as i32,
                        DepthBiasClamp: d.DepthBiasClamp,
                        SlopeScaledDepthBias: d.SlopeScaledDepthBias,
                        DepthClipEnable: d.DepthClipEnable,
                        MultisampleEnable: multisample.into(),
                        AntialiasedLineEnable: antialiased.into(),
                        ForcedSampleCount: d.ForcedSampleCount,
                        ConservativeRaster: d.ConservativeRaster,
                    };
                }
                12 => {
                    let layout = at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned();
                    (self.inputs, self.names) = Self::copy_input_layout(layout);
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

    /// Everything the state holds, as text: part of a pipeline's name in
    /// the library and of the key a title pipeline is started early by.
    pub(crate) fn describe(&self) -> String {
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .zip(&self.names)
            .map(|(e, name)| {
                format!(
                    "{name:?} {} {} {} {} {} {}",
                    e.SemanticIndex, e.Format.0, e.InputSlot, e.AlignedByteOffset, e.InputSlotClass.0, e.InstanceDataStepRate
                )
            })
            .collect();
        format!(
            "{:?} {} {:?} {:?} {:?} {inputs:?} {} {} {:?} {} {:?}",
            self.blend,
            self.sample_mask,
            self.rasterizer,
            self.depth,
            self.depth2,
            self.strip_cut.0,
            self.topology.0,
            self.targets,
            self.depth_format.0,
            self.samples
        )
    }

    /// The `DEPTH_STENCIL2` description when the plain one would change
    /// what the pipeline does: stencil on with back-face masks that differ
    /// from the front ones.
    ///
    /// shortcut: the depth bounds flag is cleared (as for kind 21), since
    /// bounds are not forwarded yet; carry it through when they are.
    fn per_face_depth(&self) -> Option<D3D12_DEPTH_STENCIL_DESC2> {
        let d = self.depth2?;
        let differ = d.FrontFace.StencilReadMask != d.BackFace.StencilReadMask
            || d.FrontFace.StencilWriteMask != d.BackFace.StencilWriteMask;
        (d.StencilEnable.as_bool() && differ).then_some(D3D12_DEPTH_STENCIL_DESC2 {
            DepthBoundsTestEnable: false.into(),
            ..d
        })
    }

    /// The topology type of the host pipeline: the console's rectangle and
    /// quad types draw as triangles on the host (see `draw`).
    fn host_topology(&self) -> D3D12_PRIMITIVE_TOPOLOGY_TYPE {
        match self.topology.0 {
            RECTANGLES | QUADS => D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            _ => self.topology,
        }
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
                let offset = if e.AlignedByteOffset == D3D12_APPEND_ALIGNED_ELEMENT {
                    next[slot]
                } else {
                    e.AlignedByteOffset
                };
                let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                next[slot] = offset + bytes;
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
    /// The vertex buffer slots it reads (bit per slot).
    pub(crate) slots: u32,
    pub(crate) inputs: Vec<Input>,
    /// Its shaders, for reports of slow submissions.
    pub(crate) name: String,
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
            for (n, s) in t.static_samplers.iter().enumerate() {
                samplers.push(D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER(s.filter as i32),
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
                slots: 0,
                inputs: Vec::new(),
            });
        }
        let Some(g) = g else {
            return refuse("graphics state not kept (plain description)", shaders);
        };
        let state = g.describe();
        let depth2 = g.per_face_depth().filter(|_| {
            let kept = host.independent_stencil_masks();
            if !kept {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(
                        "back-face stencil masks differ from the front ones, but the host has no \
                         independent front and back stencil masks: the front ones are used for \
                         both (logged once)"
                    )
                });
            }
            kept
        });
        if mesh.is_some() {
            let name = super::library::name(
                "mesh",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                &state,
            );
            let pso = match stream_pipeline(
                &host.device,
                &root,
                code(0),
                code(1),
                false,
                g,
                depth2,
                &name,
            ) {
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
                slots: 0,
                inputs: Vec::new(),
            });
        }

        let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(0),
            PS: code(1),
            BlendState: g.blend,
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: g.inputs.as_ptr(),
                NumElements: g.inputs.len() as u32,
            },
            IBStripCutValue: g.strip_cut,
            PrimitiveTopologyType: g.host_topology(),
            NumRenderTargets: g.targets.NumRenderTargets,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        let name = super::library::name(
            "graphics",
            &root_blob,
            &[&code_bytes(&code(0)), &code_bytes(&code(1))],
            &state,
        );
        let made = if depth2.is_some() {
            // Separate front and back stencil masks need the stream form.
            stream_pipeline(
                &host.device,
                &root,
                code(0),
                code(1),
                true,
                g,
                depth2,
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
        tracing::info!(?shaders, "host pipeline");
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
            slots: g.slots(),
            inputs: g.elements(),
        })
    }
}

/// A pipeline-state stream being built: whole subobjects, each its type
/// then its description, pointer-aligned.
#[derive(Default)]
struct Stream(Vec<u64>);

impl Stream {
    fn push<T: Copy>(&mut self, kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, value: T) {
        // The layout of `struct alignas(void*) { TYPE type; T value; }`.
        let offset = size_of_val(&kind).next_multiple_of(align_of::<T>());
        let size = (offset + size_of::<T>()).next_multiple_of(8);
        let at = self.0.len();
        self.0.resize(at + size / 8, 0);
        // SAFETY: the `size` bytes just added hold the type at their start
        // and the value at `offset`; the writes are unaligned-tolerant.
        unsafe {
            let base = self.0.as_mut_ptr().add(at).cast::<u8>();
            base.cast::<D3D12_PIPELINE_STATE_SUBOBJECT_TYPE>()
                .write_unaligned(kind);
            base.add(offset).cast::<T>().write_unaligned(value);
        }
    }
}

/// A pipeline from the title's graphics state in the pipeline-state
/// stream form: a mesh pipeline (no input layout or topology), or with
/// `vertex` a vertex one; with `depth2` its depth-stencil state is the
/// `DEPTH_STENCIL2` subobject (separate front and back stencil masks).
#[allow(clippy::too_many_arguments)]
fn stream_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    first: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    vertex: bool,
    g: &GraphicsState,
    depth2: Option<D3D12_DEPTH_STENCIL_DESC2>,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    let mut stream = Stream::default();
    stream.push(
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
        root.as_raw(),
    );
    if vertex {
        stream.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS, first);
        stream.push(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT,
            D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: g.inputs.as_ptr(),
                NumElements: g.inputs.len() as u32,
            },
        );
        stream.push(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE,
            g.strip_cut,
        );
        stream.push(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
            g.host_topology(),
        );
    } else {
        stream.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, first);
    }
    stream.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps);
    stream.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, g.blend);
    stream.push(
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK,
        g.sample_mask,
    );
    stream.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer);
    match depth2 {
        Some(d) => stream.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2, d),
        None => stream.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, g.depth),
    }
    stream.push(
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
        g.targets,
    );
    stream.push(
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
        g.depth_format,
    );
    stream.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples);
    let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: stream.0.len() * 8,
        pPipelineStateSubobjectStream: stream.0.as_mut_ptr().cast(),
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

    /// A `DEPTH_STENCIL2` subobject (kind 26) with stencil on and back
    /// masks that differ from the front ones.
    fn depth2_subobject() -> (Vec<u8>, D3D12_DEPTH_STENCIL_DESC2) {
        let face = |read, write| D3D12_DEPTH_STENCILOP_DESC1 {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_INCR,
            StencilPassOp: D3D12_STENCIL_OP_REPLACE,
            StencilFunc: D3D12_COMPARISON_FUNC_EQUAL,
            StencilReadMask: read,
            StencilWriteMask: write,
        };
        let d = D3D12_DEPTH_STENCIL_DESC2 {
            DepthEnable: true.into(),
            DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
            DepthFunc: D3D12_COMPARISON_FUNC_GREATER_EQUAL,
            StencilEnable: true.into(),
            FrontFace: face(0x0f, 0xf0),
            BackFace: face(0x3c, 0xc3),
            DepthBoundsTestEnable: true.into(),
        };
        let mut sub = vec![0u8; 64];
        sub[..4].copy_from_slice(&26u32.to_le_bytes());
        // SAFETY: the description fits at +4 of the 64-byte subobject.
        unsafe {
            sub.as_mut_ptr()
                .add(4)
                .cast::<D3D12_DEPTH_STENCIL_DESC2>()
                .write_unaligned(d)
        };
        (sub, d)
    }

    #[test]
    fn depth_stencil2_keeps_back_face_masks() {
        let (sub, d) = depth2_subobject();
        let mut g = GraphicsState::default();
        // SAFETY: a whole kind 26 subobject.
        unsafe { g.read(26, sub.as_ptr()) };
        // The plain description falls back to the front masks.
        assert_eq!(
            (g.depth.StencilReadMask, g.depth.StencilWriteMask),
            (0x0f, 0xf0)
        );
        assert_eq!(g.depth.BackFace.StencilFunc, D3D12_COMPARISON_FUNC_EQUAL);
        // The host pipeline gets both faces' masks as given.
        let kept = g.per_face_depth().expect("back masks differ, stencil on");
        assert_eq!(
            (
                kept.BackFace.StencilReadMask,
                kept.BackFace.StencilWriteMask
            ),
            (0x3c, 0xc3)
        );
        assert_eq!(kept.FrontFace, d.FrontFace);
        assert_eq!(kept.DepthFunc, d.DepthFunc);
        // A later plain depth-stencil subobject replaces it.
        let mut plain = [0u8; 56];
        plain[..4].copy_from_slice(&11u32.to_le_bytes());
        // SAFETY: a whole kind 11 subobject.
        unsafe { g.read(11, plain.as_ptr()) };
        assert!(g.per_face_depth().is_none());
    }

    #[test]
    fn depth_stencil2_with_equal_masks_stays_plain_and_stream_is_laid_out() {
        let (mut sub, _) = depth2_subobject();
        // Back masks equal to the front ones: the plain description is exact.
        let back = 4
            + std::mem::offset_of!(D3D12_DEPTH_STENCIL_DESC2, BackFace)
            + std::mem::offset_of!(D3D12_DEPTH_STENCILOP_DESC1, StencilReadMask);
        sub[back..back + 2].copy_from_slice(&[0x0f, 0xf0]);
        let mut g = GraphicsState::default();
        // SAFETY: a whole kind 26 subobject.
        unsafe { g.read(26, sub.as_ptr()) };
        assert!(g.per_face_depth().is_none());

        // Stream subobjects: type, then the value at its alignment, each
        // padded to 8 bytes.
        let mut s = Stream::default();
        s.push(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK,
            0xdead_beefu32,
        );
        s.push(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
            0x1234usize as *mut u8,
        );
        assert_eq!(s.0, [0xdead_beef << 32 | 9, 0, 0x1234]);
        s.push(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2,
            g.depth2.unwrap(),
        );
        assert_eq!(s.0.len() * 8, 24 + 64);
        assert_eq!(s.0[3] as u32, 26);
    }
}
