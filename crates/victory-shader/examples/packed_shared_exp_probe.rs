//! Validate a console pixel shader's R9G9B9E5 packed-output variant.
//! Usage: cargo run -p victory-shader --features host --example
//! packed_shared_exp_probe -- FILE.dxbc [FILE.dxbc ...]

#[cfg(all(windows, feature = "host"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use victory_shader::emit::{self, Options};
    use victory_shader::{Container, FourCC};
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    let files: Vec<_> = std::env::args().skip(1).collect();
    if files.is_empty() {
        return Err("pass one or more console DXBC files".into());
    }
    let host = emit::host::Host::new(false).map_err(|e| format!("D3D12 host: {e}"))?;
    let mut validator =
        emit::validator::Validator::load(emit::validator::Validator::default_path().as_deref())?;
    for file in files {
        let bytes = std::fs::read(&file)?;
        let ps = emit::translate_with(
            &bytes,
            Options {
                view_marks: true,
                packed_shared_exp_target0: true,
                ..Options::default()
            },
        )?;
        let mut dxil = ps.dxil.clone();
        validator.validate(&mut dxil)?;
        let c = Container::parse(&ps.dxil)?;
        let sig = c.part(FourCC::OSG1).ok_or("no output signature")?.data;
        let count = u32::from_le_bytes(sig[0..4].try_into()?) as usize;
        let base = u32::from_le_bytes(sig[4..8].try_into()?) as usize;
        let mut packed = false;
        let mut depth = false;
        for i in 0..count {
            let at = base + i * 32;
            let kind = u32::from_le_bytes(sig[at + 12..at + 16].try_into()?);
            let comp = u32::from_le_bytes(sig[at + 16..at + 20].try_into()?);
            let mask = sig[at + 24];
            if kind == 64 {
                packed = comp == 1 && mask == 1;
            }
            if kind == 65 {
                depth = true;
            }
        }
        if !packed {
            return Err(format!("{file}: Target0 is not one uint channel").into());
        }
        let vs = emit::host::stub_vs(&ps)?;
        let root = host.root_signature(&vs, &ps)?;
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: the description borrows this COM pointer; `root` stays
            // alive through pipeline creation and the description never releases it.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: vs.dxil.as_ptr().cast(),
                BytecodeLength: vs.dxil.len(),
            },
            PS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: ps.dxil.as_ptr().cast(),
                BytecodeLength: ps.dxil.len(),
            },
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        desc.RTVFormats[0] = DXGI_FORMAT_R32_UINT;
        desc.BlendState.RenderTarget[0].RenderTargetWriteMask =
            D3D12_COLOR_WRITE_ENABLE_RED.0 as u8;
        if depth {
            desc.DSVFormat = DXGI_FORMAT_D32_FLOAT;
            desc.DepthStencilState.DepthEnable = true.into();
            desc.DepthStencilState.DepthWriteMask = D3D12_DEPTH_WRITE_MASK_ALL;
            desc.DepthStencilState.DepthFunc = D3D12_COMPARISON_FUNC_ALWAYS;
        }
        // SAFETY: the root signature and shader bytecode referenced by `desc`
        // remain alive for this synchronous D3D12 pipeline creation call.
        let _pso: ID3D12PipelineState = unsafe { host.device.CreateGraphicsPipelineState(&desc) }?;
        println!(
            "{file}: packed uint Target0, depth={depth}, DXIL validated, R32_UINT PSO created"
        );
    }
    Ok(())
}

#[cfg(not(all(windows, feature = "host")))]
fn main() {
    eprintln!("this D3D12 probe needs Windows and the host feature");
}
