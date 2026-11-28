//! Mesh shaders on a GPU with no mesh stage.
//!
//! The producer is a compute shader. It writes indices and vertices for at
//! most [`GROUPS`] groups. A vertex shader then draws that buffer. Indirect
//! mesh dispatches (the world's path) are capped on the GPU and replayed as
//! a dispatch plus an indexed draw, a few records at a time. Output buffers
//! live on the list and are reused, so a draw does not allocate.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::{HostList, MeshSlot, host, transition};
use crate::pipeline::host::{HostPipeline, MeshFallback};

use super::State;

/// Groups one output buffer can hold. A larger dispatch draws this many.
const GROUPS: u32 = 64;
/// Indirect records replayed per call. The rest are skipped.
const RECORDS: u32 = 8;

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0), UAV(u1), SRV(t0)"
cbuffer Limits : register(b0) {
    uint stride;
    uint records;
    uint cap;
    uint prims3;
};
RWByteAddressBuffer args : register(u0);
RWByteAddressBuffer draws : register(u1);
ByteAddressBuffer count : register(t0);

[RootSignature(RS)]
[numthreads(8, 1, 1)]
void main(uint id : SV_DispatchThreadID) {
    if (id >= records) return;
    uint n = count.Load(0);
    uint x = 0;
    if (id < n) {
        x = min(args.Load(id * stride), cap);
    }
    args.Store3(id * stride, uint3(x, 1, 1));
    draws.Store4(id * 20, uint4(x * prims3, 1, 0, 0));
    draws.Store(id * 20 + 16, 0);
}
"#;

struct Cap {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Cap {}
// SAFETY: as above.
unsafe impl Sync for Cap {}

fn cap() -> Option<&'static Cap> {
    static CAP: std::sync::OnceLock<Option<Cap>> = std::sync::OnceLock::new();
    CAP.get_or_init(|| match make_cap() {
        Ok(made) => Some(made),
        Err(e) => {
            tracing::warn!(error = %e, "no mesh fallback cap: indirect mesh draws dropped");
            None
        }
    })
    .as_ref()
}

fn make_cap() -> windows::core::Result<Cap> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("mesh-cap"),
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
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("mesh fallback cap: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Cap { root, pso })
    }
}

/// A count buffer of `u32::MAX`, so a draw with no count runs every record.
fn open_count() -> Option<u64> {
    struct Held {
        _buffer: ID3D12Resource,
        gpu: u64,
    }
    // SAFETY: the buffer is immutable after this write and lives forever.
    unsafe impl Send for Held {}
    // SAFETY: no thread mutates the buffer after publication.
    unsafe impl Sync for Held {}
    static OPEN: std::sync::OnceLock<Option<Held>> = std::sync::OnceLock::new();
    OPEN.get_or_init(|| {
        let (buffer, data) = host()?.shared_buffer(256)?;
        // SAFETY: the mapping covers the first dword.
        unsafe { data.cast::<u32>().write(u32::MAX) };
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        Some(Held { _buffer: buffer, gpu })
    })
    .as_ref()
    .map(|h| h.gpu)
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

fn to(
    list: &HostList,
    resource: &ID3D12Resource,
    state: &mut D3D12_RESOURCE_STATES,
    next: D3D12_RESOURCE_STATES,
) {
    if state.0 == next.0 {
        return;
    }
    // SAFETY: an open list and a live buffer.
    unsafe { list.list.ResourceBarrier(&[transition(resource, *state, next)]) };
    *state = next;
}

fn remember(list: &HostList, slot: &MeshSlot, index: D3D12_RESOURCE_STATES, vertex: D3D12_RESOURCE_STATES, draws: D3D12_RESOURCE_STATES) {
    list.mesh_remember(&slot.index, index, &slot.vertex, vertex, &slot.draws, draws);
}

/// One direct mesh dispatch of `groups` groups.
pub(crate) fn direct(state: &mut State, list: &HostList, pipeline: &HostPipeline, groups: u32) -> bool {
    let Some(fallback) = pipeline.fallback.as_deref() else {
        return false;
    };
    let groups = groups.min(GROUPS);
    if groups == 0 {
        return true;
    }
    let Some(slot) = outputs(list, fallback) else {
        return false;
    };
    let mut states = MeshStates { index: slot.index_state, vertex: slot.vertex_state };
    let ran = draw_groups(state, list, fallback, groups, &slot, &mut states, None);
    remember(list, &slot, states.index, states.vertex, slot.draws_state);
    ran
}

/// Indirect mesh records. `arguments` is the clamped buffer, already in the
/// indirect-argument state. At most [`RECORDS`] records run.
pub(crate) fn indirect(
    state: &mut State,
    list: &HostList,
    fallback: &MeshFallback,
    arguments: &ID3D12Resource,
    max: u32,
    count_gpu: Option<u64>,
) -> bool {
    let records = max.min(RECORDS);
    if max > RECORDS {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(max, kept = RECORDS, "indirect mesh fallback keeps the first records");
        });
    }
    if records == 0 {
        return true;
    }
    let Some(slot) = outputs(list, fallback) else {
        return false;
    };
    let Some(shader) = cap() else {
        super::not_run("no mesh fallback cap");
        return false;
    };
    let Some(count_gpu) = count_gpu.filter(|g| *g != 0).or_else(open_count) else {
        super::not_run("no mesh fallback count");
        return false;
    };
    let Some(dispatch) = super::host_signature(2, 12) else {
        super::not_run("no mesh fallback dispatch signature");
        return false;
    };
    let Some(draw) = super::host_signature(1, 20) else {
        super::not_run("no mesh fallback draw signature");
        return false;
    };
    let mut states = MeshStates { index: slot.index_state, vertex: slot.vertex_state };
    let mut draws_state = slot.draws_state;
    let prims3 = fallback.layout.max_primitives.saturating_mul(3);
    let capped = apply_cap(
        list,
        shader,
        arguments,
        &slot.draws,
        &mut draws_state,
        CapWork { records, prims3, count_gpu },
    );
    if !capped {
        remember(list, &slot, states.index, states.vertex, draws_state);
        super::not_run("mesh fallback cap was not recorded");
        return false;
    }
    // The cap shader replaced the compute pipeline. Bind sees a cleared
    // tracker (the caller zeroed it) and sets the producer.
    let mut ran = true;
    for i in 0..records {
        let produced = draw_groups(
            state,
            list,
            fallback,
            0,
            &slot,
            &mut states,
            Some(Record {
                dispatch: &dispatch,
                draw: &draw,
                arguments,
                draws: &slot.draws,
                index: i,
            }),
        );
        if !produced {
            ran = false;
            break;
        }
    }
    remember(list, &slot, states.index, states.vertex, draws_state);
    if !ran {
        super::not_run_if_empty("mesh fallback indirect record was not recorded");
    }
    ran
}

struct Record<'a> {
    dispatch: &'a ID3D12CommandSignature,
    draw: &'a ID3D12CommandSignature,
    arguments: &'a ID3D12Resource,
    draws: &'a ID3D12Resource,
    index: u32,
}

fn outputs(list: &HostList, fallback: &MeshFallback) -> Option<MeshSlot> {
    let (index_bytes, vertex_bytes) = fallback.layout.bytes_for_groups(GROUPS)?;
    match list.mesh_slot(index_bytes, vertex_bytes) {
        Some(slot) => Some(slot),
        None => {
            super::not_run("no mesh fallback buffers");
            None
        }
    }
}

struct CapWork {
    records: u32,
    prims3: u32,
    count_gpu: u64,
}

fn apply_cap(
    list: &HostList,
    shader: &Cap,
    arguments: &ID3D12Resource,
    draws: &ID3D12Resource,
    draws_state: &mut D3D12_RESOURCE_STATES,
    work: CapWork,
) -> bool {
    to(list, draws, draws_state, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    let constants = [12u32, work.records, GROUPS, work.prims3];
    // SAFETY: an open list, our shader and the list's buffers.
    unsafe {
        list.list.ResourceBarrier(&[
            uav_barrier(),
            transition(arguments, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
        ]);
        list.list.SetComputeRootSignature(&shader.root);
        list.list.SetPipelineState(&shader.pso);
        list.list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        list.list.SetComputeRootUnorderedAccessView(1, arguments.GetGPUVirtualAddress());
        list.list.SetComputeRootUnorderedAccessView(2, draws.GetGPUVirtualAddress());
        list.list.SetComputeRootShaderResourceView(3, work.count_gpu);
        list.list.Dispatch(1, 1, 1);
        list.list.ResourceBarrier(&[
            uav_barrier(),
            transition(arguments, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT),
            transition(draws, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT),
        ]);
    }
    *draws_state = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;
    true
}

struct MeshStates {
    index: D3D12_RESOURCE_STATES,
    vertex: D3D12_RESOURCE_STATES,
}

/// Produce `groups` (direct) or one indirect record, then draw it.
/// `record` selects the indirect path; `groups` is then ignored.
fn draw_groups(
    state: &mut State,
    list: &HostList,
    fallback: &MeshFallback,
    groups: u32,
    slot: &MeshSlot,
    states: &mut MeshStates,
    record: Option<Record<'_>>,
) -> bool {
    to(list, &slot.index, &mut states.index, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    to(list, &slot.vertex, &mut states.vertex, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    if !state.bind_roots(list, &fallback.compute, true, false) {
        return false;
    }
    // SAFETY: an open list and the list's own buffers.
    unsafe {
        list.list.SetComputeRootUnorderedAccessView(fallback.index_uav, slot.index.GetGPUVirtualAddress());
        list.list.SetComputeRootUnorderedAccessView(fallback.vertex_uav, slot.vertex.GetGPUVirtualAddress());
        match &record {
            None => list.list.Dispatch(groups, 1, 1),
            Some(record) => {
                list.list.ExecuteIndirect(
                    record.dispatch,
                    1,
                    record.arguments,
                    u64::from(record.index) * 12,
                    None,
                    0,
                );
            }
        }
        list.list.ResourceBarrier(&[uav_barrier()]);
    }
    to(list, &slot.index, &mut states.index, D3D12_RESOURCE_STATE_INDEX_BUFFER);
    to(list, &slot.vertex, &mut states.vertex, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    if !state.prepare(list, false) {
        return false;
    }
    let topology = state.topology;
    // SAFETY: an open list, the list's buffers, and a pipeline prepare bound.
    unsafe {
        list.list.SetGraphicsRootShaderResourceView(fallback.vertex_srv, slot.vertex.GetGPUVirtualAddress());
        let view = D3D12_INDEX_BUFFER_VIEW {
            BufferLocation: slot.index.GetGPUVirtualAddress(),
            SizeInBytes: u32::try_from(slot.index_bytes).unwrap_or(u32::MAX),
            Format: DXGI_FORMAT_R32_UINT,
        };
        list.list.IASetIndexBuffer(Some(&view));
        list.list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(4));
        match &record {
            None => {
                let indices = fallback.layout.index_count(groups).unwrap_or(0);
                list.list.DrawIndexedInstanced(indices, 1, 0, 0, 0);
            }
            Some(record) => {
                list.list.ExecuteIndirect(record.draw, 1, record.draws, u64::from(record.index) * 20, None, 0);
            }
        }
    }
    if topology != 0 {
        state.set_topology(list, topology);
    }
    to(list, &slot.index, &mut states.index, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    to(list, &slot.vertex, &mut states.vertex, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
    true
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY, ID3DBlob};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::{PCSTR, s};

    use super::{CapWork, GROUPS, apply_cap, cap, open_count, uav_barrier};
    use crate::host::{host, transition};

    fn compile(source: &str, target: &str) -> Vec<u8> {
        let target = std::ffi::CString::new(target).unwrap();
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source and NUL-terminated target stay live throughout
        // D3DCompile; the output slots are valid writable option pointers.
        let compiled = unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                s!("mesh-test"),
                None,
                None,
                s!("main"),
                PCSTR(target.as_ptr().cast()),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(e) = compiled {
            let text = errors
                .as_ref()
                .map(|errors| {
                    // SAFETY: the error blob owns this byte range during copy.
                    unsafe {
                        let bytes = std::slice::from_raw_parts(
                            errors.GetBufferPointer().cast::<u8>(),
                            errors.GetBufferSize(),
                        );
                        String::from_utf8_lossy(bytes).into_owned()
                    }
                })
                .unwrap_or_default();
            panic!("compile failed: {e}: {text}");
        }
        let code = code.unwrap();
        // SAFETY: the compiled blob owns the byte range copied here.
        unsafe {
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
                .to_vec()
        }
    }

    fn buffer(device: &ID3D12Device, size: u64, kind: D3D12_HEAP_TYPE, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let mut made = None;
        // SAFETY: the device, buffer description, and output slot are live.
        unsafe {
            device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made)
                .unwrap();
        }
        made.unwrap()
    }

    #[test]
    fn cap_clamps_group_count_and_writes_draw_args() {
        let Some(host) = host() else {
            eprintln!("skipped mesh cap: no host device");
            return;
        };
        let list = host.new_list().unwrap();
        let (args, ptr) = host.shared_buffer(256).unwrap();
        // SAFETY: the mapping covers three dwords.
        unsafe {
            let words = ptr.cast::<u32>();
            words.write_volatile(1000);
            words.add(1).write_volatile(5);
            words.add(2).write_volatile(9);
        }
        let draws = buffer(
            &host.device,
            256,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        );
        let readback = buffer(
            &host.device,
            256,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        // SAFETY: the command list is open and args is a live resource in
        // the COMMON state until this barrier records its transition.
        unsafe {
            list.list.ResourceBarrier(&[transition(
                &args,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            )]);
        }
        let mut draws_state = D3D12_RESOURCE_STATE_COMMON;
        let shader = cap().expect("cap shader");
        let count = open_count().expect("open count");
        assert!(apply_cap(
            &list,
            shader,
            &args,
            &draws,
            &mut draws_state,
            CapWork { records: 1, prims3: 3, count_gpu: count }
        ));
        // SAFETY: this open list owns the live args, draws, and readback
        // resources; their state transitions precede the copy commands.
        unsafe {
            list.list.ResourceBarrier(&[
                transition(&args, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COPY_SOURCE),
                transition(&draws, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COPY_SOURCE),
            ]);
            list.list.CopyBufferRegion(&readback, 0, &args, 0, 12);
            list.list.CopyBufferRegion(&readback, 64, &draws, 0, 20);
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: Map exposes at least the requested 84-byte readback range;
        // the slices are copied before Unmap and the resource stays alive.
        let words = unsafe {
            readback
                .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 84 }), Some(&mut mapped))
                .unwrap();
            let args = std::slice::from_raw_parts(mapped.cast::<u32>(), 3).to_vec();
            let draws = std::slice::from_raw_parts(mapped.add(64).cast::<u32>(), 5).to_vec();
            readback.Unmap(0, Some(&D3D12_RANGE::default()));
            (args, draws)
        };
        assert_eq!(words.0, vec![GROUPS, 1, 1], "clamped dispatch");
        assert_eq!(words.1, vec![GROUPS * 3, 1, 0, 0, 0], "indexed draw args");
    }

    #[test]
    fn compute_written_triangle_is_visible() {
        let Some(host) = host() else {
            eprintln!("skipped mesh triangle: no host device");
            return;
        };
        let cs = compile(
            r#"
            #define RS "UAV(u0, space=3), UAV(u1, space=3)"
            RWByteAddressBuffer indices : register(u0, space3);
            RWByteAddressBuffer verts : register(u1, space3);
            [RootSignature(RS)]
            [numthreads(1, 1, 1)]
            void main() {
                indices.Store3(0, uint3(0, 1, 2));
                verts.Store4(0, asuint(float4(-1, -1, 0, 1)));
                verts.Store4(16, asuint(float4(1, 0, 0, 1)));
                verts.Store4(32, asuint(float4(1, -1, 0, 1)));
                verts.Store4(48, asuint(float4(1, 0, 0, 1)));
                verts.Store4(64, asuint(float4(0, 1, 0, 1)));
                verts.Store4(80, asuint(float4(1, 0, 0, 1)));
            }
            "#,
            "cs_5_1",
        );
        let vs = compile(
            r#"
            #define RS "SRV(t0, space=3, visibility=SHADER_VISIBILITY_VERTEX)"
            ByteAddressBuffer verts : register(t0, space3);
            struct Out { float4 pos : SV_Position; float4 color : TEXCOORD0; };
            [RootSignature(RS)]
            Out main(uint id : SV_VertexID) {
                Out o;
                o.pos = asfloat(verts.Load4(id * 32));
                o.color = asfloat(verts.Load4(id * 32 + 16));
                return o;
            }
            "#,
            "vs_5_1",
        );
        let ps = compile(
            "struct In { float4 pos : SV_Position; float4 color : TEXCOORD0; }; float4 main(In i) : SV_Target { return i.color; }",
            "ps_5_1",
        );
        let device = &host.device;
        // SAFETY: the compiled vertex bytecode remains live for this call.
        let root: ID3D12RootSignature = unsafe { device.CreateRootSignature(0, &vs).unwrap() };
        // SAFETY: the compiled compute bytecode remains live for this call.
        let compute_root: ID3D12RootSignature = unsafe { device.CreateRootSignature(0, &cs).unwrap() };
        let mut targets = [DXGI_FORMAT_UNKNOWN; 8];
        targets[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        // SAFETY: root and both shader bytecode vectors remain live during
        // pipeline creation; the descriptor names valid D3D12 state.
        let pso: ID3D12PipelineState = unsafe {
            device
                .CreateGraphicsPipelineState(&D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    VS: D3D12_SHADER_BYTECODE { pShaderBytecode: vs.as_ptr().cast(), BytecodeLength: vs.len() },
                    PS: D3D12_SHADER_BYTECODE { pShaderBytecode: ps.as_ptr().cast(), BytecodeLength: ps.len() },
                    SampleMask: u32::MAX,
                    BlendState: {
                        let mut blend = D3D12_BLEND_DESC::default();
                        blend.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
                        blend
                    },
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    RTVFormats: targets,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    ..Default::default()
                })
                .unwrap_or_else(|e| {
                    panic!(
                        "{e}; {}",
                        host.log_messages().unwrap_or_else(|| "no debug message".into())
                    )
                })
        };
        // SAFETY: the compute root and bytecode remain live for this call.
        let cs_pso: ID3D12PipelineState = unsafe {
            device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&compute_root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: cs.as_ptr().cast(), BytecodeLength: cs.len() },
                    ..Default::default()
                })
                .unwrap()
        };
        let index = buffer(device, 256, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON);
        let vertex = buffer(device, 256, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON);
        let rt_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 8,
            Height: 8,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let rt_heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
        let mut rt = None;
        // SAFETY: the device, render-target description and output slot live
        // through committed resource creation.
        unsafe {
            device
                .CreateCommittedResource(&rt_heap, D3D12_HEAP_FLAG_NONE, &rt_desc, D3D12_RESOURCE_STATE_RENDER_TARGET, None, &mut rt)
                .unwrap();
        }
        let rt = rt.unwrap();
        let readback = buffer(device, 256 * 8, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST);
        let rtv = host.rtv(&rt, None);
        let list = host.new_list().unwrap();
        let l = &list.list;
        // SAFETY: the list is open and every referenced resource and pipeline
        // stays live through command recording and execution below.
        unsafe {
            l.ResourceBarrier(&[
                transition(&index, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
                transition(&vertex, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
            ]);
            l.SetComputeRootSignature(&compute_root);
            l.SetPipelineState(&cs_pso);
            l.SetComputeRootUnorderedAccessView(0, index.GetGPUVirtualAddress());
            l.SetComputeRootUnorderedAccessView(1, vertex.GetGPUVirtualAddress());
            l.Dispatch(1, 1, 1);
            l.ResourceBarrier(&[
                uav_barrier(),
                transition(&index, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_INDEX_BUFFER),
                transition(&vertex, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
            ]);
            l.SetGraphicsRootSignature(&root);
            l.SetPipelineState(&pso);
            l.SetGraphicsRootShaderResourceView(0, vertex.GetGPUVirtualAddress());
            l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(4));
            l.IASetIndexBuffer(Some(&D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: index.GetGPUVirtualAddress(),
                SizeInBytes: 12,
                Format: DXGI_FORMAT_R32_UINT,
            }));
            l.RSSetViewports(&[D3D12_VIEWPORT { Width: 8.0, Height: 8.0, MaxDepth: 1.0, ..Default::default() }]);
            l.RSSetScissorRects(&[RECT { left: 0, top: 0, right: 8, bottom: 8 }]);
            l.OMSetRenderTargets(1, Some(&rtv), false, None);
            l.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0], None);
            l.DrawIndexedInstanced(3, 1, 0, 0, 0);
            l.ResourceBarrier(&[transition(&rt, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            let mut from = D3D12_TEXTURE_COPY_LOCATION {
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                ..Default::default()
            };
            let mut to = D3D12_TEXTURE_COPY_LOCATION {
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: 8,
                            Height: 8,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
                ..Default::default()
            };
            from.pResource = std::mem::transmute_copy(&rt);
            to.pResource = std::mem::transmute_copy(&readback);
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: Map exposes the requested readback bytes; they are copied
        // into an owned Vec before Unmap.
        let image = unsafe {
            readback
                .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 256 * 8 }), Some(&mut mapped))
                .unwrap();
            let image = std::slice::from_raw_parts(mapped.cast::<u8>(), 256 * 8).to_vec();
            readback.Unmap(0, Some(&D3D12_RANGE::default()));
            image
        };
        let pixel = image[4 * 256 + 4 * 4..4 * 256 + 4 * 4 + 4].to_vec();
        if pixel != [255, 0, 0, 255] {
            let rows = (0..8)
                .map(|y| {
                    (0..8)
                        .map(|x| {
                            let p = &image[y * 256 + x * 4..y * 256 + x * 4 + 4];
                            format!("{:02x}{:02x}{:02x}{:02x}", p[0], p[1], p[2], p[3])
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
                .join("\n");
            panic!("center pixel {pixel:?} of the compute-written triangle\n{rows}");
        }
    }
}
