//! Xbox Series shaders to host shaders.
//!
//! An Xbox Series shader is a standard DXBC container ([`dxbc`]). Its `DXIL`
//! part ([`dxil`]) may carry Xbox additions around the LLVM bitcode: an
//! `XDAT` table before it indexing blobs after it, among them `xbop`
//! (compiler options) and the zlib-compressed GFX10 code. The bitcode may use
//! console-only intrinsics, so the GFX10 code is translated in general.

pub mod dxbc;
pub mod dxil;
pub mod emit;
pub mod gfx10;
pub mod io;
pub mod ir;
pub mod native;
pub mod range;
pub mod resource;
pub mod structure;

pub use dxbc::{Container, FourCC, Part};
pub use dxil::{DxilProgram, ShaderKind, XboxExtras};
pub use native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, native_programs,
};

/// A hash of what this translator is built from: its sources, manifest and
/// build script, the workspace's lock file and the compiler's version. Caches
/// of translations key on it, so a changed translator never serves an old
/// translation.
pub const SOURCE_HASH: &str = env!("VICTORY_SHADER_SOURCE_HASH");

/// Whether a reachable native instruction traverses a ray-tracing BVH.
/// This inspects the original GFX10 program, so it also works for shaders
/// whose translated DXIL came from the on-disk cache.
pub fn uses_ray_traversal(container: &[u8]) -> Result<bool, ShaderError> {
    let container = Container::parse(container)?;
    let dxil = container
        .part(FourCC::DXIL)
        .or(container.part(FourCC::ACCC))
        .ok_or(ShaderError::Malformed("no DXIL part"))?;
    let native = native_programs(dxil.data)?
        .into_iter()
        .find(|program| matches!(program.stage, HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms))
        .ok_or(ShaderError::Malformed("no translated native program"))?;
    Ok(gfx10::decode(&native.patched_code())?
        .iter()
        .any(|instruction| instruction.name().is_some_and(|name| name.starts_with("image_bvh"))))
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaderError {
    #[error("not a DXBC container")]
    NotDxbc,
    #[error("malformed shader: {0}")]
    Malformed(&'static str),
    #[error("GFX10 code at {offset:#x}: {reason}")]
    Isa { offset: usize, reason: &'static str },
    /// Valid code the translator cannot handle yet: an opcode name or a construct.
    #[error("GFX10 code at {offset:#x}: {what} is not supported")]
    Unsupported { offset: usize, what: &'static str },
}
