//! Tests share one host device (`super::host`), and D3D12 hands every
//! caller on an adapter the same device, so a test that removes it removes
//! it for every test after it in the binary. Without care the removal then
//! surfaces wherever the next test calls the device, or, through
//! `super::fail` (which ends the process when nothing set `on_failure`),
//! as an exit with no test named.
//!
//! So in tests: a failure report panics in the test that made it; the first
//! test found holding a removed device is named, with the reason, on the
//! process's standard error (past the test's captured output, so it shows
//! even if the process ends early); after it, `host()` is `None` to every
//! other test, which skips the host GPU as on a machine without one; and a
//! panic after the removal says what happened before it.

use std::io::Write as _;
use std::sync::{Mutex, OnceLock};

use super::Host;

/// The test the removal was first found in, and the removal reason.
struct Found {
    test: String,
    reason: String,
}

static FOUND: OnceLock<Found> = OnceLock::new();

thread_local! {
    /// This thread panics with a failure report, which names the removal.
    static REPORTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn test_name() -> String {
    std::thread::current()
        .name()
        .unwrap_or("an unnamed thread")
        .to_owned()
}

/// The removal reason of a removed device.
fn removed(host: &Host) -> Option<String> {
    // SAFETY: a plain query.
    let e = unsafe { host.device.GetDeviceRemovedReason() }.err()?;
    let mut reason = format!("{} {}", e.code(), e.message());
    if super::dred_on() {
        reason.push_str(&host.removal_data(&[]));
    }
    Some(reason)
}

/// Notes that this test holds a removed device, if no test did before.
fn found(reason: String) -> &'static Found {
    FOUND.get_or_init(|| {
        let test = test_name();
        let _ = writeln!(
            std::io::stderr(),
            "victory-gpu tests: `{test}` found the host device removed ({reason}); \
             GPU tests after it skip the host GPU"
        );
        Found { test, reason }
    })
}

/// Whether this test may use the host: not once another test found it
/// removed. Never panics, as runtime code called through the console's
/// tables (which cannot unwind) asks too.
pub(super) fn usable(host: &Host) -> bool {
    let found = match FOUND.get() {
        Some(found) => found,
        None => match removed(host) {
            None => return true,
            Some(reason) => found(reason),
        },
    };
    let me = test_name();
    if found.test == me {
        // The test that removed it fails on its own calls.
        return true;
    }
    static SKIPPED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut skipped = SKIPPED.lock().unwrap_or_else(|e| e.into_inner());
    if !skipped.contains(&me) {
        let _ = writeln!(
            std::io::stderr(),
            "victory-gpu tests: `{me}` skips the host GPU: the device was found removed in `{}` ({})",
            found.test,
            found.reason
        );
        skipped.push(me);
    }
    false
}

/// A host GPU failure report in a test: the failure of the test that made
/// it, not the end of the process. Returns on threads of ours (pipeline
/// workers), which tests do not join, after writing the report where it
/// shows; the caller then ends the process.
pub(super) fn failed(text: &str) {
    if let Some(host) = super::created_yet()
        && let Some(reason) = removed(host)
    {
        found(reason);
    }
    if std::thread::current()
        .name()
        .is_some_and(|n| n.starts_with("victory-"))
    {
        let _ = writeln!(std::io::stderr(), "victory-gpu tests: {text}");
        return;
    }
    REPORTING.set(true);
    panic!("{text}");
}

/// Called once, when the host is made: a panic in a test holding a removed
/// device says so after its own message.
pub(super) fn watch() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        if REPORTING.replace(false) {
            return;
        }
        let found = match (FOUND.get(), super::created_yet()) {
            (Some(found), _) => found,
            (None, Some(host)) => match removed(host) {
                Some(reason) => found(reason),
                None => return,
            },
            (None, None) => return,
        };
        if found.test == test_name() {
            eprintln!(
                "note: the host device is removed ({}); this test was the first to find it so, \
                 and it was removed here, in a test before this one, or in one running \
                 alongside it",
                found.reason
            );
        } else {
            eprintln!(
                "note: the host device was found removed in `{}` ({}) before this panic; \
                 this failure may only follow from that",
                found.test, found.reason
            );
        }
    }));
}

mod tests {
    use super::super::host;
    use windows::Win32::Graphics::Direct3D12::ID3D12Device5;
    use windows::core::Interface;

    /// Set only for the runs of this binary that
    /// `a_test_that_removes_the_device_fails_alone` starts, to how the first
    /// test below removes the device; the tests below do nothing without it.
    const CHILD: &str = "VICTORY_TEST_REMOVES_DEVICE";
    const FIRST: &str = "host::removal::tests::child_1_removes_the_device";
    const SECOND: &str = "host::removal::tests::child_2_runs_after_the_removal";
    const THIRD: &str = "host::removal::tests::child_3_runs_after_the_removal";

    /// Removes the device, then: with `submission`, submits a list, as a
    /// test whose commands fault the GPU does; with `call`, makes one, as a
    /// test whose call into the device removed it does; with `unnoticed`,
    /// nothing, as a test whose removal shows only later.
    #[test]
    fn child_1_removes_the_device() {
        let Some(how) = std::env::var_os(CHILD) else {
            return;
        };
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let device: ID3D12Device5 = host.device.cast().unwrap();
        // SAFETY: a plain call; the device is this process's alone.
        unsafe { device.RemoveDevice() };
        match how.to_str() {
            Some("submission") => {
                list.close();
                host.execute(&[&list]);
            }
            Some("call") => drop(host.new_list().unwrap()),
            _ => {}
        }
    }

    /// Any GPU test after it.
    fn after() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        list.close();
        host.execute(&[&list]);
    }

    #[test]
    fn child_2_runs_after_the_removal() {
        after();
    }

    #[test]
    fn child_3_runs_after_the_removal() {
        after();
    }

    /// Run in processes of their own (D3D12 gives the tests of a process
    /// one device), each way of removing the device fails the one test that
    /// found it removed, saying why, the tests after it skip the host GPU,
    /// and the process ends as a run with a failed test, not an abort.
    ///
    /// It starts this test binary again (three times), so it is left out of
    /// a plain `cargo test`; run it with `--ignored`.
    #[test]
    #[ignore = "starts this test binary again; run with --ignored"]
    fn a_test_that_removes_the_device_fails_alone() {
        if std::env::var_os(CHILD).is_some() || host().is_none() {
            return;
        }
        for (how, culprit, reason) in [
            ("submission", FIRST, "device removed"),
            ("call", FIRST, "the host device is removed"),
            ("unnoticed", SECOND, "the host device is removed"),
        ] {
            let run = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "--test-threads=1", FIRST, SECOND, THIRD])
                .env(CHILD, how)
                .output()
                .unwrap();
            let out = format!(
                "{}\n{}",
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );
            assert_eq!(
                run.status.code(),
                Some(101),
                "{how}: a run with a failed test:\n{out}"
            );
            for test in [FIRST, SECOND, THIRD] {
                let result = if test == culprit { "FAILED" } else { "ok" };
                assert!(
                    out.contains(&format!("test {test} ... {result}")),
                    "{how}:\n{out}"
                );
            }
            assert!(
                out.contains(&format!("failures:\n    {culprit}\n")),
                "{how}: only it:\n{out}"
            );
            assert!(out.contains(reason), "{how}: the reason:\n{out}");
            assert!(
                out.contains(&format!(
                    "`{THIRD}` skips the host GPU: the device was found removed in `{culprit}`"
                )),
                "{how}:\n{out}"
            );
        }
    }
}
