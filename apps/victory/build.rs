//! D3D12 Agility SDK opt-in.
//!
//! Windows 10's inbox D3D12 runtime only exposes shader model 6.5, while the
//! translated title shaders need 6.6 (heap-direct descriptors). Putting an
//! Agility SDK `D3D12Core.dll` next to the built executable lets the host
//! loader pick it up, but only when the executable exports the loader's
//! opt-in symbols. Those exports are build-time, so this build script enables
//! them exactly when a runtime is present next to the output binary:
//!
//! ```text
//! target/<profile>/D3D12/D3D12Core.dll
//! ```
//!
//! Stage that file with `cargo xtask agility`, then rebuild. Without it the
//! binary keeps using the system runtime, unchanged. A cargo warning is
//! emitted either way so a missing runtime is not mistaken for a GPU limit.
//!
//! Set `VICTORY_AGILITY_VERSION` to announce a newer SDK than the default
//! (for example `620` after staging a 1.620.x `D3D12Core.dll`). The staged
//! DLL must be at least as new as the announced version.

use std::path::PathBuf;

/// Default Agility SDK version the exports announce, matching `1.614.1`.
const DEFAULT_SDK_VERSION: u32 = 614;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=VICTORY_AGILITY_VERSION");
    println!("cargo:rustc-check-cfg=cfg(victory_agility)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let sdk_version = agility_sdk_version();
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
    let runtime = profile_dir.join("D3D12").join("D3D12Core.dll");
    println!("cargo:rerun-if-changed={}", runtime.display());
    if !runtime.is_file() {
        println!(
            "cargo:warning=no app-local D3D12 runtime at {}; the host stays at \
             whatever shader model the system D3D12 reports (SM 6.5 on Windows 10). \
             Run `cargo xtask agility` then rebuild to opt into Agility SDK {sdk_version}",
            runtime.display()
        );
        return;
    }
    // The binary reads this through `env!` when the `victory_agility` cfg is on.
    println!("cargo:rustc-env=VICTORY_AGILITY_VERSION={sdk_version}");
    println!("cargo:rustc-cfg=victory_agility");
    // DATA: these are statics, not functions; MSVC needs the flag for dumpbin
    // to list them as data exports the D3D12 loader can read.
    // `-bins` (not plain `rustc-link-arg`) keeps `/EXPORT:…,DATA` off benches
    // and other package targets that do not link these statics (LNK2001).
    println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
    println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
    println!(
        "cargo:warning=opting the host D3D12 runtime into Agility SDK {sdk_version} ({})",
        runtime.display()
    );
}

fn agility_sdk_version() -> u32 {
    match std::env::var("VICTORY_AGILITY_VERSION") {
        Ok(raw) => match raw.parse::<u32>() {
            Ok(v) if v > 0 => v,
            Ok(_) => {
                println!(
                    "cargo:warning=VICTORY_AGILITY_VERSION must be a positive integer; \
                     using default {DEFAULT_SDK_VERSION}"
                );
                DEFAULT_SDK_VERSION
            }
            Err(_) => {
                println!(
                    "cargo:warning=VICTORY_AGILITY_VERSION={raw:?} is not a u32; \
                     using default {DEFAULT_SDK_VERSION}"
                );
                DEFAULT_SDK_VERSION
            }
        },
        Err(_) => DEFAULT_SDK_VERSION,
    }
}
