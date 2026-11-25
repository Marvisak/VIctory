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
/// lock on the main one while tests run).
fn build_fixture() -> PathBuf {
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

/// The title loaded and bound once per test process (the runtime's state
/// is global: one title at a time); `call(name)` runs one of its report
/// exports.
fn call(name: &str) -> String {
    static EXPORTS: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let &(run, bench) = EXPORTS.get_or_init(|| {
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
        let exports = (
            image.export("conformance_run").expect("conformance_run"),
            image
                .export("conformance_bench")
                .expect("conformance_bench"),
        );
        // Stays loaded for the rest of the process.
        std::mem::forget(image);
        exports
    });
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let at = if name == "conformance_bench" {
        bench
    } else {
        run
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
