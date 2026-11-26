//! The conformance title (tests/fixtures/conformance) run through VIctory's
//! loader and runtime, as `victory run` runs a title. The title is ours,
//! written from the public platform documentation only: its checks are what
//! those pages say each call does. No game data and no SDK.
//!
//! `cargo test -p victory-hle --test conformance -- --nocapture` builds the
//! title, runs it and prints its TAP report. `KNOWN` lists the checks VIctory
//! is known to answer differently from the documentation, each with why:
//! they are reported, not failed, and one that starts passing fails the test
//! so the list stays true. `-- --ignored` runs the streaming workload.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

use victory_hle::{HleResolver, HostPolicy};
use victory_loader::{UnimplementedPolicy, prepare};

// The GPU frame drawn natively on WARP (its own folder: a file here is a
// test of its own).
#[path = "conformance/reference.rs"]
mod reference;
// The GPU frame's scene, shared with the title so both draw the same (the
// title alone uses some of it).
#[allow(dead_code)]
#[path = "../../../tests/fixtures/conformance/src/scene.rs"]
mod scene;

/// Checks VIctory is known to answer differently from the public reference,
/// each with why: reported, not failed. A check here that passes fails the
/// test, so the list stays true.
const KNOWN: &[(&str, &str)] = &[
    (
        "xasync: an async call with no queue and no process queue is E_NO_TASK_QUEUE",
        "VIctory runs the call anyway; XTaskQueueSetCurrentProcessTaskQueue's \
         page says APIs needing a queue then fail with E_NO_TASK_QUEUE",
    ),
    (
        "xsystem: a retail console id is the documented zero id",
        "VIctory returns 38 digits ending in 1; the reference gives \
         00000000.00000000.00000000.00000000.00 for retail and PC",
    ),
    (
        "dstorage: a queue filled to half its capacity is not submitted on its own",
        "VIctory submits once half the capacity is enqueued; the overview \
         says an Enqueue that fills more than half submits",
    ),
    (
        "xuser: XUserFindUserByLocalId of no user is HRESULT_FROM_WIN32(ERROR_NOT_FOUND)",
        "VIctory returns E_GAMEUSER_USER_NOT_FOUND (0x89245104); the \
         XUserFindUserByLocalId page gives HRESULT_FROM_WIN32(ERROR_NOT_FOUND)",
    ),
    (
        "xuser: an invalid gamertag component is E_INVALIDARG",
        "VIctory answers any component other than ModernSuffix with the \
         gamertag; the XUserGetGamertag page gives E_INVALIDARG",
    ),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Builds the fixture into its own target dir (the outer cargo holds the
/// lock on the main one while tests run). `VICTORY_CONFORMANCE_DLL` names
/// one built already instead: for a test binary run where cargo is not, such
/// as a cross-compiled one under Wine.
fn build_fixture() -> PathBuf {
    if let Some(dll) = std::env::var_os("VICTORY_CONFORMANCE_DLL") {
        return PathBuf::from(dll);
    }
    let manifest = workspace_root().join("tests/fixtures/conformance/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("conformance");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect("running cargo");
    assert!(status.success(), "building the conformance title failed");
    target.join("debug").join("conformance.dll")
}

/// The title's report exports.
struct Exports {
    run: usize,
    bench: usize,
    gpu: usize,
}

/// Calls into the title one at a time: its runtime state, and the reports
/// it hands out, are global.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs one of the title's report exports (`conformance_run` or
/// `conformance_bench`).
fn call(name: &str) -> String {
    let e = exports();
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let at = if name == "conformance_bench" {
        e.bench
    } else {
        e.run
    };
    // SAFETY: the exports' signature in the fixture; the report stays valid
    // until the next call, which the lock holds off.
    unsafe {
        let f: extern "system" fn(*mut usize) -> *const u8 = std::mem::transmute(at);
        let mut len = 0;
        let report = f(&mut len);
        String::from_utf8_lossy(std::slice::from_raw_parts(report, len)).into_owned()
    }
}

/// The title loaded and bound once per test process (the runtime's state
/// is global: one title at a time).
fn exports() -> &'static Exports {
    static EXPORTS: std::sync::OnceLock<Exports> = std::sync::OnceLock::new();
    EXPORTS.get_or_init(|| {
        let dll = build_fixture();
        let original = std::fs::read(&dll).unwrap();
        let prepared = prepare(&original).unwrap();
        let images = tempfile::tempdir().unwrap().keep();
        let image = victory_loader::load(&prepared, &images).unwrap();
        victory_hle::runtime::set_main_image(image.base());
        victory_hle::runtime::set_image_path(image.base(), &dll);
        // Persistent local storage (game saves live beside it), as `victory
        // run` gives a title.
        let data = tempfile::tempdir().unwrap().keep();
        // `victory run` makes the folder before the title starts.
        std::fs::create_dir_all(data.join("storage")).unwrap();
        victory_hle::runtime::set_title(victory_hle::runtime::Title {
            id: 0x1234_5678,
            storage: data.join("storage"),
            storage_bytes: 64 << 20,
            ..Default::default()
        });
        // The console's drives: the package at G: (holding the file the
        // title checks), temporary storage at T:.
        let package = data.join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("conformance.txt"),
            b"conformance: public documentation only
",
        )
        .unwrap();
        let temp = data.join("temp");
        std::fs::create_dir_all(&temp).unwrap();
        victory_hle::modules::files::mount_at('G', &package);
        victory_hle::modules::files::mount_at('T', &temp);
        let resolver = HleResolver::new(HostPolicy::default());
        let report = image
            .bind_imports(&resolver, UnimplementedPolicy::ReturnZero)
            .unwrap();
        assert!(
            report.missing.is_empty(),
            "imports VIctory does not serve: {:?}",
            report.missing.iter().map(|m| &m.symbol).collect::<Vec<_>>()
        );
        // SAFETY: its imports are bound; it depends on no title DLL.
        unsafe { image.attach() }.unwrap();
        let export = |name| image.export(name).unwrap_or_else(|| panic!("{name}"));
        let exports = Exports {
            run: export("conformance_run"),
            bench: export("conformance_bench"),
            gpu: export("conformance_gpu"),
        };
        // Stays loaded for the rest of the process.
        std::mem::forget(image);
        exports
    })
}

/// What `conformance_gpu` takes and hands back (the fixture's `GpuRun`).
#[repr(C)]
struct GpuRun {
    reference: *const f32,
    reference_len: usize,
    window_seconds: u32,
    window_width: u32,
    window_height: u32,
    window_scale: u32,
    image: *const f32,
    image_len: usize,
    window_image: *const f32,
    window_image_len: usize,
}

/// Runs the title's GPU frame against `reference`: its TAP report and the
/// frame it read back.
fn call_gpu(
    reference: Option<&[f32]>,
    window_seconds: u32,
    (window_width, window_height): (u32, u32),
    window_scale: u32,
) -> (String, Vec<f32>, Vec<f32>) {
    let e = exports();
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut run = GpuRun {
        reference: reference.map_or(std::ptr::null(), <[f32]>::as_ptr),
        reference_len: reference.map_or(0, <[f32]>::len),
        window_seconds,
        window_width,
        window_height,
        window_scale,
        image: std::ptr::null(),
        image_len: 0,
        window_image: std::ptr::null(),
        window_image_len: 0,
    };
    // SAFETY: the export's signature in the fixture; the report and image
    // stay valid until the next call, which the lock holds off.
    unsafe {
        let f: extern "system" fn(*mut GpuRun, *mut usize) -> *const u8 =
            std::mem::transmute(e.gpu);
        let mut len = 0;
        let report = f(&mut run, &mut len);
        let report = String::from_utf8_lossy(std::slice::from_raw_parts(report, len)).into_owned();
        let image = if run.image.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(run.image, run.image_len).to_vec()
        };
        let window_image = if run.window_image.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(run.window_image, run.window_image_len).to_vec()
        };
        (report, image, window_image)
    }
}

/// The GPU frame: the title draws a scene
/// through VIctory's console D3D12 with hand-assembled GFX10 shaders, and
/// checks its frame against the same scene drawn natively on WARP.
/// `VICTORY_CONFORMANCE_WINDOW=<seconds>` keeps presenting it, animated,
/// in a window that long (`until-closed`: until the window is closed, which
/// ends the process), drawn at `VICTORY_CONFORMANCE_SIZE=<width>x<height>`
/// (2560x1440 unless set; multiples of 8), with `VICTORY_CONFORMANCE_SCALE`
/// times the work a frame (1 unless set; up to 1024). `VICTORY_CONFORMANCE_DUMP=<folder>`
/// saves the checked frames there as PPM images (and the
/// window's last, `window.ppm`, after a timed window).
#[test]
fn the_gpu_frame_matches_warp() {
    let reference = match reference::render() {
        Ok(r) => Some(r),
        Err(e) => {
            println!("no WARP reference: {e}");
            None
        }
    };
    let window = match std::env::var("VICTORY_CONFORMANCE_WINDOW").ok().as_deref() {
        Some("until-closed") => u32::MAX,
        Some(seconds) => seconds.parse().unwrap_or(0),
        None => 0,
    };
    if window > 0 {
        end_when_the_window_closes();
    }
    let size = window_size();
    let scale = window_scale();
    let (tap, image, window_image) = call_gpu(reference.as_deref(), window, size, scale);
    println!("{tap}");
    if let Some(dir) = std::env::var_os("VICTORY_CONFORMANCE_DUMP") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let checked = (scene::WIDTH, scene::HEIGHT);
        save_ppm(&dir.join("victory.ppm"), &image, checked);
        if let Some(r) = &reference {
            save_ppm(&dir.join("warp.ppm"), r, checked);
        }
        if !window_image.is_empty() {
            save_ppm(&dir.join("window.ppm"), &window_image, size);
        }
    }
    if let Err(why) = accept(&tap, &[]) {
        panic!("{why}\n{tap}");
    }
}

/// An RGBA float frame of `width` x `height` as a binary PPM (RGB8).
fn save_ppm(path: &Path, image: &[f32], (width, height): (u32, u32)) {
    let mut out = format!("P6\n{width} {height}\n255\n").into_bytes();
    for p in image.chunks(4) {
        out.extend(
            p[..3]
                .iter()
                .map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8),
        );
    }
    std::fs::write(path, out).unwrap();
}

/// The streaming workload: throughput, printed. Run with `cargo test
/// --release -p victory-hle --test conformance -- --ignored --nocapture`.
#[test]
#[ignore = "a benchmark: run on demand"]
fn workloads() {
    println!("{}", call("conformance_bench"));
}

#[test]
fn the_conformance_title_runs_through_victory() {
    let tap = call("conformance_run");
    println!("{tap}");
    match accept(&tap, KNOWN) {
        Ok(known) => {
            for (name, why) in known {
                println!("known divergence: {name}: {why}");
            }
        }
        Err(why) => panic!("{why}\n{tap}"),
    }
}

/// Checks a TAP report against `known` divergences: every result the plan
/// announces is there (numbered 1 to N, in order), each known divergence
/// is in it exactly once and fails, and nothing else fails. Returns the
/// known divergences seen.
fn accept<'a>(tap: &str, known: &'a [(&str, &str)]) -> Result<Vec<(&'a str, &'a str)>, String> {
    let plan: usize = tap
        .lines()
        .find_map(|l| l.strip_prefix("1.."))
        .and_then(|n| n.trim().parse().ok())
        .ok_or("no TAP plan")?;
    // (number, passed, name), the name without a skip's reason.
    let results: Vec<(usize, bool, &str)> = tap
        .lines()
        .filter_map(|l| {
            let (passed, rest) = match (l.strip_prefix("not ok "), l.strip_prefix("ok ")) {
                (Some(rest), _) => (false, rest),
                (None, Some(rest)) => (true, rest),
                (None, None) => return None,
            };
            let (n, name) = rest.split_once(" - ")?;
            let name = name.split_once(" # SKIP").map_or(name, |(name, _)| name);
            Some((n.parse().ok()?, passed, name))
        })
        .collect();
    if plan == 0 {
        return Err("no checks ran".into());
    }
    let numbers: Vec<usize> = results.iter().map(|r| r.0).collect();
    if numbers != (1..=plan).collect::<Vec<_>>() {
        return Err(format!(
            "the report has {} results numbered {numbers:?}, its plan 1..{plan}",
            results.len()
        ));
    }
    let mut seen = Vec::new();
    for &(name, why) in known {
        match results.iter().filter(|r| r.2 == name).collect::<Vec<_>>()[..] {
            [] => return Err(format!("a known divergence's check is missing: {name}")),
            [(_, true, _)] => {
                return Err(format!(
                    "a known divergence now passes (take it off KNOWN): {name}"
                ));
            }
            [_] => seen.push((name, why)),
            _ => return Err(format!("a known divergence's check is there twice: {name}")),
        }
    }
    let unexpected: Vec<&str> = results
        .iter()
        .filter(|r| !r.1 && !known.iter().any(|(k, _)| *k == r.2))
        .map(|r| r.2)
        .collect();
    if !unexpected.is_empty() {
        return Err(format!("checks that failed: {unexpected:#?}"));
    }
    Ok(seen)
}

/// What [`accept`] refuses, on made-up reports (review of #2057): a known
/// divergence whose check is gone, also with the plan lowered to match, or
/// a report cut short of its plan; as before, a known divergence that
/// passes and an unknown failure.
#[test]
fn reports_missing_or_cut_short_checks_are_refused() {
    let known = &[("k1", "why 1"), ("k2", "why 2")];
    let report =
        |lines: &[&str]| format!("TAP version 13\n1..{}\n{}\n", lines.len(), lines.join("\n"));
    let numbered = |lines: &[(&str, bool)]| -> Vec<String> {
        lines
            .iter()
            .enumerate()
            .map(|(i, (name, ok))| {
                format!("{}ok {} - {name}", if *ok { "" } else { "not " }, i + 1)
            })
            .collect()
    };
    let all = numbered(&[("a", true), ("k1", false), ("k2", false)]);
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    assert_eq!(accept(&report(&all), known).map(|k| k.len()), Ok(2));
    let skipped = report(&[
        "ok 1 - a # SKIP no device",
        "not ok 2 - k1",
        "not ok 3 - k2",
    ]);
    assert!(accept(&skipped, known).is_ok(), "a skipped check counts");
    let gone = numbered(&[("a", true), ("k1", false)]);
    let gone: Vec<&str> = gone.iter().map(String::as_str).collect();
    assert!(
        accept(&report(&gone), known)
            .unwrap_err()
            .contains("missing: k2")
    );
    let cut = format!("TAP version 13\n1..3\n{}\n", all[0]);
    assert!(accept(&cut, known).unwrap_err().contains("plan 1..3"));
    let fixed = numbered(&[("a", true), ("k1", true), ("k2", false)]);
    let fixed: Vec<&str> = fixed.iter().map(String::as_str).collect();
    assert!(
        accept(&report(&fixed), known)
            .unwrap_err()
            .contains("now passes")
    );
    let new = numbered(&[("a", false), ("k1", false), ("k2", false)]);
    let new: Vec<&str> = new.iter().map(String::as_str).collect();
    assert!(accept(&report(&new), known).unwrap_err().contains("failed"));
    let twice = numbered(&[("k1", false), ("k1", false), ("k2", false)]);
    let twice: Vec<&str> = twice.iter().map(String::as_str).collect();
    assert!(
        accept(&report(&twice), known)
            .unwrap_err()
            .contains("twice")
    );
    assert!(
        accept("TAP version 13\n1..0\n", known).is_err(),
        "no checks"
    );
}

/// The size the window's frames are drawn at: `VICTORY_CONFORMANCE_SIZE`
/// (`<width>x<height>`, each a multiple of the compute pass's 8-pixel
/// groups), 2560x1440 unless set.
fn window_size() -> (u32, u32) {
    let Ok(asked) = std::env::var("VICTORY_CONFORMANCE_SIZE") else {
        return (2560, 1440);
    };
    let size = asked
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
    match size {
        Some((w, h)) if w > 0 && h > 0 && w % scene::GROUP == 0 && h % scene::GROUP == 0 => (w, h),
        _ => panic!(
            "VICTORY_CONFORMANCE_SIZE={asked}: <width>x<height>, multiples of {}",
            scene::GROUP
        ),
    }
}

/// Ends the process once VIctory's window (class `VIctory`, in this
/// process) has been shown and is gone: closing it destroys it, and the
/// title would otherwise go on presenting to no window.
fn end_when_the_window_closes() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetWindowThreadProcessId};
    fn ours_is_open() -> bool {
        let class: Vec<u16> = "VIctory\0".encode_utf16().collect();
        let mut after = std::ptr::null_mut();
        loop {
            // SAFETY: a null-terminated class name; null parent and window
            // name search every top-level window.
            let hwnd = unsafe {
                FindWindowExW(
                    std::ptr::null_mut(),
                    after,
                    class.as_ptr(),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return false;
            }
            let mut process = 0;
            // SAFETY: a window handle FindWindowExW just returned.
            unsafe { GetWindowThreadProcessId(hwnd, &mut process) };
            if process == std::process::id() {
                return true;
            }
            after = hwnd;
        }
    }
    std::thread::spawn(|| {
        let mut seen = false;
        loop {
            let open = ours_is_open();
            if seen && !open {
                println!("the window was closed");
                std::process::exit(0);
            }
            seen |= open;
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
}

/// The window's stress scale: `VICTORY_CONFORMANCE_SCALE` (1 to 1024), 1
/// unless set. Each step adds 64 draws to a frame, with as many views made
/// and constants written, and a submission.
fn window_scale() -> u32 {
    let Ok(asked) = std::env::var("VICTORY_CONFORMANCE_SCALE") else {
        return 1;
    };
    match asked.trim().parse() {
        Ok(scale @ 1..=1024) => scale,
        _ => panic!("VICTORY_CONFORMANCE_SCALE={asked}: 1 to 1024"),
    }
}
