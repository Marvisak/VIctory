//! Agility SDK exports, so Windows 10 gets shader model 6.6 (feature
//! `agility-sdk`; off by default).
//!
//! Windows 10's in-box Direct3D 12 runtime refuses root signatures with
//! directly indexed descriptor heaps (flags 0xc01): every host pipeline is
//! refused. `d3d12.dll` loads an app-local `D3D12Core.dll` when the
//! executable exports `D3D12SDKVersion` and `D3D12SDKPath`; `build.rs`
//! exports them when the feature is on.
//!
//! The runtime does not fall back to the in-box one when the folder is
//! missing: device creation fails (0x887E0003, `D3D12_ERROR_INVALID_REDIST`).
//! Commands that use Direct3D 12 check for the folder first ([`check`]).

/// Agility SDK version the runtime must match (1.620.x).
#[cfg(all(windows, feature = "agility-sdk"))]
#[allow(non_upper_case_globals)]
#[used]
#[unsafe(no_mangle)]
pub static D3D12SDKVersion: u32 = 620;

/// Folder, relative to the executable, that holds `D3D12Core.dll`.
#[cfg(all(windows, feature = "agility-sdk"))]
#[allow(non_upper_case_globals)]
#[used]
#[unsafe(no_mangle)]
pub static D3D12SDKPath: &[u8; 9] = b".\\D3D12\\\0";

/// With the feature: an error naming what to copy where when
/// `D3D12\D3D12Core.dll` is not beside the executable.
pub(crate) fn check() -> anyhow::Result<()> {
    if !cfg!(all(windows, feature = "agility-sdk")) {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let Some(dir) = exe.parent() else {
        return Ok(());
    };
    let core = dir.join("D3D12").join("D3D12Core.dll");
    anyhow::ensure!(
        core.is_file(),
        "this build asks for the Direct3D 12 Agility SDK 1.620 runtime (feature agility-sdk), and {} is missing: \
         copy build\\native\\bin\\x64\\D3D12Core.dll from the Microsoft.Direct3D.D3D12 1.620.x NuGet package \
         there, or build without the feature",
        core.display()
    );
    Ok(())
}
