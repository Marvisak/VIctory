//! Our own compute shaders: HLSL in `shaders/`, 5.1 programs with their root
//! signature inside. A build that runs on Windows compiles them
//! (`build.rs`), so the binary needs no HLSL compiler where it runs and a
//! shader that does not compile fails the build. A build elsewhere (a cross
//! build) leaves them to the system's compiler at first use. Wine's cannot
//! compile them, so under Wine a failure says what to do.

use std::borrow::Cow;

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{PCSTR, s};

/// One of our shaders.
pub(crate) struct Shader {
    /// Its file's stem in `shaders/`.
    name: &'static str,
    source: &'static str,
    /// Compiled by the build.
    prebuilt: Option<&'static [u8]>,
}

macro_rules! shader {
    ($name:literal) => {
        Shader {
            name: $name,
            source: include_str!(concat!("shaders/", $name, ".hlsl")),
            #[cfg(victory_hlsl_prebuilt)]
            prebuilt: Some(include_bytes!(concat!(
                env!("OUT_DIR"),
                "/",
                $name,
                ".dxbc"
            ))),
            #[cfg(not(victory_hlsl_prebuilt))]
            prebuilt: None,
        }
    };
}

/// Indirect execution's argument clamp (`indirect`).
pub(crate) const CLAMP: Shader = shader!("clamp");
/// 16-bit depth to the host's depth plane and back (`twin`).
pub(crate) const DEPTH: Shader = shader!("depth");

/// `shader`'s root signature and compute pipeline on `device`.
pub(crate) fn pipeline(
    device: &ID3D12Device,
    shader: &Shader,
) -> windows::core::Result<(ID3D12RootSignature, ID3D12PipelineState)> {
    let made = bytecode(shader).and_then(|code| {
        // SAFETY: a compiled shader with its root signature inside, alive
        // through the calls.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &code)?;
            let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })?;
            Ok((root, pso))
        }
    });
    if made.is_err() && shader.prebuilt.is_none() && on_wine() {
        tracing::warn!(
            shader = shader.name,
            "Wine's HLSL compiler cannot make our shaders: build VIctory on Windows, or \
             install Microsoft's d3dcompiler_47 in the Wine prefix (winetricks or \
             protontricks: d3dcompiler_47)"
        );
    }
    made
}

fn bytecode(shader: &Shader) -> windows::core::Result<Cow<'static, [u8]>> {
    match shader.prebuilt {
        Some(code) => Ok(Cow::Borrowed(code)),
        None => compile(shader).map(Cow::Owned),
    }
}

/// `shader` compiled by the system's compiler, as `build.rs` compiles it.
fn compile(shader: &Shader) -> windows::core::Result<Vec<u8>> {
    let name = format!("{}\0", shader.name);
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            shader.source.as_ptr().cast(),
            shader.source.len(),
            PCSTR(name.as_ptr()),
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
            tracing::warn!(
                shader = shader.name,
                "{}",
                String::from_utf8_lossy(bytes(&errors))
            );
        }
        return Err(e);
    }
    code.map(|c| bytes(&c).to_vec())
        .ok_or_else(windows::core::Error::empty)
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: the blob's own bytes, alive while it is.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
fn on_wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: plain lookups in ntdll, always loaded.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of our shaders makes a pipeline.
    #[test]
    fn our_shaders_make_pipelines() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        for shader in [&CLAMP, &DEPTH] {
            pipeline(&host.device, shader).unwrap();
        }
    }

    /// The build's programs are byte for byte the ones the system's compiler
    /// makes at first use, where a cross build leaves them to it.
    #[test]
    fn prebuilt_shaders_are_what_the_system_compiler_makes() {
        for shader in [&CLAMP, &DEPTH] {
            if let Some(code) = shader.prebuilt {
                assert_eq!(compile(shader).unwrap(), code, "{}", shader.name);
            }
        }
    }
}
