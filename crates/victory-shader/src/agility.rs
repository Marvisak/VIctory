//! Agility SDK opt-in.
//!
//! Windows 10's in-box D3D12 runtime tops out at shader model 6.5, and this crate emits
//! SM 6.6 (dynamic resources, `WaveSize`, compute derivatives). The Agility SDK loader in
//! `d3d12.dll` switches to a redistributable `D3D12Core.dll` when the host module exports
//! the two symbols below; `D3D12SDKPath` names the folder holding it, relative to the
//! executable, and `D3D12SDKVersion` must match the `D3D12_SDK_VERSION` in the SDK's
//! `d3d12.h` (620 for package 1.620.2).
//!
//! `build.rs` sets `cfg(agility_runtime)` once the runtime is staged next to the test
//! binaries, and everything here hangs off that: exporting the symbols also makes the
//! loader refuse to create a device when the DLL is missing (`0x887e0003`), so a build
//! without the staged runtime stays opted out instead of exporting and failing.
//!
//! `build.rs` publishes the symbols with `cargo:rustc-link-arg` (MSVC `/EXPORT:`, a `.def`
//! file for GNU hosts). The module is compiled for every target of the crate, `guard_runs`
//! bench included, so no target is asked to export a symbol it does not define; a library
//! archive member is only pulled into a binary that references it, which keeps the
//! selectors out of `apps/victory`, where a second copy of them would collide.

#[cfg(agility_runtime)]
#[allow(dead_code)]
#[unsafe(no_mangle)]
pub static D3D12SDKVersion: u32 = 620;

/// A `const char*`, not a char array: the loader dereferences this symbol.
#[cfg(agility_runtime)]
#[allow(dead_code)]
#[unsafe(no_mangle)]
pub static D3D12SDKPath: &[u8; 9] = b".\\D3D12\\\0";
