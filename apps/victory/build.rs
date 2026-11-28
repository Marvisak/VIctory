//! D3D12 Agility SDK opt-in.
//!
//! Windows 10's inbox D3D12 runtime stops at shader model 6.5 and refuses the
//! heap-directly-indexed root-signature flags the translated shaders need. An
//! Agility SDK `D3D12Core.dll` next to the built executable is used only when
//! the executable exports the loader's opt-in symbols. Those exports are
//! build-time, so enable them exactly when the runtime is present at:
//!
//! ```text
//! target/<profile>/D3D12/D3D12Core.dll
//! ```
//!
//! Without that file the binary keeps using the system runtime, unchanged.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(victory_agility)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    // `target/<profile>/build/victory-<hash>/out`: the binary lands three
    // directories above `OUT_DIR`.
    let Some(profile_dir) = PathBuf::from(&out_dir)
        .ancestors()
        .nth(3)
        .map(PathBuf::from)
    else {
        return;
    };
    let runtime_dir = profile_dir.join("D3D12");
    // A copied DLL can have an older timestamp than the last build. The
    // directory changes when the DLL is added or removed.
    println!("cargo:rerun-if-changed={}", runtime_dir.display());
    let runtime = runtime_dir.join("D3D12Core.dll");
    if !runtime.is_file() {
        return;
    }
    let version = match sdk_version(&runtime) {
        Ok(version) => version,
        Err(error) => {
            println!(
                "cargo:warning=not opting into the D3D12 Agility SDK: {}: {error}",
                runtime.display()
            );
            return;
        }
    };
    println!("cargo:rustc-cfg=victory_agility");
    println!("cargo:rustc-env=VICTORY_AGILITY_SDK_VERSION={version}");
    // `bin=victory` keeps the exports off integration-test binaries; `,DATA`
    // tells the MSVC linker these are variables, not functions.
    println!("cargo:rustc-link-arg-bin=victory=/EXPORT:D3D12SDKVersion,DATA");
    println!("cargo:rustc-link-arg-bin=victory=/EXPORT:D3D12SDKPath,DATA");
    println!(
        "cargo:warning=opting the host D3D12 runtime into Agility SDK {version} ({})",
        runtime.display()
    );
}

/// Reads the `u32` `D3D12SDKVersion` export from a `D3D12Core.dll` image on
/// disk. The executable must announce exactly this value, so do not hard-code
/// a version that a staged runtime can silently disagree with.
fn sdk_version(path: &Path) -> Result<u32, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() < 0x40 || &bytes[..2] != b"MZ" {
        return Err("not a PE image".into());
    }
    let pe = u32_at(&bytes, 0x3c)? as usize;
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("missing PE signature".into());
    }
    let coff = pe + 4;
    if u16_at(&bytes, coff)? != 0x8664 {
        return Err("not an x64 DLL".into());
    }
    let section_count = usize::from(u16_at(&bytes, coff + 2)?);
    let optional_size = usize::from(u16_at(&bytes, coff + 16)?);
    let optional = coff + 20;
    if u16_at(&bytes, optional)? != 0x20b {
        return Err("not a PE32+ image".into());
    }
    // PE32+ data directory 0 is export table RVA/size.
    let export_rva = u32_at(&bytes, optional + 112)?;
    if export_rva == 0 {
        return Err("no export table".into());
    }
    let sections_at = optional + optional_size;
    let mut sections = Vec::with_capacity(section_count);
    for index in 0..section_count {
        let at = sections_at + index * 40;
        if bytes.get(at..at + 40).is_none() {
            return Err("truncated section table".into());
        }
        sections.push(Section {
            virtual_size: u32_at(&bytes, at + 8)?,
            virtual_address: u32_at(&bytes, at + 12)?,
            raw_size: u32_at(&bytes, at + 16)?,
            raw_at: u32_at(&bytes, at + 20)?,
        });
    }
    let rva_offset = |rva: u32| -> Result<usize, String> {
        sections
            .iter()
            .find_map(|s| s.file_offset(rva))
            .ok_or_else(|| format!("RVA {rva:#x} is not in the image"))
    };
    let export = rva_offset(export_rva)?;
    let name_count = u32_at(&bytes, export + 24)? as usize;
    let functions = rva_offset(u32_at(&bytes, export + 28)?)?;
    let names = rva_offset(u32_at(&bytes, export + 32)?)?;
    let ordinals = rva_offset(u32_at(&bytes, export + 36)?)?;
    for index in 0..name_count {
        let name_rva = u32_at(&bytes, names + index * 4)?;
        let name_at = rva_offset(name_rva)?;
        if cstr(&bytes, name_at)? != b"D3D12SDKVersion" {
            continue;
        }
        let ordinal = usize::from(u16_at(&bytes, ordinals + index * 2)?);
        let export_rva = u32_at(&bytes, functions + ordinal * 4)?;
        let version = u32_at(&bytes, rva_offset(export_rva)?)?;
        if version == 0 {
            return Err("D3D12SDKVersion is zero".into());
        }
        return Ok(version);
    }
    Err("no D3D12SDKVersion export".into())
}

struct Section {
    virtual_size: u32,
    virtual_address: u32,
    raw_size: u32,
    raw_at: u32,
}

impl Section {
    fn file_offset(&self, rva: u32) -> Option<usize> {
        let span = self.virtual_size.max(self.raw_size);
        let offset = rva.checked_sub(self.virtual_address)?;
        if offset >= span || offset >= self.raw_size {
            return None;
        }
        Some((self.raw_at + offset) as usize)
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, String> {
    let bytes = bytes
        .get(at..at + 2)
        .ok_or_else(|| format!("truncated PE at {at:#x}"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, String> {
    let bytes = bytes
        .get(at..at + 4)
        .ok_or_else(|| format!("truncated PE at {at:#x}"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn cstr(bytes: &[u8], at: usize) -> Result<&[u8], String> {
    let rest = bytes
        .get(at..)
        .ok_or_else(|| format!("truncated PE at {at:#x}"))?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| format!("unterminated string at {at:#x}"))?;
    Ok(&rest[..end])
}
