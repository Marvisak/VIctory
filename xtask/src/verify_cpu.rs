//! An explicit CPU-only selection of the existing synthetic test suites.
//!
//! This is a reviewed selection, not a sandbox: keep it in sync when adding
//! tests. In particular, `victory-shader --lib` also contains GPU tests on
//! Windows, so it must never run here without an explicit filter.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Instant;

use anyhow::{Context, Result, ensure};

#[derive(Debug)]
struct Check {
    name: &'static str,
    covers: &'static str,
    args: Vec<String>,
    tests: bool,
    windows_only: bool,
}

fn cargo_check(name: &'static str, covers: &'static str, args: &[&str], jobs: usize) -> Check {
    let mut args: Vec<String> = args.iter().map(|arg| (*arg).into()).collect();
    args.extend([
        "--locked".into(),
        "--color".into(),
        "never".into(),
        "--jobs".into(),
        jobs.to_string(),
    ]);
    Check {
        name,
        covers,
        args,
        tests: false,
        windows_only: false,
    }
}

fn test_check(
    name: &'static str,
    covers: &'static str,
    args: &[&str],
    filter: &[&str],
    jobs: usize,
    windows_only: bool,
) -> Check {
    let mut check = cargo_check(name, covers, args, jobs);
    check.tests = true;
    check.windows_only = windows_only;
    check.args.push("--".into());
    check.args.extend(filter.iter().map(|arg| (*arg).into()));
    check.args.push("--test-threads=1".into());
    check
}

fn plan(jobs: usize) -> Vec<Check> {
    let mut checks = vec![
        cargo_check(
            "compile",
            "Type-check every workspace target without executing it",
            &["check", "--workspace", "--all-targets"],
            jobs,
        ),
        test_check(
            "source-libraries",
            "Configuration, PE parsing, formats/codecs, cache and synthetic game containers",
            &[
                "test",
                "--lib",
                "-p",
                "victory-core",
                "-p",
                "victory-pe",
                "-p",
                "victory-formats",
                "-p",
                "victory-pipeline",
                "-p",
                "victory-game",
            ],
            &[],
            jobs,
            false,
        ),
        test_check(
            "loader-unit",
            "Loader preparation, relocations and TLS unit tests",
            &["test", "--lib", "-p", "victory-loader"],
            &[],
            jobs,
            false,
        ),
        test_check(
            "loader-probe",
            "Build/load/bind/call our own hle-probe DLL; no title binary",
            &["test", "-p", "victory-loader", "--test", "load_probe"],
            &[],
            jobs,
            true,
        ),
    ];
    for (name, covers, filter) in [
        (
            "hle-files",
            "Temporary files and virtual path rewrites",
            "modules::files::tests::",
        ),
        (
            "hle-storage",
            "Temporary storage, quota and async space queries",
            "modules::storage::tests::",
        ),
        (
            "hle-runtime",
            "Our runtime imports and API lookup",
            "modules::xgameruntime::tests::",
        ),
        (
            "hle-async",
            "Synthetic task queues, completion and cancellation",
            "modules::xasync::tests::",
        ),
        (
            "hle-saves",
            "Synthetic save transactions in temporary directories",
            "modules::xgamesave::tests::",
        ),
        (
            "hle-memory",
            "CPU allocation, page aliases and mapping",
            "modules::xmem::tests::",
        ),
    ] {
        checks.push(test_check(
            name,
            covers,
            &["test", "--lib", "-p", "victory-hle"],
            &[filter],
            jobs,
            true,
        ));
    }
    checks.push(test_check(
        "shader-analysis",
        "Synthetic containers, instruction decoding, SSA, ranges, resources and EXEC structure",
        &["test", "--lib", "-p", "victory-shader"],
        &["--skip", "emit::"],
        jobs,
        false,
    ));
    for (name, covers, filter) in [
        (
            "shader-lowering",
            "Lower synthetic shader programs without dispatching them",
            "emit::lower::tests::",
        ),
        (
            "shader-bitcode",
            "LLVM bitcode serialization",
            "emit::bitcode::tests::",
        ),
        (
            "shader-container",
            "Generated DXIL container records and hashes",
            "emit::container::tests::",
        ),
        (
            "shader-wave",
            "Wave control-flow analysis",
            "emit::wave::tests::",
        ),
        (
            "shader-sampler",
            "Sampler constants and encoding",
            "emit::sampler::tests::",
        ),
    ] {
        checks.push(test_check(
            name,
            covers,
            &["test", "--lib", "-p", "victory-shader"],
            &[filter],
            jobs,
            false,
        ));
    }
    checks.push(test_check(
        "tooling",
        "The asset policy and this verification plan/report parser",
        &["test", "-p", "xtask"],
        &[],
        jobs,
        false,
    ));
    if let Some(target) = selected_target() {
        for check in &mut checks {
            let at = check
                .args
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or(check.args.len());
            check
                .args
                .splice(at..at, ["--target".to_owned(), target.clone()]);
        }
    }
    checks
}

fn selected_target() -> Option<String> {
    std::env::var("CARGO_BUILD_TARGET")
        .ok()
        .and_then(normalize_target)
        .or_else(|| {
            cfg!(all(windows, target_arch = "x86_64")).then(|| "x86_64-pc-windows-msvc".to_owned())
        })
}

fn normalize_target(target: String) -> Option<String> {
    let target = target.trim();
    (!target.is_empty()).then(|| target.to_owned())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    passed: u64,
    failed: u64,
    ignored: u64,
    suites: u64,
}

fn counts(text: &str) -> Counts {
    let mut total = Counts::default();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("test result: ") else {
            continue;
        };
        let Some((status, rest)) = rest.split_once(". ") else {
            continue;
        };
        if !matches!(status, "ok" | "FAILED") {
            continue;
        }
        let mut fields = rest.split(';');
        let parse = |field: Option<&str>, suffix: &str| -> Option<u64> {
            field?.trim().strip_suffix(suffix)?.parse().ok()
        };
        let Some(passed) = parse(fields.next(), " passed") else {
            continue;
        };
        let Some(failed) = parse(fields.next(), " failed") else {
            continue;
        };
        let Some(ignored) = parse(fields.next(), " ignored") else {
            continue;
        };
        total.passed += passed;
        total.failed += failed;
        total.ignored += ignored;
        total.suites += 1;
    }
    total
}

fn accepted(check: &Check, success: bool, counts: Counts) -> bool {
    success && (!check.tests || counts.suites > 0 && counts.passed > 0 && counts.failed == 0)
}

fn excluded_environment(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.starts_with("VICTORY_") || matches!(name.as_str(), "GH_TOKEN" | "GITHUB_TOKEN")
}

fn removed_environment() -> Vec<OsString> {
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| excluded_environment(&name.to_string_lossy()))
        .collect()
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn metadata(root: &Path, program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unavailable".into())
}

pub(crate) fn run(root: &Path, jobs: usize, report: Option<&Path>, list: bool) -> Result<()> {
    ensure!(jobs > 0, "--jobs must be greater than zero");
    let checks = plan(jobs);
    if list {
        for check in &checks {
            println!(
                "{}{}: cargo {}\n  {}",
                check.name,
                if check.windows_only { " [Windows]" } else { "" },
                check.args.join(" "),
                check.covers
            );
        }
        println!(
            "Omitted: title execution/data, GPU tests, GPU pipeline validation, real-game compatibility and performance."
        );
        return Ok(());
    }
    if let Some(path) = report
        && let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
    {
        ensure!(
            parent.is_dir(),
            "report directory does not exist: {}",
            parent.display()
        );
    }
    let mut markdown = String::from("# CPU-only source verification\n\n");
    let state = match metadata(root, "git", &["status", "--porcelain"]).as_str() {
        "" => "clean",
        "unavailable" => "unavailable",
        _ => "modified",
    };
    writeln!(
        markdown,
        "Source commit: `{}` (working tree: {state}). Platform: `{}/{}`.\n",
        metadata(root, "git", &["rev-parse", "HEAD"]),
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    writeln!(
        markdown,
        "Compiler: `{}`.\n",
        metadata(root, "rustc", &["--version"])
    )?;
    writeln!(
        markdown,
        "Cargo target: `{}`.\n",
        selected_target().as_deref().unwrap_or("Cargo default")
    )?;
    markdown.push_str("Scope: the existing synthetic source tests selected below. Title execution and game data are not part of this plan. GPU execution/validation, real-game compatibility and performance are unverified. A passing build does not establish runtime behavior.\n\n");
    markdown.push_str(
        "| Check | Result | Passed | Failed | Ignored | Seconds |\n|---|---|---:|---:|---:|---:|\n",
    );
    let environment = removed_environment();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut details = String::new();
    let mut failures = 0;
    for check in &checks {
        writeln!(
            details,
            "\n## {}\n\n{}\n\nCommand: `cargo {}`\n",
            check.name,
            check.covers,
            check.args.join(" ")
        )?;
        if check.windows_only && !cfg!(windows) {
            writeln!(
                markdown,
                "| {} | SKIP (requires Windows) | — | — | — | — |",
                check.name
            )?;
            details.push_str("Not executed: requires Windows. This is not a passing test.\n");
            continue;
        }
        println!("{}: cargo {}", check.name, check.args.join(" "));
        let started = Instant::now();
        let mut command = Command::new(&cargo);
        command
            .args(&check.args)
            .current_dir(root)
            .env("CARGO_TERM_COLOR", "never");
        if let Some(target) = selected_target() {
            // The loader's synthetic DLL fixture starts a nested Cargo build.
            // Keep it on the same target as the outer verification command.
            command.env("CARGO_BUILD_TARGET", target);
        }
        for name in &environment {
            command.env_remove(name);
        }
        let output = command.output();
        let (success, text, code) = match output {
            Ok(output) => (
                output.status.success(),
                output_text(&output),
                format!("{:?}", output.status.code()),
            ),
            Err(error) => (
                false,
                format!("Could not execute Cargo: {error}\n"),
                "not started".into(),
            ),
        };
        let totals = counts(&text);
        let ok = accepted(check, success, totals);
        let status = if ok {
            "PASS"
        } else {
            failures += 1;
            "FAIL"
        };
        let seconds = started.elapsed().as_secs_f64();
        writeln!(
            markdown,
            "| {} | {status} | {} | {} | {} | {seconds:.2} |",
            check.name, totals.passed, totals.failed, totals.ignored
        )?;
        println!(
            "  {status}: {} passed, {} failed, {} ignored ({seconds:.2}s)",
            totals.passed, totals.failed, totals.ignored
        );
        writeln!(details, "Result: {status}. Process exit: {code}.\n")?;
        if check.tests && success && !ok {
            details.push_str("The command exited successfully but no nonempty passing test run was observed; a missing/renamed filter cannot silently pass.\n\n");
        }
        // A four-backtick fence keeps ordinary code fences printed by doctests inside the log.
        details.push_str("````text\n");
        details.push_str(&text.replace("````", "` ` ` `"));
        if !text.ends_with('\n') {
            details.push('\n');
        }
        details.push_str("````\n");
    }
    markdown.push_str("\nIgnored tests and Windows-only checks skipped on another host are missing coverage. The selection is reviewed source, not a sandbox; audit it when adding tests. `VICTORY_*` path/data overrides and GitHub token variables are removed from child Cargo processes.\n");
    markdown.push_str(&details);
    if let Some(path) = report {
        std::fs::write(path, &markdown).with_context(|| format!("writing {}", path.display()))?;
        println!("Report: {}", path.display());
    } else {
        print!("{markdown}");
    }
    ensure!(
        failures == 0,
        "{failures} verification checks failed; see the report/log output"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_path_overrides_and_github_tokens_are_excluded() {
        for name in [
            "VICTORY_GAME_DIR",
            "Victory_Ref_Ps",
            "VICTORY_CONFIG",
            "GH_TOKEN",
            "GITHUB_TOKEN",
        ] {
            assert!(excluded_environment(name), "{name}");
        }
        for name in ["PATH", "RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR"] {
            assert!(!excluded_environment(name), "{name}");
        }
    }

    #[test]
    fn plan_uses_only_locked_cargo_checks_and_selected_tests() {
        for check in plan(2) {
            assert!(matches!(check.args[0].as_str(), "check" | "test"));
            assert!(check.args.iter().any(|arg| arg == "--locked"));
            assert!(
                !check
                    .args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "--execute" | "run" | "--ignored"))
            );
            if check.tests {
                assert!(check.args.iter().any(|arg| arg == "--test-threads=1"));
            }
            if let Some(target) = selected_target() {
                let delimiter = check
                    .args
                    .iter()
                    .position(|arg| arg == "--")
                    .unwrap_or(check.args.len());
                let before_test_args = &check.args[..delimiter];
                let target_arg = before_test_args
                    .iter()
                    .position(|arg| arg == "--target")
                    .unwrap();
                assert_eq!(before_test_args[target_arg + 1], target);
            }
        }
    }

    #[test]
    fn shader_tests_always_have_an_explicit_cpu_filter() {
        for check in plan(2)
            .into_iter()
            .filter(|check| check.args.iter().any(|arg| arg == "victory-shader"))
        {
            let delimiter = check.args.iter().position(|arg| arg == "--").unwrap();
            let args = &check.args[delimiter + 1..];
            assert!(
                args.iter().any(|arg| arg == "emit::"
                    || arg.starts_with("emit::") && arg.ends_with("::tests::"))
            );
            assert!(!args.iter().any(|arg| arg.contains("synthetic_tests")
                || arg.contains("valu_tests")
                || arg.contains("host::")));
        }
    }

    #[test]
    fn hle_and_probe_checks_are_marked_as_windows_only() {
        for check in plan(2)
            .into_iter()
            .filter(|check| check.name.starts_with("hle-") || check.name == "loader-probe")
        {
            assert!(check.windows_only);
        }
    }

    #[test]
    fn target_overrides_are_trimmed_and_empty_values_ignored() {
        assert_eq!(
            normalize_target(" x86_64-pc-windows-msvc  ".into()).as_deref(),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(normalize_target("  ".into()), None);
    }

    #[test]
    fn parses_and_sums_complete_libtest_summaries_only() {
        let text = "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 8 filtered out; finished in 0.01s\n\
                    warning: 99 passed is not a summary\n\
                    test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s\n";
        assert_eq!(
            counts(text),
            Counts {
                passed: 5,
                failed: 1,
                ignored: 1,
                suites: 2
            }
        );
        assert_eq!(counts("test result: ok. incomplete"), Counts::default());
    }

    #[test]
    fn empty_filters_and_ignored_only_runs_cannot_pass() {
        let check = plan(2).remove(1);
        assert!(!accepted(&check, true, Counts::default()));
        assert!(!accepted(
            &check,
            true,
            Counts {
                ignored: 4,
                suites: 1,
                ..Counts::default()
            }
        ));
        assert!(accepted(
            &check,
            true,
            Counts {
                passed: 1,
                suites: 1,
                ..Counts::default()
            }
        ));
    }

    #[test]
    fn command_failures_are_not_hidden_by_passing_summaries() {
        let check = plan(2).remove(1);
        assert!(!accepted(
            &check,
            false,
            Counts {
                passed: 10,
                suites: 1,
                ..Counts::default()
            }
        ));
        assert!(!accepted(
            &check,
            true,
            Counts {
                passed: 10,
                failed: 1,
                suites: 1,
                ..Counts::default()
            }
        ));
    }

    #[test]
    fn build_checks_do_not_require_test_summaries() {
        let check = plan(2).remove(0);
        assert!(accepted(&check, true, Counts::default()));
        assert!(!accepted(&check, false, Counts::default()));
    }
}
