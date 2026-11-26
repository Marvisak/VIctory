//! `cargo xtask conformance`: the acceptance suite (the conformance title,
//! `tests/fixtures/conformance`) in one command, on Windows or on a Mac.
//!
//! On Windows it is `cargo test -p victory-hle --test conformance`. On macOS
//! the title and the test are cross-compiled for Windows with `cargo xwin`
//! and run under CrossOver (Wine, with Apple's D3DMetal for D3D12) in a
//! bottle of their own, created on first use.
//!
//! `--window <seconds>` also shows the GPU frame, animated, in a window
//! that long, drawn at `--resolution` (1440p by default) with VIctory's
//! overlay (frame rate, frame times) in its top left corner.
//!
//! `cargo xtask demo` (`cargo demo`) shows only the GPU frame, until its
//! window is closed: to leave running, or to show. `--scale <n>` multiplies
//! the work each frame gives VIctory (draws, views, constants written into
//! title memory, submissions) to find where the translation layer slows.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

/// The Windows target everything is built for.
const TARGET: &str = "x86_64-pc-windows-msvc";
/// The CrossOver bottle the suite runs in.
const BOTTLE: &str = "VIctory";

/// The test that draws (and shows) the GPU frame.
const GPU_TEST: &str = "the_gpu_frame_matches_warp";

#[derive(clap::Args)]
pub struct Conformance {
    /// Also show the GPU frame, animated, in a window for this many seconds.
    #[arg(long, value_name = "SECONDS")]
    window: Option<u32>,
    #[command(flatten)]
    shown: Shown,
    /// Passed to the test binary: a test name filter, `--ignored`, ...
    #[arg(last = true)]
    test_args: Vec<String>,
}

#[derive(clap::Args)]
pub struct Demo {
    /// Stop after this many seconds instead of when the window is closed.
    #[arg(long, value_name = "SECONDS")]
    seconds: Option<u32>,
    #[command(flatten)]
    shown: Shown,
}

/// How the GPU frame is shown and built.
#[derive(clap::Args)]
struct Shown {
    /// The resolution the window's frames are drawn at: 720p, 1080p,
    /// 1440p, 4k (2160p), or <width>x<height> (multiples of 8).
    #[arg(long, default_value = "1440p", value_parser = resolution)]
    resolution: (u32, u32),
    /// Stress VIctory's translation layer: each step past 1 adds 64 draws
    /// to a frame, each with its constants written into title memory and
    /// its view made in the heap that frame, and one more submission
    /// (1: the checked frame's work).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=1024))]
    scale: u32,
    /// Leave VIctory's overlay (frame rate and frame times) hidden in the
    /// window; Alt+Z still shows it.
    #[arg(long)]
    no_overlay: bool,
    /// Build the test in the dev profile instead of release (much slower
    /// frames).
    #[arg(long)]
    debug: bool,
    /// Save the checked frames (and, after a timed window, the window's
    /// last) in this folder as PPM images.
    #[arg(long, value_name = "DIR")]
    dump: Option<PathBuf>,
}

pub fn conformance(root: &Path, args: Conformance) -> Result<()> {
    let window = args.window.map(|s| s.to_string());
    run(root, window.as_deref(), &args.shown, args.test_args)
}

pub fn demo(root: &Path, args: Demo) -> Result<()> {
    let window = args
        .seconds
        .map_or("until-closed".into(), |s| s.to_string());
    let test_args = vec![GPU_TEST.into(), "--exact".into()];
    run(root, Some(&window), &args.shown, test_args)
}

/// Runs the conformance test, showing the GPU frame in a window for
/// `window` (seconds, or `until-closed`) if asked.
fn run(root: &Path, window: Option<&str>, shown: &Shown, mut test_args: Vec<String>) -> Result<()> {
    let mut env: Vec<(&str, String)> = vec![];
    if let Some(window) = window {
        let (width, height) = shown.resolution;
        env.push(("VICTORY_CONFORMANCE_WINDOW", window.into()));
        env.push(("VICTORY_CONFORMANCE_SIZE", format!("{width}x{height}")));
        env.push(("VICTORY_CONFORMANCE_SCALE", shown.scale.to_string()));
        env.push((
            "VICTORY_OVERLAY",
            if shown.no_overlay { "0" } else { "1" }.into(),
        ));
    }
    if let Some(dir) = &shown.dump {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let dir = dir.canonicalize()?;
        // Wine sees the Mac's files on its Z: drive.
        let named = if cfg!(target_os = "macos") {
            wine_path(&dir)
        } else {
            dir.display().to_string()
        };
        env.push(("VICTORY_CONFORMANCE_DUMP", named));
    }
    if !test_args.iter().any(|a| a == "--nocapture") {
        test_args.push("--nocapture".into());
    }
    if cfg!(windows) {
        windows(root, shown.debug, &env, &test_args)
    } else if cfg!(target_os = "macos") {
        mac(root, shown.debug, &env, &test_args)
    } else {
        bail!("the conformance title runs on Windows, or on macOS under CrossOver")
    }
}

/// `720p`, `1080p`, `1440p`, `4k` (`2160p`) or `<width>x<height>`, each a
/// multiple of the GPU frame's 8-pixel compute groups.
fn resolution(s: &str) -> Result<(u32, u32), String> {
    let size = match s.to_ascii_lowercase().as_str() {
        "720p" | "hd" => (1280, 720),
        "1080p" | "fhd" => (1920, 1080),
        "1440p" | "qhd" => (2560, 1440),
        "4k" | "2160p" | "uhd" => (3840, 2160),
        other => other
            .split_once('x')
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .ok_or_else(|| format!("`{s}`: 720p, 1080p, 1440p, 4k, or <width>x<height>"))?,
    };
    match size {
        (w, h) if w > 0 && h > 0 && w % 8 == 0 && h % 8 == 0 && w <= 8192 && h <= 8192 => Ok(size),
        (w, h) => Err(format!(
            "{w}x{h}: width and height must be multiples of 8, at most 8192"
        )),
    }
}

/// On Windows: the test as it is, natively (it builds the title itself).
fn windows(root: &Path, debug: bool, env: &[(&str, String)], test_args: &[String]) -> Result<()> {
    let mut cargo = Command::new(cargo_bin());
    cargo
        .current_dir(root)
        .args(["test", "-p", "victory-hle", "--test", "conformance"]);
    if !debug {
        cargo.arg("--release");
    }
    cargo
        .arg("--")
        .args(test_args)
        .envs(env.iter().map(|(k, v)| (k, v)));
    let status = cargo.status().context("running cargo test")?;
    ensure!(status.success(), "the conformance suite failed");
    Ok(())
}

/// On macOS: cross-compiled, then run under CrossOver with D3DMetal.
fn mac(root: &Path, debug: bool, env: &[(&str, String)], test_args: &[String]) -> Result<()> {
    let crossover = crossover()?;
    xwin_available()?;

    // The title: a DLL the test loads through VIctory (Wine runs no cargo,
    // so it is built here and named by VICTORY_CONFORMANCE_DLL).
    println!("building the conformance title for {TARGET}");
    let title_dir = root.join("target/xwin-title");
    let mut cargo = Command::new(cargo_bin());
    cargo
        .current_dir(root)
        .args(["xwin", "build", "--target", TARGET, "--manifest-path"]);
    cargo.arg(root.join("tests/fixtures/conformance/Cargo.toml"));
    cargo.arg("--target-dir").arg(&title_dir);
    let status = cargo.status().context("running cargo xwin build")?;
    ensure!(status.success(), "building the conformance title failed");
    let dll = title_dir.join(TARGET).join("debug/conformance.dll");

    // The test binary. blake3's build script assembles with MSVC's ml64,
    // which cargo-xwin does not provide: CARGO_FEATURE_PURE makes it build
    // its portable code instead (it reads that variable, as cargo sets it
    // for its `pure` feature).
    println!("building the conformance test for {TARGET}");
    let mut cargo = Command::new(cargo_bin());
    cargo
        .current_dir(root)
        .args(["xwin", "test", "--no-run", "--target", TARGET]);
    cargo.args([
        "-p",
        "victory-hle",
        "--test",
        "conformance",
        "--message-format=json-render-diagnostics",
    ]);
    cargo.arg("--target-dir").arg(root.join("target/xwin"));
    if !debug {
        cargo.arg("--release");
    }
    cargo.env("CARGO_FEATURE_PURE", "1");
    let out = cargo
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("running cargo xwin test")?;
    ensure!(out.status.success(), "building the conformance test failed");
    let exe = test_executable(&String::from_utf8_lossy(&out.stdout))
        .context("cargo did not name the conformance test's executable")?;

    ensure_bottle(&crossover)?;
    println!(
        "running {} in the {BOTTLE} bottle (CrossOver, D3DMetal)",
        exe.display()
    );
    let mut wine = Command::new(crossover.join("wine"));
    wine.args(["--bottle", BOTTLE, "--wait-children", "--"])
        .arg(&exe)
        .args(test_args);
    wine.env("VICTORY_CONFORMANCE_DLL", wine_path(&dll))
        .env("CX_GRAPHICS_BACKEND", "d3dmetal")
        .env("WINEDEBUG", "-all")
        .envs(env.iter().map(|(k, v)| (k, v)));
    let status = wine.status().context("running wine")?;
    ensure!(status.success(), "the conformance suite failed");
    Ok(())
}

fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

/// CrossOver's `bin` folder (`VICTORY_CROSSOVER_BIN`, else the app in
/// /Applications or ~/Applications).
fn crossover() -> Result<PathBuf> {
    if let Some(bin) = std::env::var_os("VICTORY_CROSSOVER_BIN") {
        return Ok(bin.into());
    }
    let app = "Applications/CrossOver.app/Contents/SharedSupport/CrossOver/bin";
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    [PathBuf::from("/").join(app), home.join(app)]
        .into_iter()
        .find(|bin| bin.join("wine").exists())
        .context("CrossOver not found: install it, or set VICTORY_CROSSOVER_BIN to its bin folder")
}

fn xwin_available() -> Result<()> {
    let ok = Command::new(cargo_bin())
        .args(["xwin", "--version"])
        .output()
        .is_ok_and(|o| o.status.success());
    ensure!(
        ok,
        "cargo-xwin is needed to cross-compile for Windows: `cargo install --locked cargo-xwin` \
         (its first build downloads Microsoft's CRT and Windows SDK, under Microsoft's licence)"
    );
    Ok(())
}

/// Creates the bottle (Windows 10, 64-bit) unless it is there.
fn ensure_bottle(crossover: &Path) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let bottle = home
        .join("Library/Application Support/CrossOver/Bottles")
        .join(BOTTLE);
    if bottle.exists() {
        return Ok(());
    }
    println!("creating the CrossOver bottle {BOTTLE}");
    let status = Command::new(crossover.join("cxbottle"))
        .args(["--bottle", BOTTLE, "--create", "--template", "win10_64"])
        .args(["--description", "VIctory acceptance suite"])
        .status()
        .context("running cxbottle")?;
    ensure!(status.success(), "creating the CrossOver bottle failed");
    Ok(())
}

/// The conformance test's executable among cargo's JSON messages.
fn test_executable(messages: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter(|l| {
            l.contains(r#""reason":"compiler-artifact""#) && l.contains(r#""name":"conformance""#)
        })
        .find_map(|l| {
            let start = l.find(r#""executable":""#)? + r#""executable":""#.len();
            let end = start + l[start..].find('"')?;
            Some(PathBuf::from(&l[start..end]))
        })
}

/// A macOS path as Wine names it: on the Z: drive, with backslashes.
fn wine_path(path: &Path) -> String {
    format!("Z:{}", path.display().to_string().replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_test_executable_in_cargo_messages() {
        let messages = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"victory_hle"},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"kind":["test"],"name":"conformance"},"executable":"/w/target/xwin/x86_64-pc-windows-msvc/release/deps/conformance-0123.exe","fresh":true}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );
        assert_eq!(
            test_executable(messages),
            Some(PathBuf::from(
                "/w/target/xwin/x86_64-pc-windows-msvc/release/deps/conformance-0123.exe"
            ))
        );
        assert_eq!(test_executable(r#"{"reason":"build-finished"}"#), None);
    }

    #[test]
    fn resolutions_by_name_and_by_size() {
        assert_eq!(resolution("1080p"), Ok((1920, 1080)));
        assert_eq!(resolution("1440P"), Ok((2560, 1440)));
        assert_eq!(resolution("4k"), Ok((3840, 2160)));
        assert_eq!(resolution("2160p"), Ok((3840, 2160)));
        assert_eq!(resolution("1280x720"), Ok((1280, 720)));
        assert!(resolution("1366x768").is_err(), "not a multiple of 8");
        assert!(resolution("big").is_err());
    }

    #[test]
    fn wine_paths_are_on_the_z_drive() {
        assert_eq!(wine_path(Path::new("/a/b/c.dll")), r"Z:\a\b\c.dll");
    }
}
