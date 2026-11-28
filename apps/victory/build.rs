//! Exports the Agility SDK symbols from `victory.exe` with the feature
//! `agility-sdk` (see `src/sdkpath.rs`): Rust does not export statics from
//! a binary, so the MSVC linker is asked to.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows_msvc && std::env::var_os("CARGO_FEATURE_AGILITY_SDK").is_some() {
        println!("cargo:rustc-link-arg-bin=victory=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bin=victory=/EXPORT:D3D12SDKPath,DATA");
    }
}
