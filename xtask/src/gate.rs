//! `cargo xtask test-gate`: run the tests, then check the run against its own
//! account of itself.
//!
//! A green check that cannot be trusted is worse than no check, because it is
//! read as safety. libtest announces a binary's tests (`running N tests`) and
//! reports them (`test result: …`), and a filter that matches nothing prints
//! `running 0 tests` and exits 0 — which CI reports as a pass. This gate
//! counts what each binary announced against what it accounted for, and fails
//! a run that executed nothing.
//!
//! Three things make that reading trustworthy:
//!
//! - Targets run serially (`--jobs 1`). Cargo otherwise runs test binaries
//!   concurrently into one stdout, so a `test result:` from one binary lands
//!   between another's lines and no report can be attributed. A caller's own
//!   `-j`/`--jobs` is refused rather than honoured, because it would void the
//!   property the reading depends on. `--jobs 1` serializes compilation too,
//!   so CI builds the suite in parallel first (`cargo test --no-run`) and
//!   runs this with nothing left to build.
//! - The counts are only read when `cargo test` exited 0. A failing run
//!   dumps captured test output into the same stream, which can contain
//!   harness-shaped lines; that run is already red, so the gate reports
//!   cargo's own diagnosis instead of guessing at the counts. It also echoes
//!   each failing test's panic: this process captured cargo's output, so if
//!   the gate does not print the reason, nothing does.
//! - Reports nest. A test that spawns `current_exe()` writes the child's
//!   harness lines into the same stream, ahead of the parent's own result.
//!   `parse` attaches counts last-in-first-out, and only the outermost
//!   reports — the binaries cargo ran and named — are gated. A report that
//!   never closes is named as such: its parent's result went to the child, so
//!   its zeros are not what it executed.
//!
//! Cargo names each target it runs, and those names must pair one-to-one with
//! the outermost reports. When they do not, nothing attributes a report to a
//! binary, so the per-binary check cannot run and the run is refused rather
//! than reported green with the check silently skipped.
//!
//! What this cannot see, stated so the green is not read as more than it is:
//!
//! - stdout is a pipe any test can write to. A test that prints a
//!   `test result:` line and exits the process can close its binary's report
//!   on the harness's behalf, and the gate never counts `test … ok` lines, so
//!   only cargo's exit status protects a *red* run. A green gate means every
//!   binary cargo named reported a consistent account; it is not proof that no
//!   test wrote into that account.
//! - a target built with `harness = false` prints no libtest report, so a run
//!   that contains one is refused as unattributable rather than gated.
//! - a narrowed command (`--exclude`, `-p`, `--lib`, `--doc`) is gated as the
//!   set it ran. The gate cannot know what the workspace would otherwise have
//!   run, so CI's command line is what asserts coverage.
//!
//! Doc-test binaries are exempt from the per-binary zero check: a crate with
//! no doc examples runs no doc tests, and a workspace sweep would otherwise
//! be permanently red for a fact that is not a defect. The whole-run check
//! still covers them.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};

/// One test binary: what it announced, and what it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The target as cargo named it, e.g. `unittests src\main.rs`; `None`
    /// until `pair` attaches one, which happens only when cargo's names and
    /// the reports line up.
    pub target: Option<String>,
    /// How many reports were open when this one was announced: 0 is a binary
    /// cargo ran, 1+ is a child process a test spawned. A test that spawns
    /// `current_exe()` writes the child's `running N tests` / `test result:`
    /// lines into the same stream, ahead of the parent's own result.
    pub depth: usize,
    /// What `running N tests` announced; ignored tests are counted here.
    pub announced: usize,
    /// Whether no `test result:` ever closed this announcement. A child that
    /// announces inside its parent and never reports consumes the parent's
    /// result, so the parent is left like this; its zeros are not what it
    /// executed and it is named instead of read as a binary that ran nothing.
    pub unreported: bool,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    /// Tests libtest removed before running them. `running 0 tests` alone
    /// cannot tell a binary with no tests from a filter that matched none:
    /// the first counted 0 filtered out, the second counted the tests it
    /// removed. Only the second is a defect.
    pub filtered: usize,
}

impl Report {
    /// How to name this report in a message.
    fn label(&self) -> &str {
        self.target.as_deref().unwrap_or("(unnamed target)")
    }

    /// Tests that actually executed (ignored ones did not).
    fn ran(&self) -> usize {
        self.passed + self.failed
    }

    /// Tests the binary accounted for, executed or ignored.
    fn counted(&self) -> usize {
        self.passed + self.failed + self.ignored
    }
}

/// The arguments for `cargo test`, with the gate's own flags placed where
/// cargo accepts them: before the `--` that starts libtest's arguments.
///
/// Appending them at the end instead hands `--no-fail-fast` and `--jobs` to
/// libtest as soon as the caller has written `--`, and libtest refuses them
/// (`error: Unrecognized option: 'no-fail-fast'`, exit 101, no test runs).
///
/// `--jobs 1` is not optional: with binaries running concurrently into one
/// stdout, no report can be attributed, so a caller's `-j`/`--jobs` is refused
/// rather than honoured or silently overridden.
pub fn argv(args: &[String]) -> Result<Vec<String>> {
    let separator = args.iter().position(|a| a == "--");
    let (to_cargo, to_harness) = match separator {
        Some(at) => (&args[..at], Some(&args[at + 1..])),
        None => (args, None),
    };
    if let Some(given) = to_cargo
        .iter()
        .find(|a| a.starts_with("-j") || a.starts_with("--jobs"))
    {
        bail!(
            "`{given}` is the gate's own flag: it runs one test binary at a time, which is what makes every report attributable; remove it from the arguments"
        );
    }
    let mut out = vec![String::from("test")];
    out.extend(to_cargo.iter().cloned());
    // Every binary reports, even when an earlier one failed.
    if !to_cargo.iter().any(|a| a == "--no-fail-fast") {
        out.push(String::from("--no-fail-fast"));
    }
    // See the module doc: without one binary at a time, the output cannot be
    // attributed.
    out.push(String::from("--jobs"));
    out.push(String::from("1"));
    if let Some(to_harness) = to_harness {
        out.push(String::from("--"));
        out.extend(to_harness.iter().cloned());
    }
    Ok(out)
}

/// Run `cargo test` with `args`, then gate the run.
pub fn run(root: &Path, args: &[String]) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.args(argv(args)?);
    // Streamed, not buffered. `.output()` kept everything until the child
    // exited, so a run that hung -- or was killed by CI's timeout -- left no
    // log at all. Each chunk is echoed as it arrives and collected for the
    // checks below.
    let mut child = command
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running `cargo test`")?;
    let stdout_pipe = child
        .stdout
        .take()
        .context("cargo test left no stdout pipe")?;
    let stderr_pipe = child
        .stderr
        .take()
        .context("cargo test left no stderr pipe")?;
    let stdout_thread = std::thread::spawn(move || copy(stdout_pipe, std::io::stdout()));
    let stderr_thread = std::thread::spawn(move || copy(stderr_pipe, std::io::stderr()));
    let status = child.wait().context("waiting for `cargo test`")?;
    let stdout = joined(stdout_thread, "stdout")?;
    let stderr = joined(stderr_thread, "stderr")?;
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);

    if !status.success() {
        // Already red, so the counts are not read: a failure dump can hold
        // harness-shaped lines. The report is what a reader needs instead,
        // and this process captured it, so nothing else will print it.
        let report = failure_report(&stdout, &stderr);
        if !report.is_empty() {
            eprintln!("{report}");
        }
        bail!("cargo test exited {status}");
    }

    let mut reports = parse(&stdout);
    if reports.is_empty() {
        // Nothing to gate: say why rather than report success on nothing.
        bail!(
            "no test binary reported anything (cargo test exited {status}):\n{}",
            tail(&stderr, 20)
        );
    }
    let names = targets(&stderr);
    if let Err(problem) = pair(&mut reports, &names) {
        // Nothing attributes a report to a binary, so the per-binary rules
        // cannot run. The counts are printed once so the reader can see what
        // came out of the stream, then the run is refused.
        print_reports(&reports);
        eprintln!("GATE: {problem}");
        bail!(
            "this run's reports cannot be attributed to the binaries cargo ran, so it cannot be called green"
        );
    }
    print_reports(&reports);
    let top_reports: Vec<Report> = reports.iter().filter(|r| r.depth == 0).cloned().collect();
    let ran: usize = top_reports.iter().map(Report::ran).sum();
    println!(
        "test gate: {} binary/binaries, {ran} test(s) executed",
        top_reports.len()
    );
    let nested = reports.len() - top_reports.len();
    if nested > 0 {
        // Counted separately, not hidden: a nested run is a test running
        // this binary again, not a target cargo executed.
        println!("test gate: {nested} nested report(s), from test(s) that ran this binary again");
    }

    let problems = problems(&top_reports);
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("GATE: {p}");
        }
        bail!(
            "{} test-gate problem(s) in a run cargo called a success",
            problems.len()
        );
    }
    Ok(())
}

/// Read a child's stream to EOF, echoing each chunk as it arrives and
/// returning everything read.
///
/// The echo happens per chunk rather than after the child exits, so a run that
/// hangs -- or is killed by CI's timeout -- keeps the log of what it had
/// printed.
fn copy(mut from: impl Read, mut to: impl Write) -> std::io::Result<Vec<u8>> {
    let mut all = Vec::new();
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(all),
            Ok(read) => read,
            // End the capture rather than discard it: the bytes already
            // echoed are the only log a killed run leaves, and a read error
            // says nothing about what was run.
            Err(_) => return Ok(all),
        };
        all.extend_from_slice(&buffer[..read]);
        // The capture is what the gate parses. A destination that closed (a
        // reader that piped this into `head` and stopped) must not lose it,
        // so its error is dropped rather than failing the run.
        let _ = to.write_all(&buffer[..read]);
        let _ = to.flush();
    }
}

/// Wait for one of the reader threads and collect what it captured.
fn joined(
    thread: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>> {
    thread
        .join()
        .map_err(|_| anyhow!("the thread reading cargo's {stream} panicked"))?
        .with_context(|| format!("reading cargo's {stream}"))
}

/// Attach cargo's target names to the reports of the binaries cargo ran.
///
/// Cargo names each target on stderr just before running it, so the Nth name
/// matches the Nth outermost report -- while the counts agree. When they do
/// not, nothing attributes a report to a binary: a caller's `-j` interleaves
/// them, and a report that arrives after another's summary has no place in
/// the order. A wrong name is worse than none, and the per-binary rules need
/// a real one, so a mismatch is an error rather than a skipped check.
pub fn pair(reports: &mut [Report], names: &[String]) -> Result<()> {
    let top: Vec<usize> = reports
        .iter()
        .enumerate()
        .filter(|(_, r)| r.depth == 0)
        .map(|(i, _)| i)
        .collect();
    if names.len() != top.len() {
        bail!(
            "cargo named {} target(s) but {} report(s) came from binaries it ran; no report can be attributed to a binary",
            names.len(),
            top.len()
        );
    }
    for (index, name) in top.iter().zip(names) {
        reports[*index].target = Some(name.clone());
    }
    Ok(())
}

/// One line per binary cargo ran, in the order it ran them.
fn print_reports(reports: &[Report]) {
    for r in reports.iter().filter(|r| r.depth == 0) {
        println!(
            "  {}: {} announced, {} accounted for ({} passed, {} failed, {} ignored)",
            r.label(),
            r.announced,
            r.counted(),
            r.passed,
            r.failed,
            r.ignored
        );
    }
}

/// Everything wrong with a run that exited 0. Pure, so the rules can be
/// tested without a suite to run.
pub fn problems(reports: &[Report]) -> Vec<String> {
    let mut out = Vec::new();
    // Every binary announced nothing. That is one cause, one message, whether
    // it is an empty list or a filter that removed every test everywhere; a
    // line per binary would turn a workspace typo into twenty-six.
    let announced_total: usize = reports.iter().map(|r| r.announced).sum();
    if announced_total == 0 {
        return vec![format!(
            "the run executed no test ({} binary/binaries, 0 announced)",
            reports.len()
        )];
    }
    // An announcement no result ever closed: a child that announced inside
    // its parent and never reported consumed the parent's result, so the
    // parent's zeros are not what it executed. Named as the unaccounted
    // announcement it is; "accounted for 0" would report the symptom.
    for r in reports.iter().filter(|r| r.unreported) {
        out.push(format!(
            "{}: announced {} and never accounted for",
            r.label(),
            r.announced
        ));
    }
    for r in reports.iter().filter(|r| !r.unreported) {
        if r.counted() != r.announced {
            out.push(format!(
                "{}: announced {} test(s), accounted for {}",
                r.label(),
                r.announced,
                r.counted()
            ));
        }
        // A doc-test binary in a crate with no doc examples runs nothing by
        // design, and so does a binary target with no tests of its own: both
        // report `0 filtered out`. A filter that matched none of a binary's
        // tests reports the tests it removed instead. Telling them apart takes
        // the filtered count and a name, and an unnamed report here would fail
        // every doc-test crate in the sweep.
        let removed = r.announced == 0
            && r.filtered > 0
            && r.target
                .as_deref()
                .is_some_and(|name| !name.starts_with("Doc-tests"));
        if removed {
            out.push(format!(
                "{}: announced 0 tests after filtering out {} (a filter that matched nothing passes cargo)",
                r.label(),
                r.filtered
            ));
        }
    }
    let ran: usize = reports.iter().map(Report::ran).sum();
    if ran == 0 && out.is_empty() {
        // One cause, one message. Tests were announced and every one was
        // ignored; the all-zero case returned above.
        return vec![format!(
            "{announced_total} test(s) announced and none executed (every test that reported was ignored)"
        )];
    }
    out
}

/// Read libtest's account of a run, in the order it printed it.
///
/// Reports nest. A test that spawns `current_exe()` inherits this stream, so
/// the child's `running N tests` lands between the parent's announcement and
/// its result — measured in `victory-hle`, where the child's 1-test report
/// arrives first and the parent's 152-test result last. Results therefore
/// close reports last-in-first-out; attaching one to the latest announcement
/// instead (rather than the latest *unresolved* one) would give the parent's
/// 152 counts to the child and leave the parent accounting for nothing.
pub fn parse(output: &str) -> Vec<Report> {
    let mut reports: Vec<Report> = Vec::new();
    // Indices of reports announced but not yet closed by a result.
    let mut open: Vec<usize> = Vec::new();
    for line in output.lines() {
        let line = plain(line.trim_end_matches('\r'));
        if let Some(announced) = announced(&line) {
            reports.push(Report {
                target: None,
                depth: open.len(),
                announced,
                unreported: true,
                passed: 0,
                failed: 0,
                ignored: 0,
                filtered: 0,
            });
            open.push(reports.len() - 1);
        } else if let Some((passed, failed, ignored, filtered)) = result(&line)
            && let Some(index) = open.pop()
        {
            let report = &mut reports[index];
            report.unreported = false;
            report.passed = passed;
            report.failed = failed;
            report.ignored = ignored;
            report.filtered = filtered;
        }
    }
    reports
}

/// A line with ANSI color sequences removed.
///
/// CI sets `CARGO_TERM_COLOR=always`, and cargo colors `Running` and
/// `Doc-tests`. The prefix matches are on the plain word, and a colored line
/// starts with an escape byte — which is how `targets` returned zero names on
/// CI while finding them locally, where cargo writes no color.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: `ESC [`, then parameter and intermediate bytes, then a final
        // byte in `@`..=`~`. Anything else after an escape is dropped with it.
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if ('@'..='~').contains(&c) {
                break;
            }
        }
    }
    out
}

/// The targets cargo named, in the order it ran them: `Running unittests
/// src\main.rs (…)` and `Doc-tests victory_core`. Cargo prints these on
/// stderr; libtest's numbers come on stdout.
pub fn targets(progress: &str) -> Vec<String> {
    progress
        .lines()
        .filter_map(|line| {
            // Strip first: cargo's indent sits *inside* the color span, so it
            // survives the escapes and trimming has to come after.
            let line = plain(line);
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Running ") {
                return Some(rest.split(" (").next().unwrap_or(rest).to_owned());
            }
            line.starts_with("Doc-tests ").then_some(line.to_owned())
        })
        .collect()
}

/// The tests that reported themselves FAILED, as libtest printed them.
pub fn failures(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| plain(line.trim_end_matches('\r')))
        .map(|line| line.trim_end().to_owned())
        .filter(|line| line.starts_with("test ") && line.ends_with(" ... FAILED"))
        .collect()
}

/// `running 4 tests` / `running 1 test`.
fn announced(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("running ")?;
    let (n, what) = rest.split_once(' ')?;
    if !matches!(what, "test" | "tests") {
        return None;
    }
    n.parse().ok()
}

/// `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered
/// out; finished in 0.22s`. The verdict is not read: a run that reaches here
/// exited 0, so it was `ok`.
fn result(line: &str) -> Option<(usize, usize, usize, usize)> {
    let (_, counts) = line.strip_prefix("test result: ")?.split_once(". ")?;
    let mut out = (0, 0, 0, 0);
    for part in counts.split("; ") {
        let Some((n, kind)) = part.split_once(' ') else {
            continue;
        };
        let Ok(n) = n.parse::<usize>() else {
            continue;
        };
        match kind {
            "passed" => out.0 = n,
            "failed" => out.1 = n,
            "ignored" => out.2 = n,
            "filtered out" => out.3 = n,
            _ => {}
        }
    }
    Some(out)
}

/// The last lines of a stream, for an error a reader can act on.
fn tail(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// What a red run should say, from the two streams cargo wrote.
///
/// A failing test is named, and the panic block libtest printed for it is
/// echoed. A run that failed with no failed test never reached one: a build
/// error, a bad argument, or a harness that refused a flag. For that the first
/// error block(s) are quoted whole — the `error[E0308]` line, the `--> file:line`
/// that locates it, the span under it — because a prefix filter drops the
/// location and a tail drops the cause.
pub fn failure_report(stdout: &str, stderr: &str) -> String {
    let failed = failures(stdout);
    if failed.is_empty() {
        return error_excerpt(stderr);
    }
    let mut out = String::new();
    for line in stderr
        .lines()
        .map(|line| plain(line.trim_end()))
        .filter(|line| line.starts_with("error"))
    {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&format!("{} test(s) reported FAILED:\n", failed.len()));
    for line in failed.iter().take(40) {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    // Why they fell over. The echo above showed these bytes live; this repeats
    // the blocks after the name list so the report reads as one account, and a
    // reader who kept only the log still has the panic.
    let why = details(stdout);
    if !why.is_empty() {
        out.push_str(&why);
        out.push('\n');
    }
    out.trim_end().to_owned()
}

/// How much of a red stream to quote: the first few error blocks, each with
/// its context, not the whole transcript.
const EXCERPT_BLOCKS: usize = 4;
const EXCERPT_BLOCK_LINES: usize = 24;

/// The first error blocks of a red stream, with the lines that locate them.
///
/// rustc writes one block per error: the `error[E…]` line, the `--> file:line`
/// that locates it, the source line the span marks, then a blank line. A run
/// that failed before any test can print hundreds of lines after the first
/// error — a whole build transcript — so a tail of the stream drops exactly
/// the error that explains the rest. The stream's tail is still the fallback
/// for a failure that prints no `error` line at all.
pub fn error_excerpt(stderr: &str) -> String {
    // Colour first: CI sets `CARGO_TERM_COLOR=always`, and a coloured
    // `error[E0308]` starts with an escape byte, so a raw prefix match finds
    // no block and falls back to the tail — the very behaviour this excerpt
    // replaced.
    let lines: Vec<String> = stderr.lines().map(|line| plain(line.trim_end())).collect();
    let mut out: Vec<&str> = Vec::new();
    let mut blocks = 0;
    let mut start = 0;
    while start < lines.len() && blocks < EXCERPT_BLOCKS {
        if !lines[start].starts_with("error") {
            start += 1;
            continue;
        }
        blocks += 1;
        // The error line, then its context until a blank line or the next
        // error. The cap is a safety net for a block that never ends; rustc
        // puts the `-->` and the span well inside it.
        let mut end = start + 1;
        while end < lines.len()
            && end < start + EXCERPT_BLOCK_LINES
            && !lines[end].trim().is_empty()
            && !lines[end].starts_with("error")
        {
            end += 1;
        }
        out.extend(lines[start..end].iter().map(String::as_str));
        out.push("");
        start = end;
    }
    if out.is_empty() {
        return tail(stderr, 60);
    }
    out.join("\n").trim_end().to_owned()
}

/// What each failing test printed, from libtest's `---- <name> stdout ----`
/// blocks. A red run that names the test and stops leaves the reader with
/// nothing to act on, and the stream this reads was captured by the caller,
/// so nobody else is going to print it.
///
/// Bounded, because a run can fail everywhere at once and the point is to
/// make the first cause readable, not to reproduce the whole suite.
fn details(output: &str) -> String {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in output.lines() {
        let line = line.trim_end_matches('\r').trim_end();
        if line.starts_with("---- ") && line.ends_with(" ----") {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = Some(vec![line]);
            continue;
        }
        // The name list that closes a block. The one printed before the
        // blocks arrives while nothing is open, so it is ignored.
        if line == "failures:" {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            continue;
        }
        if let Some(block) = current.as_mut() {
            block.push(line);
        }
    }
    if let Some(block) = current {
        blocks.push(block);
    }

    let mut out: Vec<&str> = Vec::new();
    for block in &blocks {
        let end = block
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(0, |i| i + 1);
        out.extend_from_slice(&block[..end]);
        out.push("");
    }
    out.truncate(out.len().min(200));
    out.join("\n").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        Report, argv, copy, details, failure_report, failures, pair, parse, problems, run, targets,
    };

    /// One binary: `announced` tests, one `ok` line per test, then the result.
    fn binary(announced: usize, verdict: &str, counts: &str) -> String {
        let lines: String = (0..announced)
            .map(|i| format!("test t{i} ... ok\n"))
            .collect();
        format!(
            "running {announced} tests\n{lines}\n\
             test result: {verdict}. {counts}; finished in 0.01s\n"
        )
    }

    const OK4: &str = "4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
    const OK2: &str = "2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";

    #[test]
    fn counts_a_run_that_did_what_it_announced() {
        let reports = parse(&binary(4, "ok", OK4));
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].announced, 4);
        assert_eq!(reports[0].passed, 4);
        assert!(problems(&reports).is_empty(), "{:?}", problems(&reports));
    }

    #[test]
    fn a_filter_that_matched_nothing_is_one_failure() {
        let nothing = "0 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out";
        // A second binary that ran something, so the whole-run rule does not
        // answer first: this exercises the per-binary `filtered` rule.
        let mut reports = parse(&format!(
            "{}{}",
            binary(2, "ok", OK2),
            binary(0, "ok", nothing)
        ));
        reports[1].target = Some(String::from("unittests src\\lib.rs"));
        let trouble = problems(&reports);
        assert_eq!(trouble.len(), 1, "one cause, one message: {trouble:?}");
        assert!(
            trouble[0].contains("a filter that matched nothing"),
            "{trouble:?}"
        );
    }

    /// When every binary was filtered to nothing, the cause is one cause: a
    /// workspace typo should not print one line per binary.
    #[test]
    fn every_binary_filtered_to_nothing_is_one_message() {
        let nothing = "0 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out";
        let mut output = String::new();
        for _ in 0..26 {
            output.push_str(&binary(0, "ok", nothing));
        }
        let trouble = problems(&parse(&output));
        assert_eq!(trouble.len(), 1, "one cause, one message: {trouble:?}");
        assert!(trouble[0].contains("executed no test"), "{trouble:?}");
    }

    /// A report that never received a result consumed its parent's: the child
    /// announced inside the parent, took the parent's result, and left the
    /// parent reading as "announced 4, accounted for 0". The old whole-run
    /// message called that "executed no test (1 binary, 4 announced)", which
    /// contradicts itself -- four tests did run. The unclosed announcement is
    /// what happened, and it is named.
    #[test]
    fn a_child_that_announces_and_never_reports_is_named() {
        let output = "\
running 4 tests
running 1 test
test t0 ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let reports = parse(output);
        assert_eq!(reports.len(), 2);
        assert!(reports[0].unreported, "the parent's result went away");
        assert!(!reports[1].unreported, "the child took it");
        assert_eq!(
            reports[1].counted(),
            4,
            "the child holds the parent's counts"
        );
        let mut top: Vec<Report> = reports.into_iter().filter(|r| r.depth == 0).collect();
        top[0].target = Some(String::from("unittests src\\lib.rs"));
        let trouble = problems(&top);
        assert!(
            trouble
                .iter()
                .any(|p| p.contains("unittests src\\lib.rs: announced 4 and never accounted for")),
            "{trouble:?}"
        );
        assert!(
            !trouble.iter().any(|p| p.contains("executed no test")),
            "four tests were announced, so this is not an empty run: {trouble:?}"
        );
    }

    /// A run where every announced test was ignored executed nothing, which
    /// the gate refuses. It is not the empty-run case: the message says what
    /// happened instead of claiming nothing was announced.
    #[test]
    fn a_run_where_every_test_was_ignored_executed_nothing() {
        let output = "\
running 4 tests
test t0 ... ignored
test t1 ... ignored
test t2 ... ignored
test t3 ... ignored

test result: ok. 0 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let trouble = problems(&parse(output));
        assert_eq!(trouble.len(), 1, "{trouble:?}");
        assert!(trouble[0].contains("none executed"), "{trouble:?}");
        assert!(trouble[0].contains("ignored"), "{trouble:?}");
    }

    /// Cargo names each target it runs. When the names and the reports do not
    /// line up, nothing attributes a report to a binary -- a caller's `-j`
    /// interleaves them, and a report that arrives after another's summary has
    /// no place in the order -- so the per-binary check cannot run and the run
    /// cannot be called green.
    #[test]
    fn a_run_whose_reports_do_not_pair_with_cargos_names_is_refused() {
        let output = format!("{}{}", binary(2, "ok", OK2), binary(4, "ok", OK4));
        let mut reports = parse(&output);
        assert_eq!(reports.len(), 2);

        let one_name = targets("Running unittests src\\main.rs (target\\debug\\deps\\xtask.exe)\n");
        let error = pair(&mut reports, &one_name).expect_err("one name and two reports");
        assert!(
            error.to_string().contains("no report can be attributed"),
            "{error}"
        );
        assert!(
            reports.iter().all(|r| r.target.is_none()),
            "no name is attached to the wrong binary: {reports:?}"
        );

        // The same reports with a name each do pair, in cargo's order.
        let two_names = targets(
            "Running unittests src\\main.rs (target\\debug\\deps\\xtask.exe)\nDoc-tests xtask\n",
        );
        pair(&mut reports, &two_names).expect("two names and two reports");
        assert_eq!(reports[0].target.as_deref(), Some("unittests src\\main.rs"));
        assert_eq!(reports[1].target.as_deref(), Some("Doc-tests xtask"));
    }

    /// The end-to-end form of the pairing rule: `run` must refuse a stream
    /// whose reports cannot be attributed. `--quiet` keeps the nested
    /// project's own target names off the stream -- the gate reading this
    /// suite would take them for extra binaries -- and leaves two reports with
    /// no names at all. Before the fix this run printed a note on stdout and
    /// exited 0.
    #[test]
    fn a_run_cargo_cannot_attribute_is_refused_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\n\
             name = \"mismatch\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n\
             \n\
             [workspace]\n\
             \n\
             [[test]]\n\
             name = \"quiet\"\n\
             path = \"tests/quiet.rs\"\n\
             harness = false\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src").join("lib.rs"),
            "#[test]\nfn it_works() {}\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests").join("quiet.rs"), "fn main() {}\n").unwrap();

        let error = run(dir.path(), &[String::from("--quiet")]).expect_err("two reports, no names");
        assert!(
            error.to_string().contains("cannot be attributed"),
            "{error}"
        );
    }

    #[test]
    fn an_announced_count_the_report_contradicts_is_a_failure() {
        // Announced 4, accounted for 3: a run that stopped short.
        let short = "3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
        let trouble = problems(&parse(&binary(4, "ok", short)));
        assert!(
            trouble.iter().any(|p| p.contains("announced 4")),
            "{trouble:?}"
        );
    }

    #[test]
    fn a_named_binary_that_announces_nothing_is_a_failure() {
        // Named, so it is not mistaken for a doc-test crate with no examples,
        // and 3 filtered out, so it is a filter that removed every test it
        // had rather than a target that has none.
        let quiet = "0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out";
        let output = format!("{}{}", binary(2, "ok", OK2), binary(0, "ok", quiet));
        let mut reports = parse(&output);
        reports[1].target = Some(String::from("unittests src\\lib.rs"));
        let trouble = problems(&reports);
        assert_eq!(trouble.len(), 1, "{trouble:?}");
        assert!(trouble[0].contains("after filtering out 3"), "{trouble:?}");
    }

    /// A binary target with no tests of its own reports `running 0 tests` too.
    /// It filtered nothing out, so it is not a filter that matched nothing and
    /// refusing it would fail every workspace that has a test-less bin.
    #[test]
    fn a_target_with_no_tests_is_not_a_filter_failure() {
        let none = "0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
        let output = format!("{}{}", binary(2, "ok", OK2), binary(0, "ok", none));
        let mut reports = parse(&output);
        reports[1].target = Some(String::from("unittests src\\main.rs"));
        let trouble = problems(&reports);
        assert!(trouble.is_empty(), "{trouble:?}");
    }

    #[test]
    fn doc_tests_that_run_nothing_are_not_a_defect() {
        let quiet = "0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
        let output = format!("{}{}", binary(2, "ok", OK2), binary(0, "ok", quiet));
        let mut reports = parse(&output);
        reports[1].target = Some(String::from("Doc-tests victory_core"));
        let trouble = problems(&reports);
        assert!(trouble.is_empty(), "no doc examples is normal: {trouble:?}");
    }

    #[test]
    fn an_unnamed_zero_report_is_not_called_a_filtered_one() {
        // Without a name the gate cannot tell a doc-test target from a
        // filtered one, and guessing would fail every doc-test crate in the
        // workspace. `run` refuses a run whose names do not pair before the
        // per-binary rules see it; the rule itself still must not guess.
        let quiet = "0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";
        let reports = parse(&format!(
            "{}{}",
            binary(2, "ok", OK2),
            binary(0, "ok", quiet)
        ));
        assert!(problems(&reports).is_empty(), "{:?}", problems(&reports));
    }

    #[test]
    fn each_binary_keeps_its_own_count() {
        let none = "0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out";
        let reports = parse(&format!(
            "{}{}",
            binary(2, "ok", OK2),
            binary(0, "ok", none)
        ));
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].passed, 2);
        assert_eq!(reports[1].announced, 0, "the second keeps its own count");
    }

    #[test]
    fn cargo_names_each_binary_on_stderr() {
        let progress = "   Compiling xtask v0.1.0\r\n    Finished `test` profile\r\n     \
                        Running unittests src\\main.rs (target\\debug\\deps\\xtask.exe)\r\n\
                        Doc-tests victory_core\r\n";
        assert_eq!(
            targets(progress),
            vec!["unittests src\\main.rs", "Doc-tests victory_core"]
        );
    }

    /// CI sets `CARGO_TERM_COLOR=always`, so cargo wraps `Running` and
    /// `Doc-tests` in escape codes and the name starts with ESC. The first
    /// push of the strict pairing rule read that as zero targets and refused
    /// a green workspace run; this is the exact line shape from run
    /// 45819533685.
    #[test]
    fn colored_cargo_progress_still_names_its_targets() {
        let progress = "\u{1b}[1m\u{1b}[92m     Running\u{1b}[0m unittests src\\lib.rs \
                        (target\\debug\\deps\\x.exe)\r\n\
                        \u{1b}[1m\u{1b}[92m   Doc-tests\u{1b}[0m victory_core\r\n";
        assert_eq!(
            targets(progress),
            vec!["unittests src\\lib.rs", "Doc-tests victory_core"]
        );
    }

    /// The harness's own lines can carry color too; a stripped line must
    /// still count.
    #[test]
    fn colored_harness_lines_still_parse() {
        let output = "\u{1b}[32mrunning 1 test\u{1b}[0m\n\
                      \u{1b}[32mtest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; \
                      0 filtered out\u{1b}[0m; finished in 0.01s\n";
        let reports = parse(output);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].counted(), 1);
    }

    #[test]
    fn a_failing_test_is_named() {
        let output = "running 2 tests\ntest a ... ok\ntest b ... FAILED\n";
        assert_eq!(failures(output), vec!["test b ... FAILED"]);
        assert!(failures(&binary(2, "ok", OK2)).is_empty());
    }

    #[test]
    fn an_empty_run_is_reported_not_passed() {
        let trouble = problems(&[]);
        assert_eq!(trouble.len(), 1, "{trouble:?}");
        assert!(trouble[0].contains("executed no test"), "{trouble:?}");
    }

    /// A red run has to say why. libtest prints the panic inside a
    /// `---- <name> stdout ----` block; the name list alone is not actionable,
    /// and on CI this is the only copy of that text.
    #[test]
    fn a_red_run_carries_the_panic_that_caused_it() {
        let output = "\
running 1 test
test frame::tests::waits_end_on_ticks_and_count_frames ... FAILED

failures:

---- frame::tests::waits_end_on_ticks_and_count_frames stdout ----
thread 'frame::tests::waits_end_on_ticks_and_count_frames' panicked at crates\\victory-gpu\\src\\frame.rs:243:9:
assertion failed: before.elapsed() < Duration::from_millis(10)
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    frame::tests::waits_end_on_ticks_and_count_frames

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s
";
        let why = details(output);
        assert!(why.contains("assertion failed"), "{why}");
        assert!(why.contains("frame.rs:243"), "{why}");
        assert!(
            !why.contains("test result:"),
            "the block ends at the name list: {why}"
        );
    }

    /// A run that fell over before any test started has no blocks; the
    /// caller's own `error` lines carry that, so this must not invent one.
    #[test]
    fn a_red_run_with_no_block_yields_nothing() {
        assert_eq!(details("error[E0308]: mismatched types\n"), "");
    }

    /// The stream `victory-hle` really produces: one test spawns
    /// `current_exe()` with inherited stdout, so a 1-test child announces
    /// inside the parent binary's run and reports first. Attaching each
    /// result to the latest announcement gave the child the parent's 152
    /// counts and left the parent accounting for nothing.
    #[test]
    fn a_child_process_report_keeps_its_parents_counts() {
        let output = "\
running 152 tests
test host::imp::tests::loading_a_host_dll_does_not_block_thread_attach ... ok
running 1 test
test host::imp::tests::loading_a_host_dll_does_not_block_thread_attach ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 151 filtered out; finished in 0.52s
test result: ok. 152 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.23s
";
        let reports = parse(output);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].depth, 0, "the binary cargo ran");
        assert_eq!(reports[0].announced, 152);
        assert_eq!(reports[0].counted(), 152, "the parent keeps its own counts");
        assert_eq!(reports[1].depth, 1, "the child the test spawned");
        assert_eq!(reports[1].counted(), 1);
        let top: Vec<Report> = reports.into_iter().filter(|r| r.depth == 0).collect();
        assert!(problems(&top).is_empty(), "{:?}", problems(&top));
    }

    /// Flags after `--` belong to libtest, which refuses cargo's own. Written
    /// at the end they reached it: `error: Unrecognized option:
    /// 'no-fail-fast'`, exit 101, and nothing ran.
    #[test]
    fn the_gates_flags_go_before_the_harness_separator() {
        let given: Vec<String> = ["-p", "xtask", "--", "no_such_test"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            argv(&given).expect("no job count given"),
            [
                "test",
                "-p",
                "xtask",
                "--no-fail-fast",
                "--jobs",
                "1",
                "--",
                "no_such_test"
            ]
            .map(String::from)
            .to_vec()
        );
        // A `-j` after the separator belongs to libtest, not to cargo: it must
        // pass through rather than be refused as the gate's own flag.
        let harness_jobs: Vec<String> = ["-p", "xtask", "--", "-j4"].map(String::from).to_vec();
        assert_eq!(
            argv(&harness_jobs).expect("a harness -j is not the gate's"),
            [
                "test",
                "-p",
                "xtask",
                "--no-fail-fast",
                "--jobs",
                "1",
                "--",
                "-j4"
            ]
            .map(String::from)
            .to_vec()
        );
    }

    /// `--jobs 1` is the gate's own flag: with binaries running concurrently
    /// into one stream no report can be attributed, so a caller's `-j`/`--jobs`
    /// would void the reading. Refused rather than honoured (which breaks the
    /// gate) or silently overridden (which ignores what was asked).
    #[test]
    fn a_caller_jobs_flag_is_refused() {
        for given in [
            vec!["-p", "xtask", "--jobs", "4"],
            vec!["-p", "xtask", "--jobs=4"],
            vec!["-p", "xtask", "-j", "4"],
            vec!["-p", "xtask", "-j4"],
        ] {
            let given: Vec<String> = given.into_iter().map(String::from).collect();
            let error = argv(&given).expect_err("a caller job count is refused");
            assert!(
                error.to_string().contains("one test binary at a time"),
                "{given:?}: {error}"
            );
        }
    }

    #[test]
    fn flags_the_caller_gave_are_not_duplicated() {
        let given: Vec<String> = ["-p", "xtask", "--no-fail-fast"].map(String::from).to_vec();
        let out = argv(&given).expect("no job count to refuse");
        assert_eq!(
            out.iter()
                .filter(|a| a.as_str() == "--no-fail-fast")
                .count(),
            1,
            "given once by the caller, not added again: {out:?}"
        );
        assert_eq!(
            out.iter().filter(|a| a.as_str() == "--jobs").count(),
            1,
            "the gate sets its own flag once: {out:?}"
        );
        assert!(
            out.windows(2).any(|w| w[0] == "--jobs" && w[1] == "1"),
            "--jobs 1 is forced: {out:?}"
        );
    }

    /// The child's chunks are echoed as they arrive and kept for the checks.
    /// The hang this streaming exists to survive is not reproducible in a unit
    /// test; this pins that both halves of the copy happen, and that CRLF
    /// lines still parse out of the captured bytes.
    #[test]
    fn a_chunk_is_echoed_and_captured() {
        let fixture = binary(
            1,
            "ok",
            "1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
        )
        .replace('\n', "\r\n");
        let mut echoed = Vec::new();
        let kept = copy(std::io::Cursor::new(fixture.as_bytes()), &mut echoed)
            .expect("copying the child's stream");
        assert_eq!(kept, echoed, "every byte read was also echoed");
        let text = String::from_utf8_lossy(&kept);
        let parsed = parse(&text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].counted(), 1, "CRLF lines still parse: {text:?}");
    }

    /// A build error is not a test failure, and the line that locates it does
    /// not begin with `error`. Echoing only `error`-prefixed lines printed
    /// the message and hid the file, line and caret.
    #[test]
    fn a_build_error_keeps_the_line_that_names_the_source() {
        let stderr = "\
error[E0308]: mismatched types
  --> xtask\\src\\main.rs:54:9
   |
54 |         Cmd::Workflows => workflows::run(&root),
   |         ^^^^^^^^^^^^^^ expected unit variant
   |
error: could not compile `xtask` (bin \"xtask\") due to 2 previous errors
";
        let report = failure_report("", stderr);
        assert!(report.contains("--> xtask\\src\\main.rs:54:9"), "{report}");
        assert!(report.contains("E0308"), "{report}");
    }

    /// A long transcript must not bury the error that caused it. The report
    /// used to print the last 60 stderr lines, so an error at the top of a
    /// build log was dropped along with its `-->` location and caret span.
    #[test]
    fn a_long_red_run_keeps_the_first_error_and_its_location() {
        let mut stderr = String::from(
            "error[E0308]: mismatched types
  --> xtask\\src\\main.rs:54:9
   |
54 |         Cmd::Workflows => workflows::run(&root),
   |         ^^^^^^^^^^^^^^ expected unit variant
   |
",
        );
        // More than the old 60-line tail, so the tail alone cannot hold it.
        for i in 0..80 {
            stderr.push_str(&format!("   Compiling filler-{i} v0.1.0\n"));
        }
        let report = failure_report("", &stderr);
        assert!(report.contains("E0308"), "{report}");
        assert!(report.contains("--> xtask\\src\\main.rs:54:9"), "{report}");
        assert!(report.contains("^^^^^^^^^^^^^^"), "{report}");
        assert!(
            report.lines().count() < 60,
            "an excerpt, not the transcript: {report}"
        );
    }

    /// The same guarantee under CI's colour setting: `CARGO_TERM_COLOR=always`
    /// makes every `error` line start with an escape byte, so a raw prefix
    /// match found no block and fell back to the tail.
    #[test]
    fn a_colored_red_run_keeps_the_first_error_and_its_location() {
        let mut stderr = String::from(
            "\u{1b}[1m\u{1b}[91merror[E0308]\u{1b}[0m: mismatched types
  --> xtask\\src\\main.rs:54:9
   |
54 |         Cmd::Workflows => workflows::run(&root),
   |         ^^^^^^^^^^^^^^ expected unit variant
   |
",
        );
        for i in 0..80 {
            stderr.push_str(&format!("   Compiling filler-{i} v0.1.0\n"));
        }
        let report = failure_report("", &stderr);
        assert!(report.contains("E0308"), "{report}");
        assert!(report.contains("--> xtask\\src\\main.rs:54:9"), "{report}");
        assert!(report.contains("^^^^^^^^^^^^^^"), "{report}");
        assert!(
            report.lines().count() < 60,
            "an excerpt, not the transcript: {report}"
        );
    }

    /// Cargo's own `error:` line keeps its place when it is coloured too.
    #[test]
    fn a_colored_cargo_error_line_survives_the_failure_report() {
        let stdout = "\
running 1 test
test a ... FAILED

failures:

---- a stdout ----
thread 'a' panicked at src\\lib.rs:10:5:
assertion failed
";
        let stderr = "\u{1b}[1m\u{1b}[91merror\u{1b}[0m: test failed, to rerun pass `-p x`\n";
        let report = failure_report(stdout, stderr);
        assert!(report.contains("error: test failed"), "{report}");
    }

    /// The fallback: a failure with no `error` line at all still reports the
    /// end of the stream, where a tool that prints no diagnostics puts its
    /// reason.
    #[test]
    fn a_red_run_without_an_error_line_keeps_the_tail() {
        let stderr = "   Compiling victory-core v0.1.0\nnote: the real reason is here\n";
        let report = failure_report("", stderr);
        assert!(report.contains("the real reason is here"), "{report}");
    }

    #[test]
    fn a_test_failure_names_the_test_and_its_panic() {
        let stdout = "\
running 1 test
test a ... FAILED

failures:

---- a stdout ----
thread 'a' panicked at src\\lib.rs:10:5:
assertion failed
";
        let report = failure_report(stdout, "error: test failed, to rerun pass `-p x`\n");
        assert!(report.contains("1 test(s) reported FAILED:"), "{report}");
        assert!(report.contains("test a ... FAILED"), "{report}");
        assert!(report.contains("assertion failed"), "{report}");
        assert!(report.contains("error: test failed"), "{report}");
    }
}
