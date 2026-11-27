//! `cargo xtask wine-test`: the Windows build's tests on a Linux host,
//! cross-compiled with clang-cl/lld-link against an `xwin` splat and run
//! under Wine in a prefix of our own (`target/wine-prefix`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

const TARGET: &str = "x86_64-pc-windows-msvc";

pub struct Options {
    /// The MSVC CRT and Windows SDK, as `xwin splat` lays them out.
    pub xwin: Option<PathBuf>,
    /// A folder holding vkd3d-proton's x64 `d3d12.dll` and `d3d12core.dll`,
    /// put in the prefix in place of Wine's own D3D12.
    pub vkd3d_proton: Option<PathBuf>,
    /// A folder holding DXVK's x64 `dxgi.dll`, put in the prefix in place
    /// of Wine's own DXGI.
    pub dxvk: Option<PathBuf>,
    /// Arguments for `cargo test`; the whole workspace but xtask without.
    pub args: Vec<OsString>,
}

pub fn run(root: &Path, o: Options) -> Result<()> {
    let xwin = match o.xwin {
        Some(dir) => dir,
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".xwin"),
    };
    // The cargo calls below run from the repository root; anchor a relative
    // path to the invocation directory first. `absolute` keeps symlinks.
    let xwin =
        std::path::absolute(&xwin).with_context(|| format!("resolving {}", xwin.display()))?;
    ensure!(
        xwin.join("crt").is_dir() && xwin.join("sdk").is_dir(),
        "no xwin splat at {} (make one with `xwin --accept-license splat --output {0}`, \
         which accepts Microsoft's licence for the CRT and SDK)",
        xwin.display()
    );
    let target_dir = root.join("target");
    let prefix = target_dir.join("wine-prefix");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    // cc splits *FLAGS like a shell when CC_SHELL_ESCAPED_FLAGS is set;
    // quote each include so a path with spaces stays one argument.
    let includes = [
        xwin.join("crt/include"),
        xwin.join("sdk/include/ucrt"),
        xwin.join("sdk/include/um"),
        xwin.join("sdk/include/shared"),
    ]
    .map(|dir| shell_quote(&format!("/imsvc{}", dir.display())))
    .join(" ");
    let cflags = format!("--target={TARGET} -Wno-unused-command-line-argument {includes}");
    // Cargo splits RUSTFLAGS on spaces inside paths, so encode the
    // separators. Every cargo call below passes --target, so these flags
    // never reach host build scripts or proc macros.
    let rustflags = [
        xwin.join("crt/lib/x86_64"),
        xwin.join("sdk/lib/um/x86_64"),
        xwin.join("sdk/lib/ucrt/x86_64"),
    ]
    .map(|dir| format!("-Lnative={}", dir.display()))
    .join("\x1f");
    let env: Vec<(&str, OsString)> = vec![
        ("CC_x86_64_pc_windows_msvc", "clang-cl".into()),
        ("CXX_x86_64_pc_windows_msvc", "clang-cl".into()),
        ("AR_x86_64_pc_windows_msvc", "llvm-lib".into()),
        ("CFLAGS_x86_64_pc_windows_msvc", cflags.clone().into()),
        ("CXXFLAGS_x86_64_pc_windows_msvc", cflags.into()),
        ("CC_SHELL_ESCAPED_FLAGS", "1".into()),
        (
            "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
            "lld-link".into(),
        ),
        ("CARGO_ENCODED_RUSTFLAGS", rustflags.into()),
        ("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER", "wine".into()),
        ("WINEPREFIX", prefix.clone().into()),
    ];
    let command = |program: &OsString| {
        let mut c = Command::new(program);
        c.current_dir(root).envs(env.iter().map(|(k, v)| (k, v)));
        c
    };

    // Wine makes the prefix on first use; native DLLs go into it after that.
    let system32 = prefix.join("drive_c/windows/system32");
    if !system32.is_dir() {
        let status = command(&"wineboot".into())
            .arg("-i")
            .status()
            .context("running wineboot (is Wine installed?)")?;
        ensure!(status.success(), "wineboot failed");
    }
    // A crashing test must die, not wait for a debugger dialog forever.
    let reg_add = |args: &[&str]| -> Result<()> {
        let status = command(&"wine".into())
            .arg("reg")
            .arg("add")
            .args(args)
            .status()
            .context("running wine reg (is Wine installed?)")?;
        ensure!(status.success(), "wine reg add failed");
        Ok(())
    };
    reg_add(&[
        r"HKLM\Software\Microsoft\Windows NT\CurrentVersion\AeDebug",
        "/v",
        "Auto",
        "/t",
        "REG_SZ",
        "/d",
        "1",
        "/f",
    ])?;
    // WineDbg would still show its own dialog; off prints a backtrace.
    reg_add(&[
        r"HKCU\Software\Wine\WineDbg",
        "/v",
        "ShowCrashDialog",
        "/t",
        "REG_DWORD",
        "/d",
        "0",
        "/f",
    ])?;

    let vkd3d = o.vkd3d_proton.is_some();
    let mut overrides = Vec::new();
    if let Some(dir) = o.vkd3d_proton {
        copy_dlls(&dir, &system32, &["d3d12.dll", "d3d12core.dll"])?;
        overrides.extend(["d3d12", "d3d12core"]);
    }
    if let Some(dir) = o.dxvk {
        copy_dlls(&dir, &system32, &["dxgi.dll"])?;
        overrides.push("dxgi");
    }

    let mut test = command(&cargo);
    test.args(["test", "--target", TARGET]).env(
        "WINEDEBUG",
        std::env::var_os("WINEDEBUG").unwrap_or_else(|| "-all".into()),
    );
    if !overrides.is_empty() {
        test.env("WINEDLLOVERRIDES", format!("{}=n", overrides.join(",")));
        if vkd3d {
            // vkd3d-proton writes its pipeline cache to the working directory
            // (the crate's, in tests), which the asset policy refuses.
            test.env(
                "VKD3D_SHADER_CACHE_PATH",
                wine_path(&target_dir.join("vkd3d-proton-cache")),
            );
            std::fs::create_dir_all(target_dir.join("vkd3d-proton-cache"))?;
        }
    }
    if o.args.is_empty() {
        // xtask is a host tool (it runs git); it is not tested under Wine.
        test.args(["--workspace", "--exclude", "xtask", "--no-fail-fast"]);
        // The loader's two end-to-end tests (victory-loader's
        // tests/load_probe.rs) build their fixture by starting cargo from
        // inside the test process, which Wine cannot do. The prebuilt-fixture
        // hook (VICTORY_HLE_PROBE) is in PR #3270, not in this branch, so
        // skip them until it is available. The skips are the test binaries'
        // options, so they go after `--`; cargo itself has no `--skip`.
        if std::env::var_os("VICTORY_HLE_PROBE").is_none() {
            eprintln!(
                "skipping victory-loader's hle-probe tests: they need the \
                 VICTORY_HLE_PROBE hook from PR #3270 (a fixture built on \
                 the host); pass -- <cargo-test-args> to override"
            );
            test.args([
                "--",
                "--skip",
                "loads_binds_and_calls_probe",
                "--skip",
                "maps_a_title_dll_the_title_loads_itself",
            ]);
        }
    } else {
        test.args(&o.args);
    }
    let status = test.status().context("running cargo")?;
    ensure!(status.success(), "tests failed under Wine");
    Ok(())
}

/// Copies native DLLs into the prefix, replacing copies from earlier runs.
fn copy_dlls(dir: &Path, system32: &Path, dlls: &[&str]) -> Result<()> {
    for dll in dlls {
        // A copy keeps the source's permissions (read-only in Proton),
        // so the one from an earlier run is removed, not overwritten.
        let (from, to) = (dir.join(dll), system32.join(dll));
        let _ = std::fs::remove_file(&to);
        std::fs::copy(&from, &to)
            .with_context(|| format!("copying {} into the prefix", from.display()))?;
    }
    Ok(())
}

/// `path` as Wine's Z: drive (the host's root) names it; a relative path is
/// made absolute first, since Z: names the host's root.
fn wine_path(path: &Path) -> String {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    format!("Z:{}", path.display()).replace('/', "\\")
}

/// `arg` as one shell word for cc's `CC_SHELL_ESCAPED_FLAGS` parsing.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}
