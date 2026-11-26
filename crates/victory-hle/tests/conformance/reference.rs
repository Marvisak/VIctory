//! The GPU frame's reference: the conformance title's scene
//! (`tests/fixtures/conformance/src/scene.rs`) drawn natively on WARP, the
//! D3D12 software adapter, from HLSL equivalent to the title's GFX10
//! shaders, with the same resources and commands. Returns the compute
//! pass's output: RGBA floats per pixel.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_11_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory4};
use windows::core::{Interface, PCSTR, Result, s};

use crate::scene::{self, HEIGHT, WIDTH};

const GRAPHICS: &str = r#"
#define RS "RootFlags(ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT), DescriptorTable(CBV(b0)), DescriptorTable(CBV(b1)), DescriptorTable(SRV(t0)), DescriptorTable(Sampler(s0))"
cbuffer Scene : register(b0) { float4 vp[4]; float4 light; float4 eye; float4 lightColour; float4 ambient; };
cbuffer Mesh : register(b1) { float4 world[4]; float4 albedoShininess; float4 specular; float4 emissive; };
Texture2D tex : register(t0);
SamplerState smp : register(s0);
struct V { float4 pos : SV_Position; float3 wp : TEXCOORD0; float3 n : TEXCOORD1; float2 uv : TEXCOORD2; };
[RootSignature(RS)]
V vs(float3 p : POSITION, float3 n : NORMAL, float2 uv : TEXCOORD) {
    V o;
    float4 p1 = float4(p, 1);
    o.wp = float3(dot(world[0], p1), dot(world[1], p1), dot(world[2], p1));
    o.n = float3(dot(world[0].xyz, n), dot(world[1].xyz, n), dot(world[2].xyz, n));
    float4 w1 = float4(o.wp, 1);
    o.pos = float4(dot(vp[0], w1), dot(vp[1], w1), dot(vp[2], w1), dot(vp[3], w1));
    o.uv = uv;
    return o;
}
[RootSignature(RS)]
float4 ps(V i) : SV_Target {
    float3 n = i.n * rsqrt(dot(i.n, i.n));
    float3 v = eye.xyz - i.wp;
    v = v * rsqrt(dot(v, v));
    float3 h = light.xyz + v;
    h = h * rsqrt(dot(h, h));
    float nl = max(dot(light.xyz, n), 0);
    float nh = max(dot(h, n), 0);
    float s = exp2(albedoShininess.w * log2(nh));
    float4 t = tex.Sample(smp, i.uv);
    float3 c = (t.rgb * (ambient.rgb + nl * lightColour.rgb)) * albedoShininess.rgb;
    c = c + (specular.rgb * s) * lightColour.rgb;
    return float4(emissive.rgb + c, 1);
}
"#;

const COMPUTE: &str = r#"
#define RS "DescriptorTable(SRV(t0), UAV(u0), UAV(u1))"
Texture2D<float4> frame : register(t0);
RWTexture2D<float4> shown : register(u0);
RWByteAddressBuffer output : register(u1);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void cs(uint3 id : SV_DispatchThreadID) {
    float4 c = frame.Load(int3(id.xy, 0));
    float4 l = frame.Load(int3(int(id.x) - 1, id.y, 0));
    float4 r = frame.Load(int3(id.x + 1, id.y, 0));
    float4 o = (l + r) * 0.25 + 0.5 * c;
    shown[id.xy] = o;
    output.Store4((id.y * WIDTH + id.x) * 16, asuint(o));
}
"#;

fn compile(source: &str, entry: PCSTR, target: PCSTR) -> Vec<u8> {
    let source = source.replace("WIDTH", &WIDTH.to_string());
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    // SAFETY: the source and names outlive the call.
    let r = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("reference"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let bytes = |b: &ID3DBlob| {
        // SAFETY: the blob's own bytes.
        unsafe { std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()) }
            .to_vec()
    };
    if let Err(e) = r {
        panic!(
            "compiling the reference shaders: {e}: {}",
            String::from_utf8_lossy(&errors.as_ref().map(bytes).unwrap_or_default())
        );
    }
    bytes(&code.unwrap())
}

fn heap(kind: D3D12_HEAP_TYPE) -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: kind,
        ..Default::default()
    }
}

fn buffer(width: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: width,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

fn texture(
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(width),
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: flags,
        ..Default::default()
    }
}

fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// The scene's frame at time 0 drawn on WARP, or why not.
pub fn render() -> Result<Vec<f32>> {
    // SAFETY: D3D12 calls with descriptions that outlive them; every
    // resource lives until the fence says the GPU is done with it.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device)?;
        let device = device.unwrap();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let allocator: ID3D12CommandAllocator =
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;

        let vs = compile(GRAPHICS, s!("vs"), s!("vs_5_1"));
        let ps = compile(GRAPHICS, s!("ps"), s!("ps_5_1"));
        let cs = compile(COMPUTE, s!("cs"), s!("cs_5_1"));
        let graphics_root: ID3D12RootSignature = device.CreateRootSignature(0, &vs)?;
        let compute_root: ID3D12RootSignature = device.CreateRootSignature(0, &cs)?;
        let element = |name: PCSTR, format, offset| D3D12_INPUT_ELEMENT_DESC {
            SemanticName: name,
            Format: format,
            AlignedByteOffset: offset,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            ..Default::default()
        };
        let elements = [
            element(s!("POSITION"), DXGI_FORMAT_R32G32B32_FLOAT, 0),
            element(s!("NORMAL"), DXGI_FORMAT_R32G32B32_FLOAT, 12),
            element(s!("TEXCOORD"), DXGI_FORMAT_R32G32_FLOAT, 24),
        ];
        let code = |b: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: b.as_ptr().cast(),
            BytecodeLength: b.len(),
        };
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&graphics_root),
            VS: code(&vs),
            PS: code(&ps),
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_BACK,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                ..Default::default()
            },
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: elements.as_ptr(),
                NumElements: elements.len() as u32,
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            DSVFormat: DXGI_FORMAT_D32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        desc.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        desc.BlendState.RenderTarget[0].RenderTargetWriteMask =
            D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8;
        let graphics: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
        let compute: ID3D12PipelineState =
            device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&compute_root),
                CS: code(&cs),
                ..Default::default()
            })?;

        let make = |kind,
                    desc: D3D12_RESOURCE_DESC,
                    state,
                    clear: Option<D3D12_CLEAR_VALUE>|
         -> Result<ID3D12Resource> {
            let mut r: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &heap(kind),
                D3D12_HEAP_FLAG_NONE,
                &desc,
                state,
                clear.as_ref().map(|c| c as *const _),
                &mut r,
            )?;
            Ok(r.unwrap())
        };
        let write = |r: &ID3D12Resource, bytes: &[u8]| -> Result<()> {
            let mut at = std::ptr::null_mut();
            r.Map(0, None, Some(&mut at))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), at.cast::<u8>(), bytes.len());
            r.Unmap(0, None);
            Ok(())
        };
        let geometry = scene::Geometry::new();
        let vertex_bytes = std::slice::from_raw_parts(
            geometry.vertices.as_ptr().cast::<u8>(),
            geometry.vertices.len() * size_of::<scene::Vertex>(),
        );
        let index_bytes: Vec<u8> = geometry
            .indices
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect();
        let upload = D3D12_HEAP_TYPE_UPLOAD;
        let read = D3D12_RESOURCE_STATE_GENERIC_READ;
        let none = D3D12_RESOURCE_FLAG_NONE;
        let vertices = make(upload, buffer(vertex_bytes.len() as u64, none), read, None)?;
        write(&vertices, vertex_bytes)?;
        let indices = make(
            upload,
            buffer(index_bytes.len().next_multiple_of(4) as u64, none),
            read,
            None,
        )?;
        write(&indices, &index_bytes)?;
        let (scene, draws) = scene::frame(0.0);
        let mut constants_data =
            std::slice::from_raw_parts((&raw const scene).cast::<u8>(), 256).to_vec();
        for (_, m) in &draws {
            constants_data.extend(std::slice::from_raw_parts(
                (&raw const *m).cast::<u8>(),
                256,
            ));
        }
        let constants = make(
            upload,
            buffer(constants_data.len() as u64, none),
            read,
            None,
        )?;
        write(&constants, &constants_data)?;
        let texels = scene::texels();
        let staging = make(upload, buffer(texels.len() as u64, none), read, None)?;
        write(&staging, &texels)?;
        let default = D3D12_HEAP_TYPE_DEFAULT;
        let rgba8 = DXGI_FORMAT_R8G8B8A8_UNORM;
        let tex = make(
            default,
            texture(scene::TEXTURE, scene::TEXTURE, rgba8, none),
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
        )?;
        let clear = scene::CLEAR.map(|c| f32::from(c) / 255.0);
        let target = make(
            default,
            texture(
                WIDTH,
                HEIGHT,
                rgba8,
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ),
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            Some(D3D12_CLEAR_VALUE {
                Format: rgba8,
                Anonymous: D3D12_CLEAR_VALUE_0 { Color: clear },
            }),
        )?;
        let depth = make(
            default,
            texture(
                WIDTH,
                HEIGHT,
                DXGI_FORMAT_D32_FLOAT,
                D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            ),
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            Some(D3D12_CLEAR_VALUE {
                Format: DXGI_FORMAT_D32_FLOAT,
                Anonymous: D3D12_CLEAR_VALUE_0 {
                    DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                        Depth: 1.0,
                        Stencil: 0,
                    },
                },
            }),
        )?;
        let uav = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        let unordered = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let shown = make(default, texture(WIDTH, HEIGHT, rgba8, uav), unordered, None)?;
        let frame_bytes = u64::from(WIDTH * HEIGHT * 16);
        let output = make(default, buffer(frame_bytes, uav), unordered, None)?;
        let readback = make(
            D3D12_HEAP_TYPE_READBACK,
            buffer(frame_bytes, none),
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
        )?;

        let heap_of = |kind, count, flags| -> Result<ID3D12DescriptorHeap> {
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: count,
                Flags: flags,
                NodeMask: 0,
            })
        };
        let visible = D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE;
        let views = heap_of(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 16, visible)?;
        let samplers = heap_of(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, 1, visible)?;
        let rtvs = heap_of(
            D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            1,
            D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        )?;
        let dsvs = heap_of(
            D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
            1,
            D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        )?;
        let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
            as usize;
        let cpu = |n: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: views.GetCPUDescriptorHandleForHeapStart().ptr + n * step,
        };
        let gpu = |n: u64| D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: views.GetGPUDescriptorHandleForHeapStart().ptr + n * step as u64,
        };
        for n in 0..5 {
            device.CreateConstantBufferView(
                Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                    BufferLocation: constants.GetGPUVirtualAddress() + 256 * n as u64,
                    SizeInBytes: 256,
                }),
                cpu(n),
            );
        }
        device.CreateShaderResourceView(&tex, None, cpu(5));
        device.CreateShaderResourceView(&target, None, cpu(6));
        device.CreateUnorderedAccessView(&shown, None, None, cpu(7));
        device.CreateUnorderedAccessView(
            &output,
            None,
            Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        NumElements: (frame_bytes / 4) as u32,
                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                        ..Default::default()
                    },
                },
            }),
            cpu(8),
        );
        device.CreateSampler(
            &D3D12_SAMPLER_DESC {
                Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            },
            samplers.GetCPUDescriptorHandleForHeapStart(),
        );
        let rtv = rtvs.GetCPUDescriptorHandleForHeapStart();
        let dsv = dsvs.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(&target, None, rtv);
        device.CreateDepthStencilView(&depth, None, dsv);

        // The texture's upload.
        let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
            Offset: 0,
            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                Format: rgba8,
                Width: scene::TEXTURE,
                Height: scene::TEXTURE,
                Depth: 1,
                RowPitch: scene::TEXTURE * 4,
            },
        };
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&tex),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&staging),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        list.ResourceBarrier(&[transition(
            &tex,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        )]);

        // The draws.
        list.SetDescriptorHeaps(&[Some(views.clone()), Some(samplers.clone())]);
        list.SetGraphicsRootSignature(&graphics_root);
        list.ClearRenderTargetView(rtv, &clear, None);
        list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 1.0, 0, None);
        list.OMSetRenderTargets(1, Some(&rtv), false, Some(&dsv));
        list.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: WIDTH as f32,
            Height: HEIGHT as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: WIDTH as i32,
            bottom: HEIGHT as i32,
        }]);
        list.IASetPrimitiveTopology(
            windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        );
        list.SetPipelineState(&graphics);
        list.IASetVertexBuffers(
            0,
            Some(&[D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: vertices.GetGPUVirtualAddress(),
                SizeInBytes: vertex_bytes.len() as u32,
                StrideInBytes: scene::STRIDE,
            }]),
        );
        list.IASetIndexBuffer(Some(&D3D12_INDEX_BUFFER_VIEW {
            BufferLocation: indices.GetGPUVirtualAddress(),
            SizeInBytes: index_bytes.len() as u32,
            Format: DXGI_FORMAT_R16_UINT,
        }));
        list.SetGraphicsRootDescriptorTable(0, gpu(0));
        list.SetGraphicsRootDescriptorTable(2, gpu(5));
        list.SetGraphicsRootDescriptorTable(3, samplers.GetGPUDescriptorHandleForHeapStart());
        for (n, &(mesh, _)) in draws.iter().enumerate() {
            let m = geometry.meshes[mesh];
            list.SetGraphicsRootDescriptorTable(1, gpu(n as u64 + 1));
            list.DrawIndexedInstanced(m.indices, 1, m.first_index, m.base_vertex, 0);
        }

        // The compute pass, then the copy to read back.
        list.ResourceBarrier(&[transition(
            &target,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
        list.SetComputeRootSignature(&compute_root);
        list.SetPipelineState(&compute);
        list.SetComputeRootDescriptorTable(0, gpu(6));
        list.Dispatch(WIDTH / scene::GROUP, HEIGHT / scene::GROUP, 1);
        list.ResourceBarrier(&[transition(
            &output,
            unordered,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.CopyBufferRegion(&readback, 0, &output, 0, frame_bytes);
        list.Close()?;
        queue.ExecuteCommandLists(&[Some(list.cast()?)]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        queue.Signal(&fence, 1)?;
        // A null event: returns once the fence has the value.
        fence.SetEventOnCompletion(1, HANDLE::default())?;
        device.GetDeviceRemovedReason()?;

        let mut at = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut at))?;
        let mut out = vec![0f32; (frame_bytes / 4) as usize];
        std::ptr::copy_nonoverlapping(at.cast::<f32>(), out.as_mut_ptr(), out.len());
        readback.Unmap(0, None);
        Ok(out)
    }
}
