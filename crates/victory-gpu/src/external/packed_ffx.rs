//! Bridge the PC FSR upscaler's color output to a console R9G9B9E5 target.
//! R9G9B9E5 is read-only on this D3D12 device. Passing an R32_UINT UAV
//! while declaring R9 to FidelityFX made it write one float channel into
//! the integer resource, which the title then decoded as noisy packed RGB.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::{host, transition};
use crate::object::Object;

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), UAV(u0)"
cbuffer C : register(b0) { uint width; uint height; uint inputPitch; uint outputPitch; float exposure; float highlightKnee; };
ByteAddressBuffer inputBytes : register(t0);
RWByteAddressBuffer outputBytes : register(u0);

uint pack_shared_exp(float3 color) {
    color = min(max(color, 0.0), 65408.0);
    float maximum = max(color.r, max(color.g, color.b));
    uint exponent = max(asuint(maximum) >> 23, 111u) - 111u;
    float scale = asfloat((151u - exponent) << 23);
    exponent += (uint)(round(maximum * scale) >= 512.0);
    scale = asfloat((151u - exponent) << 23);
    uint3 mantissa = (uint3)round(color * scale);
    return mantissa.r | (mantissa.g << 9) | (mantissa.b << 18) | (exponent << 27);
}

[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= width || id.y >= height) return;
    uint2 words = inputBytes.Load2(id.y * inputPitch + id.x * 8);
    float3 rgb = float3(
        f16tof32(words.x & 0xffffu),
        f16tof32(words.x >> 16),
        f16tof32(words.y & 0xffffu));
    float3 exposed = max(rgb * exposure, 0.0);
    // Compress bright values by their luminance, keeping their colour ratio.
    // At low luminance this approaches the original exposure multiplication.
    if (highlightKnee > 0.0) {
        float luminance = dot(exposed, float3(0.2126, 0.7152, 0.0722));
        exposed *= rcp(1.0 + highlightKnee * luminance);
    }
    outputBytes.Store(id.y * outputPitch + id.x * 4, pack_shared_exp(exposed));
}
"#;

struct Packer {
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Packer {}
// SAFETY: as above.
unsafe impl Sync for Packer {}

fn packer() -> Option<&'static Packer> {
    static PACKER: OnceLock<Option<Packer>> = OnceLock::new();
    PACKER.get_or_init(|| match compile() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "FidelityFX packed-output converter unavailable");
            None
        }
    }).as_ref()
}

/// Optional display exposure for the packed FSR bridge. It changes only the
/// final color handed back to the title, leaving the FSR history untouched.
fn display_exposure() -> f32 {
    static VALUE: OnceLock<f32> = OnceLock::new();
    *VALUE.get_or_init(|| {
        let value = std::env::var("VICTORY_FFX_EXPOSURE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && (0.25..=64.0).contains(v))
            .unwrap_or(1.0);
        if value != 1.0 {
            tracing::info!(value, "FidelityFX bridge display exposure");
        }
        value
    })
}

/// Optional luminance-based shoulder in the packed FSR bridge. Zero keeps
/// the previous linear exposure; one is a useful diagnostic starting point.
fn highlight_knee() -> f32 {
    static VALUE: OnceLock<f32> = OnceLock::new();
    *VALUE.get_or_init(|| {
        let value = std::env::var("VICTORY_FFX_HIGHLIGHT_KNEE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && (0.0..=4.0).contains(v))
            .unwrap_or(0.0);
        if value != 0.0 {
            tracing::info!(value, "FidelityFX bridge highlight knee");
        }
        value
    })
}

fn compile() -> windows::core::Result<Packer> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: static source and string names, with owned out pointers.
    let made = unsafe {
        D3DCompile(SOURCE.as_ptr().cast(), SOURCE.len(), s!("ffx_pack"), None, None,
            s!("main"), s!("cs_5_1"), 0, 0, &mut code, Some(&mut errors))
    };
    if let Err(e) = made {
        if let Some(errors) = errors {
            // SAFETY: the blob owns these bytes.
            let text = unsafe { std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize()) };
            tracing::warn!(message = %String::from_utf8_lossy(text), "FidelityFX packed-output shader compilation failed");
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the shader blob is live through both calls and embeds its root signature.
    unsafe {
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pipeline = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
            ..Default::default()
        })?;
        Ok(Packer { root, pipeline })
    }
}

/// A float color UAV for FSR's output, held by the title list until it runs.
pub(super) fn temporary(list: *const Object, width: u32, height: u32) -> Option<ID3D12Resource> {
    packer()?;
    let (host, list) = (host()?, crate::list::host_list(list)?);
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: width.into(), Height: height,
        DepthOrArraySize: 1, MipLevels: 1,
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
    let mut texture = None;
    // SAFETY: valid UAV texture description and owned output pointer.
    unsafe {
        host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS, None, &mut texture).ok()?;
    }
    let texture: ID3D12Resource = texture?;
    crate::host::name(&texture, || format!("FidelityFX float output {width}x{height}"));
    list.keep(&texture);
    Some(texture)
}

fn subresource(resource: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: the resource stays live through the list execution.
        pResource: unsafe { std::mem::transmute_copy(resource) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    }
}

fn footprint(resource: &ID3D12Resource, at: D3D12_PLACED_SUBRESOURCE_FOOTPRINT) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: the resource stays live through the list execution.
        pResource: unsafe { std::mem::transmute_copy(resource) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: at },
    }
}

/// Record texture -> row-major float bytes -> packed integer bytes -> title
/// output. The title output starts and ends in UAV state, as its list tracker
/// expects. Returns false before recording if resources cannot be prepared.
pub(super) fn finish(list: *const Object, float: &ID3D12Resource, packed: &ID3D12Resource) -> bool {
    let (Some(host), Some(held), Some(packer)) = (host(), crate::list::host_list(list), packer()) else { return false };
    // SAFETY: both live textures have one subresource.
    let (a, b) = unsafe { (float.GetDesc(), packed.GetDesc()) };
    if a.Format != DXGI_FORMAT_R16G16B16A16_FLOAT || b.Format != DXGI_FORMAT_R32_UINT
        || a.Width != b.Width || a.Height != b.Height || a.MipLevels != 1 || b.MipLevels != 1
    {
        return false;
    }
    let mut fa = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 1];
    let mut fb = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 1];
    let (mut ba, mut bb) = (0u64, 0u64);
    // SAFETY: one footprint output per one-subresource texture.
    unsafe {
        host.device.GetCopyableFootprints(&a, 0, 1, 0, Some(fa.as_mut_ptr()), None, None, Some(&mut ba));
        host.device.GetCopyableFootprints(&b, 0, 1, 0, Some(fb.as_mut_ptr()), None, None, Some(&mut bb));
    }
    let Some(input) = host.default_buffer(ba) else { return false };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bb.max(1), Height: 1, DepthOrArraySize: 1, MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut output = None;
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() };
    // SAFETY: valid UAV buffer and owned output pointer.
    if unsafe { host.device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc,
        D3D12_RESOURCE_STATE_COMMON, None, &mut output) }.is_err() { return false }
    let Some(output): Option<ID3D12Resource> = output else { return false };
    held.keep(&input);
    held.keep(&output);
    let constants = [a.Width as u32, a.Height, fa[0].Footprint.RowPitch,
        fb[0].Footprint.RowPitch, display_exposure().to_bits(), highlight_knee().to_bits()];
    let recorded = super::record(list, |commands| {
        // SAFETY: FSR left the float output in UAV; the title output is UAV
        // by the bridge's earlier ensure_state. Buffers start in their stated
        // creation states. Every resource is held until execution finishes.
        unsafe {
            commands.ResourceBarrier(&[transition(float, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            commands.CopyTextureRegion(&footprint(&input, fa[0]), 0, 0, 0, &subresource(float), None);
            commands.ResourceBarrier(&[
                transition(&input, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE),
                transition(&output, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
            ]);
            commands.SetComputeRootSignature(&packer.root);
            commands.SetPipelineState(&packer.pipeline);
            commands.SetComputeRoot32BitConstants(0, constants.len() as u32, constants.as_ptr().cast(), 0);
            commands.SetComputeRootShaderResourceView(1, input.GetGPUVirtualAddress());
            commands.SetComputeRootUnorderedAccessView(2, output.GetGPUVirtualAddress());
            commands.Dispatch(constants[0].div_ceil(8), constants[1].div_ceil(8), 1);
            commands.ResourceBarrier(&[
                transition(&output, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE),
                transition(packed, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_DEST),
            ]);
            commands.CopyTextureRegion(&subresource(packed), 0, 0, 0, &footprint(&output, fb[0]), None);
            commands.ResourceBarrier(&[transition(packed, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
        }
    });
    if recorded.is_some() {
        tracing::info!(width = constants[0], height = constants[1], input_bytes = ba, output_bytes = bb,
            "FidelityFX float output packed into R9G9B9E5 words");
    }
    recorded.is_some()
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    use crate::host::{host, transition};

    #[test]
    fn converter_shader_and_pipeline_compile_on_host() {
        if host().is_some() {
            assert!(super::packer().is_some());
        }
    }

    fn buffer(
        device: &ID3D12Device,
        heap_type: D3D12_HEAP_TYPE,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> ID3D12Resource {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 256,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: flags,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES { Type: heap_type, ..Default::default() };
        let mut made = None;
        // SAFETY: a valid row-major buffer and an owned output pointer.
        unsafe {
            device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made)
                .expect("test buffer");
        }
        made.expect("test buffer output")
    }

    fn unpack(word: u32) -> [f32; 3] {
        let scale = 2.0_f32.powi((word >> 27) as i32 - 24);
        [word & 511, (word >> 9) & 511, (word >> 18) & 511].map(|v| v as f32 * scale)
    }

    #[test]
    fn highlight_knee_compresses_bright_gpu_pixels_and_preserves_dim_ones() {
        let Some(host) = host() else { return };
        let packer = super::packer().expect("packed FFX shader and root signature");
        let (input, data) = host.upload_buffer(256).expect("upload buffer");
        // Two RGB16F pixels: (1, 0.5, 0.25) and (1/32, 1/64, 1/128).
        // SAFETY: the upload buffer is mapped for at least four dwords.
        unsafe {
            for (i, word) in [0x3800_3c00, 0x0000_3400, 0x2400_2800, 0x0000_2000]
                .into_iter().enumerate()
            {
                data.cast::<u32>().add(i).write_volatile(word);
            }
        }
        let ordinary = buffer(&host.device, D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON);
        let compressed = buffer(&host.device, D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON);
        let readback = buffer(&host.device, D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST);
        let list = host.new_list().expect("test command list");
        for (output, knee) in [(&ordinary, 0.0_f32), (&compressed, 1.0_f32)] {
            let constants = [2_u32, 1, 16, 8, 1.0_f32.to_bits(), knee.to_bits()];
            // SAFETY: live resources, open list, and the embedded six-constant root signature.
            unsafe {
                list.list.ResourceBarrier(&[transition(output, D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
                list.list.SetComputeRootSignature(&packer.root);
                list.list.SetPipelineState(&packer.pipeline);
                list.list.SetComputeRoot32BitConstants(0, constants.len() as u32,
                    constants.as_ptr().cast(), 0);
                list.list.SetComputeRootShaderResourceView(1, input.GetGPUVirtualAddress());
                list.list.SetComputeRootUnorderedAccessView(2, output.GetGPUVirtualAddress());
                list.list.Dispatch(1, 1, 1);
                list.list.ResourceBarrier(&[transition(output, D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                list.list.CopyBufferRegion(&readback, if knee == 0.0 { 0 } else { 64 }, output, 0, 8);
            }
        }
        list.close();
        host.execute(&[&list]);
        let mut data = std::ptr::null_mut();
        // SAFETY: execution has completed and the readback mapping covers both results.
        let (normal_bright, normal_dim, soft_bright, soft_dim) = unsafe {
            readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 72 }), Some(&mut data))
                .expect("map readback");
            let words = data.cast::<u32>();
            let values = (
                unpack(words.read()), unpack(words.add(1).read()),
                unpack(words.add(16).read()), unpack(words.add(17).read()),
            );
            readback.Unmap(0, Some(&D3D12_RANGE::default()));
            values
        };
        assert!((normal_bright[0] - 1.0).abs() < 0.01, "old knee=0 path is linear");
        assert!(soft_bright[0] < normal_bright[0] * 0.7 && soft_bright[0] > normal_bright[0] * 0.6,
            "bright pixel should get a soft shoulder: {normal_bright:?} -> {soft_bright:?}");
        assert!((soft_bright[1] / soft_bright[0] - 0.5).abs() < 0.02,
            "highlight compression should preserve the RGB ratio");
        assert!(soft_dim[0] > normal_dim[0] * 0.95,
            "dim pixel should remain near the linear value: {normal_dim:?} -> {soft_dim:?}");
    }
}
