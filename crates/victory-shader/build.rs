//! `VICTORY_SHADER_SOURCE_HASH`: what the translator is built from (this
//! crate's sources, manifest and build script, the workspace's lock file
//! and the compiler's version), for caches of translations
//! (`victory_shader::SOURCE_HASH`).

use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};

fn main() {
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let mut files = vec![
        dir.join("Cargo.toml"),
        dir.join("build.rs"),
        dir.join("../../Cargo.lock"),
    ];
    sources(&dir.join("src"), &mut files);
    files.sort();
    // SipHash with fixed keys: stable for one compiler, whose version is
    // hashed too.
    let mut h = DefaultHasher::new();
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let version = std::process::Command::new(rustc).arg("-vV").output();
    h.write(&version.map(|o| o.stdout).unwrap_or_default());
    for f in &files {
        let name = f
            .strip_prefix(&dir)
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        h.write(name.as_bytes());
        h.write(&std::fs::read(f).unwrap_or_default());
    }
    for watched in ["src", "Cargo.toml", "build.rs", "../../Cargo.lock"] {
        println!("cargo:rerun-if-changed={watched}");
    }
    println!(
        "cargo:rustc-env=VICTORY_SHADER_SOURCE_HASH={:016x}",
        h.finish()
    );

    app_local_agility_runtime();
}

/// Opt the test binaries into an app-local Agility SDK runtime, the same opt-in
/// `apps/victory` has: without it Windows 10 reports shader model 6.5 and every
/// SM 6.6 test is skipped. A no-op unless the runtime is already staged.
fn app_local_agility_runtime() {
    // Declared on every build, including the ones that take an early return below: a `cfg`
    // name has to be known to rustc even when nothing sets it.
    println!("cargo:rustc-check-cfg=cfg(agility_runtime)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // `OUT_DIR` is `target/<profile>/build/<pkg>-<hash>/out`; the profile directory is three
    // levels up. The application looks for the runtime in `target/<profile>/D3D12/`, so this
    // crate reads it from the same place instead of inventing a second convention.
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let profile = out
        .ancestors()
        .nth(3)
        .expect("a profile directory above OUT_DIR")
        .to_path_buf();
    let runtime = profile.join("D3D12").join("D3D12Core.dll");
    if !runtime.is_file() {
        // No app-local runtime: leave the in-box one alone, exactly as a build without
        // the opt-in would. Do not watch the missing file: `rerun-if-changed` on a path
        // that does not exist reruns this script on every build.
        return;
    }
    // The runtime is staged: rerun if it is replaced or removed.
    println!("cargo:rerun-if-changed={}", runtime.display());

    // The loader resolves `D3D12SDKPath` next to the running module, and test binaries run
    // from `deps`, so mirror the runtime there before the tests need it.
    let deps = profile.join("deps").join("D3D12");
    let _ = std::fs::create_dir_all(&deps);
    let _ = std::fs::copy(&runtime, deps.join("D3D12Core.dll"));

    // `src/agility.rs` defines the two symbols the Agility SDK loader looks for in the host
    // module. They are `DATA` symbols and the loader needs both: exporting the version alone
    // leaves it on the in-box runtime.
    //
    // The MSVC linker takes `/EXPORT:` options straight from here. GNU hosts do not: `ld`
    // reads `.drectve` sections from MSVC objects only, and treats `/EXPORT:...` as a file
    // name, so the exports go through a `.def` file instead. `cargo:rustc-link-arg` applies
    // to this package's test and bench binaries, which is exactly the set that needs it.
    match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => {
            println!("cargo:rustc-link-arg=/EXPORT:D3D12SDKVersion,DATA");
            println!("cargo:rustc-link-arg=/EXPORT:D3D12SDKPath,DATA");
        }
        _ => {
            let def = out.join("agility-exports.def");
            let _ = std::fs::write(
                &def,
                "EXPORTS
    D3D12SDKVersion DATA
    D3D12SDKPath DATA
",
            );
            println!("cargo:rustc-link-arg=-Wl,{}", def.display());
        }
    }
    println!("cargo:rustc-cfg=agility_runtime");
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else {
            out.push(path);
        }
    }
}
