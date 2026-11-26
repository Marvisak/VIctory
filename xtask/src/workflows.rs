//! `cargo xtask workflows`: every file in `.github/workflows/` must pass the
//! shape checks this repository can make.
//!
//! GitHub reads the workflow files before it creates a job, so a file it
//! cannot read produces no run at all. Not a failing check — an absent one.
//! On 2026-11-26 `ci.yml` carried a step whose `-` sat two columns left of its
//! siblings: every other entry in the list started at column seven, that one
//! at five, and a block sequence is delimited only by its entries sharing a
//! column. There was no reading of the file, so the parser refused it — and
//! the edit was being made to the check that would have caught it.
//!
//! A parse alone would not have been enough. A `steps:` block at the wrong
//! depth is valid YAML that runs nothing, and so are the shapes below. These
//! checks cover what GitHub refuses and this file can see without its schema:
//! the file is one document, it has a trigger and jobs, each job either calls
//! a workflow or says where to run its steps, and each step does exactly one
//! of `uses` and `run`. An unknown key, a malformed expression or a `uses:`
//! that names nothing real still passes here; `actionlint` covers those, and
//! is not wired in.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlLoader};

/// What a scan read and what it found.
pub struct Checked {
    /// One entry per file read. Carried so "checked nothing" cannot be
    /// reported as "all fine": a directory that moved would otherwise pass.
    pub files: Vec<String>,
    pub problems: Vec<String>,
}

/// Read one workflow file and return every reason GitHub would refuse it.
///
/// Returned rather than printed so the checks can be tested against a document
/// directly, with no filesystem in the way.
pub fn problems(file: &str, source: &str) -> Vec<String> {
    let mut found = Vec::new();

    let docs = match YamlLoader::load_from_str(source) {
        Ok(docs) => docs,
        Err(e) => {
            found.push(format!("{file}: not valid YAML: {e}"));
            return found;
        }
    };

    let doc = match docs.as_slice() {
        [doc] => doc,
        [] => {
            found.push(format!("{file}: empty; a workflow is one document"));
            return found;
        }
        _ => {
            found.push(format!(
                "{file}: {} documents; a workflow file is exactly one",
                docs.len()
            ));
            return found;
        }
    };

    let Yaml::Hash(top) = doc else {
        found.push(format!("{file}: the document is not a mapping"));
        return found;
    };

    // `on` is the trigger list. yaml-rust2 0.13 keeps it the string `on`; a
    // loader that resolved YAML 1.1's `on` to the boolean `true` would fail
    // `a_loadable_workflow_is_accepted` rather than slip through here.
    match field(top, "on") {
        None => found.push(format!(
            "{file}: no `on:`; GitHub refuses a workflow with no trigger"
        )),
        // The key being there is not the question: `on:` with nothing after
        // it, `on: []` and `on: ''` are all a trigger that names no event, and
        // GitHub refuses each one rather than run anything.
        Some(trigger) if !names_an_event(trigger) => found.push(format!(
            "{file}: `on` names no event; GitHub refuses a workflow no event can trigger"
        )),
        Some(_) => {}
    }

    let jobs = match field(top, "jobs") {
        Some(Yaml::Hash(jobs)) if !jobs.is_empty() => jobs,
        Some(Yaml::Hash(_)) => {
            found.push(format!("{file}: `jobs` is empty"));
            return found;
        }
        Some(_) => {
            found.push(format!("{file}: `jobs` is not a mapping"));
            return found;
        }
        None => {
            found.push(format!("{file}: no `jobs:`"));
            return found;
        }
    };

    for (key, job) in jobs {
        let name = match key {
            Yaml::String(s) => s.clone(),
            other => format!("{other:?}"),
        };
        let Yaml::Hash(job) = job else {
            found.push(format!("{file}: job `{name}` is not a mapping"));
            continue;
        };
        let (calls, steps) = (field(job, "uses"), field(job, "steps"));
        // A job either calls another workflow or lists steps. GitHub refuses
        // one that does both, and the `uses` arm below would otherwise hide
        // the `steps` it cannot run.
        if calls.is_some() && steps.is_some() {
            found.push(format!(
                "{file}: job `{name}` has both `uses` and `steps`; it is either a call or a job"
            ));
            continue;
        }
        if let Some(calls) = calls {
            if !matches!(calls, Yaml::String(s) if !s.is_empty()) {
                found.push(format!(
                    "{file}: job `{name}`'s `uses` is not a workflow reference"
                ));
            }
            continue;
        }
        let steps = match steps {
            Some(steps) => steps,
            None => {
                found.push(format!(
                    "{file}: job `{name}` has neither `steps` nor `uses`"
                ));
                continue;
            }
        };
        // Without `runs-on` GitHub has nowhere to put the job and refuses the
        // file. It is the most common hand-written omission, and the key being
        // present is not the question: `runs-on:` alone parses as null, and
        // `runs-on: []` is a list of no runners; neither schedules anything.
        if !field(job, "runs-on").is_some_and(names_a_runner) {
            found.push(format!(
                "{file}: job `{name}` has steps but no `runs-on` naming a runner; GitHub will not schedule it"
            ));
        }
        match steps {
            Yaml::Array(steps) if !steps.is_empty() => {
                for (i, step) in steps.iter().enumerate() {
                    let Yaml::Hash(step) = step else {
                        found.push(format!(
                            "{file}: job `{name}` step {} is not a mapping",
                            i + 1
                        ));
                        continue;
                    };
                    let (uses, run) = (field(step, "uses"), field(step, "run"));
                    let is_ref = matches!(uses, Some(Yaml::String(s)) if !s.is_empty());
                    let is_cmd = matches!(run, Some(Yaml::String(s)) if !s.is_empty());
                    if uses.is_some() && run.is_some() {
                        found.push(format!(
                            "{file}: job `{name}` step {} has both `uses` and `run`; GitHub refuses one that does both",
                            i + 1
                        ));
                    } else if uses.is_some() && !is_ref {
                        // `- uses:` with nothing after it is a null, and a
                        // presence test would call that present.
                        found.push(format!(
                            "{file}: job `{name}` step {}'s `uses` is not a string",
                            i + 1
                        ));
                    } else if run.is_some() && !is_cmd {
                        found.push(format!(
                            "{file}: job `{name}` step {}'s `run` is not a string",
                            i + 1
                        ));
                    } else if uses.is_none() && run.is_none() {
                        found.push(format!(
                            "{file}: job `{name}` step {} has neither `uses` nor `run`",
                            i + 1
                        ));
                    }
                }
            }
            Yaml::Array(_) => {
                found.push(format!("{file}: job `{name}` has no steps"));
            }
            _ => {
                found.push(format!("{file}: job `{name}`'s `steps` is not a list"));
            }
        }
    }

    found
}

/// The workflow files in `root`, as GitHub sees them: the top level of
/// `.github/workflows/` only.
///
/// `staged` reads the blobs being committed instead of the files on disk, the
/// same way `policy --staged` does. An edit made after `git add` is not in
/// that commit, so checking the file beside it would answer a different
/// question.
pub fn scan(root: &Path, staged: bool) -> Result<Checked> {
    if staged {
        return scan_staged(root);
    }

    let dir = root.join(".github").join("workflows");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Checked {
                files: Vec::new(),
                problems: Vec::new(),
            });
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    let mut checked = Checked {
        files: Vec::new(),
        problems: Vec::new(),
    };
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        // Subdirectories are not workflows to GitHub, so they are not read.
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_workflow(name) {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        checked.files.push(name.to_owned());
        checked.problems.extend(problems(name, &source));
    }
    checked.files.sort();
    checked.problems.sort();
    Ok(checked)
}

fn scan_staged(root: &Path) -> Result<Checked> {
    let listed = Command::new("git")
        .current_dir(root)
        .args([
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            // Without `-z` git quotes a name it cannot print plainly, and
            // `café.yml` arrives as `"caf\303\251.yml"` — a leading quote that
            // no prefix match survives, so the file went unchecked. `-z` also
            // makes the names NUL-separated, which is the split below.
            "-z",
            "--",
            ".github/workflows",
        ])
        .output()
        .context("listing the staged workflow files")?;
    if !listed.status.success() {
        bail!(
            "git diff --cached failed: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }

    let mut checked = Checked {
        files: Vec::new(),
        problems: Vec::new(),
    };
    for raw in listed.stdout.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        // A name this check cannot even spell is still a name in the commit.
        // Skipping it would leave a workflow file unchecked with no sign that
        // it was.
        let Ok(path) = std::str::from_utf8(raw) else {
            checked.problems.push(format!(
                "{}: a staged path under .github/workflows is not valid UTF-8, so this check cannot read it",
                String::from_utf8_lossy(raw)
            ));
            continue;
        };
        // The pathspec is `.github/workflows`, so every name listed is under
        // it. One that is not is a shape this check does not understand, and
        // saying nothing would report a pass it did not earn.
        let Some(name) = path.strip_prefix(".github/workflows/") else {
            checked.problems.push(format!(
                "{path}: staged under .github/workflows but not a path this check can read"
            ));
            continue;
        };
        // GitHub reads the top level only, and only the two workflow
        // extensions; a subdirectory or another file is not a workflow.
        if name.is_empty() || name.contains('/') || !is_workflow(name) {
            continue;
        }
        // The blob in the index: the file on disk may have been edited since
        // it was staged, and that edit is not in this commit.
        let blob = Command::new("git")
            .current_dir(root)
            .args(["show", &format!(":{path}")])
            .output()
            .with_context(|| format!("reading staged {path}"))?;
        if !blob.status.success() {
            bail!(
                "git show :{path} failed: {}",
                String::from_utf8_lossy(&blob.stderr).trim()
            );
        }
        let source = String::from_utf8_lossy(&blob.stdout).into_owned();
        checked.files.push(name.to_owned());
        checked.problems.extend(problems(name, &source));
    }
    checked.files.sort();
    checked.problems.sort();
    Ok(checked)
}

pub fn run(root: &Path, staged: bool) -> Result<()> {
    let checked = scan(root, staged)?;
    if checked.files.is_empty() {
        // Nothing staged is an ordinary commit that touches no workflow. A
        // full scan that reads nothing means the directory moved, and saying
        // "all fine" there is the defect this command exists to catch.
        if staged {
            println!("workflows: none staged");
            return Ok(());
        }
        bail!("no workflow files under .github/workflows/; this check read nothing");
    }
    if checked.problems.is_empty() {
        println!(
            "workflows: {} file(s) checked, each one passes the shape checks this repository can make",
            checked.files.len()
        );
        return Ok(());
    }
    for line in &checked.problems {
        eprintln!("{line}");
    }
    bail!(
        "{} problem(s) in {} workflow file(s). GitHub reads these files before it creates a job, so a file it cannot load produces no run at all",
        checked.problems.len(),
        checked.files.len()
    )
}

fn is_workflow(name: &str) -> bool {
    name.ends_with(".yml") || name.ends_with(".yaml")
}

fn field<'a>(map: &'a Hash, name: &str) -> Option<&'a Yaml> {
    map.get(&Yaml::String(name.to_owned()))
}

/// Whether `on:` names at least one event. GitHub takes one event name, a
/// list of them, or a mapping of event to settings; none of those with nothing
/// in it triggers anything. `on:` with no value parses as null, not as the
/// string it was written as.
fn names_an_event(trigger: &Yaml) -> bool {
    match trigger {
        Yaml::String(event) => !event.is_empty(),
        // Every entry has to be an event name, not just some entry: `on: [42]`
        // is a non-empty list GitHub refuses.
        Yaml::Array(events) => {
            !events.is_empty()
                && events
                    .iter()
                    .all(|event| matches!(event, Yaml::String(s) if !s.is_empty()))
        }
        Yaml::Hash(events) => !events.is_empty(),
        _ => false,
    }
}

/// One label or a list of them; the value `labels:` takes.
fn names_labels(labels: &Yaml) -> bool {
    match labels {
        Yaml::String(label) => !label.is_empty(),
        Yaml::Array(labels) => {
            !labels.is_empty()
                && labels
                    .iter()
                    .all(|label| matches!(label, Yaml::String(s) if !s.is_empty()))
        }
        _ => false,
    }
}

/// Whether `runs-on:` names a runner. GitHub takes one label, a list of them,
/// or a group/labels mapping; an empty list or an empty mapping names none.
/// A mapping with any key other than `group`/`labels`, or with a value of the
/// wrong type, is a file GitHub refuses.
fn names_a_runner(runs_on: &Yaml) -> bool {
    match runs_on {
        Yaml::String(label) => !label.is_empty(),
        Yaml::Array(_) => names_labels(runs_on),
        Yaml::Hash(fields) => {
            !fields.is_empty()
                && fields.iter().all(|(key, value)| match key {
                    Yaml::String(key) if key == "group" => {
                        matches!(value, Yaml::String(s) if !s.is_empty())
                    }
                    Yaml::String(key) if key == "labels" => names_labels(value),
                    _ => false,
                })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `steps:` block as it stood in `ci.yml` on 2026-11-26, before the
    /// fix: the new step's `-` at column five and its `run:` at six, while
    /// every sibling entry starts at column seven. Kept verbatim, so the guard
    /// is proved against the shape that actually shipped rather than one
    /// written to suit it.
    const STEP_INDENTED_SHORT: &str = "\
name: CI
on:
  pull_request:
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5
      # Runs tests and the gate in one pass.
    # and fails on a run that executed nothing.
    - name: Tests, gate
      run: cargo xtask test-gate --workspace --locked
";

    /// The same file with every step, and every key inside a step, on its
    /// siblings' column. This is the negative control: the check must accept
    /// it, or the red above proves only that the check refuses everything.
    const LOADABLE: &str = "\
name: CI
on:
  pull_request:
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5
      # Runs tests and the gate in one pass.
      # and fails on a run that executed nothing.
      - name: Tests, gate
        run: cargo xtask test-gate --workspace --locked
";

    #[test]
    fn a_step_indented_short_of_its_siblings_is_refused() {
        let found = problems("ci.yml", STEP_INDENTED_SHORT);
        assert!(
            found.iter().any(|p| p.contains("not valid YAML")),
            "the 2026-11-26 ci.yml was accepted: {found:#?}"
        );
    }

    #[test]
    fn a_loadable_workflow_is_accepted() {
        assert_eq!(problems("ci.yml", LOADABLE), Vec::<String>::new());
    }

    /// Valid YAML, and no job in it: `steps` sits beside `jobs` instead of
    /// inside one. This is the case a parse alone cannot see.
    #[test]
    fn steps_outside_a_job_are_refused() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    runs-on: windows-latest
steps:
  - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(
            found
                .iter()
                .any(|p| p.contains("job `check` has neither `steps` nor `uses`")),
            "{found:#?}"
        );
    }

    #[test]
    fn a_workflow_with_no_trigger_is_refused() {
        let source = "\
name: CI
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(found.iter().any(|p| p.contains("no `on:`")), "{found:#?}");
    }

    /// `on:` with nothing after it is the key with a null value. GitHub reads
    /// that as a trigger naming no event and refuses the file, so it produces
    /// no run at all. Asking only whether the key exists accepts it.
    #[test]
    fn a_null_trigger_is_refused() {
        let source = "\
name: CI
on:
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(
            found.iter().any(|p| p.contains("names no event")),
            "{found:#?}"
        );
    }

    #[test]
    fn an_empty_trigger_list_is_refused() {
        let source = "\
name: CI
on: []
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(
            found.iter().any(|p| p.contains("names no event")),
            "{found:#?}"
        );
    }

    #[test]
    fn a_trigger_written_as_an_empty_string_is_refused() {
        let source = "\
name: CI
on: ''
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(
            found.iter().any(|p| p.contains("names no event")),
            "{found:#?}"
        );
    }

    #[test]
    fn a_step_with_neither_uses_nor_run_is_refused() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    runs-on: windows-latest
    steps:
      - name: nothing happens
";
        let found = problems("ci.yml", source);
        assert!(
            found.iter().any(|p| p.contains("neither `uses` nor `run`")),
            "{found:#?}"
        );
    }

    /// A job that calls another workflow has no steps and is still valid.
    #[test]
    fn a_job_that_calls_another_workflow_is_accepted() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  call:
    uses: ./.github/workflows/other.yml
";
        assert_eq!(problems("ci.yml", source), Vec::<String>::new());
    }

    /// Valid YAML, valid-looking steps, and nowhere to run them.
    #[test]
    fn a_job_with_steps_and_no_runs_on_is_refused() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    steps:
      - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(
            found.iter().any(|p| p.contains("no `runs-on`")),
            "{found:#?}"
        );
    }

    /// `runs-on:` with nothing after it parses as null. The key is there and
    /// names no runner, so the job has nowhere to run.
    #[test]
    fn a_null_runs_on_is_refused() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    runs-on:
    steps:
      - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(
            found
                .iter()
                .any(|p| p.contains("no `runs-on` naming a runner")),
            "{found:#?}"
        );
    }

    #[test]
    fn an_empty_runs_on_list_is_refused() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    runs-on: []
    steps:
      - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(
            found
                .iter()
                .any(|p| p.contains("no `runs-on` naming a runner")),
            "{found:#?}"
        );
    }

    /// GitHub schedules a job on a runner group given as a mapping, so the
    /// shape check must not call that missing.
    #[test]
    fn a_runs_on_group_mapping_is_accepted() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    runs-on:
      group: my-group
      labels: [self-hosted]
    steps:
      - uses: actions/checkout@v5
";
        assert_eq!(problems("ci.yml", source), Vec::<String>::new());
    }

    /// A list is only a trigger list if every entry names an event. `on: [42]`
    /// and `on: [push, {}]` are non-empty lists GitHub still refuses.
    #[test]
    fn a_trigger_list_with_a_non_event_entry_is_refused() {
        for trigger in ["on: [42]", "on: [push, {}]", "on: [push, '']"] {
            let source = format!(
                "name: CI\n{trigger}\njobs:\n  check:\n    runs-on: windows-latest\n    steps:\n      - uses: actions/checkout@v5\n"
            );
            let found = problems("ci.yml", &source);
            assert!(
                found.iter().any(|p| p.contains("names no event")),
                "{trigger}: {found:#?}"
            );
        }
    }

    /// The runner-group mapping has exactly two keys, and each takes a string
    /// or a label list. An unknown key or a mis-typed value is a file GitHub
    /// refuses, not a runner.
    #[test]
    fn a_runner_mapping_with_an_unknown_key_or_wrong_type_is_refused() {
        for runs_on in [
            "runs-on: {group: 1}",
            "runs-on: {group: g, bogus: x}",
            "runs-on: {group: ''}",
            "runs-on: {labels: {a: b}}",
        ] {
            let source = format!(
                "name: CI\non: [pull_request]\njobs:\n  check:\n    {runs_on}\n    steps:\n      - uses: actions/checkout@v5\n"
            );
            let found = problems("ci.yml", &source);
            assert!(
                found
                    .iter()
                    .any(|p| p.contains("no `runs-on` naming a runner")),
                "{runs_on}: {found:#?}"
            );
        }
    }

    /// A job is a call or a job, never both. The `uses` arm would otherwise
    /// skip the steps it cannot run.
    #[test]
    fn a_job_that_both_calls_and_lists_steps_is_refused() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    uses: ./.github/workflows/other.yml
    steps:
      - uses: actions/checkout@v5
";
        let found = problems("ci.yml", source);
        assert!(
            found.iter().any(|p| p.contains("both `uses` and `steps`")),
            "{found:#?}"
        );
    }

    #[test]
    fn a_step_that_both_uses_and_runs_is_refused() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v5
        run: echo hi
";
        let found = problems("ci.yml", source);
        assert!(
            found.iter().any(|p| p.contains("both `uses` and `run`")),
            "{found:#?}"
        );
    }

    /// `- uses:` with nothing after it parses as a null, and GitHub refuses
    /// it. Asking only whether the key is there would call that present.
    #[test]
    fn a_step_with_a_null_uses_is_refused() {
        let source = "\
name: CI
on: [pull_request]
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses:
";
        let found = problems("ci.yml", source);
        assert!(
            found.iter().any(|p| p.contains("`uses` is not a string")),
            "{found:#?}"
        );
    }

    /// The guard that matters: the real tree. It walks the directory rather
    /// than naming `ci.yml`, so the next workflow file is covered without
    /// anyone remembering to add it.
    #[test]
    fn every_workflow_in_the_repository_loads() {
        let checked = scan(&crate::workspace_root(), false).expect("reading .github/workflows");
        assert!(!checked.files.is_empty(), "the scan read nothing");
        assert!(
            checked.problems.is_empty(),
            "{}",
            checked.problems.join("\n")
        );
    }

    /// A directory that moved would otherwise report success for having read
    /// nothing, which is the defect class this command exists to catch.
    #[test]
    fn a_tree_with_no_workflows_is_a_failure_not_a_pass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".github").join("workflows")).unwrap();
        let error = run(dir.path(), false).expect_err("an empty directory is not a pass");
        assert!(error.to_string().contains("read nothing"), "{error}");
    }

    /// Run git in `dir`, failing the test with git's stderr if it does not
    /// succeed.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("running git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The staged scan needs git. Where it is missing the hook that calls this
    /// could not run either, so the test says so by not running.
    fn have_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// The staged check reads the blob in the index, not the file beside it:
    /// an edit made after `git add` is not in that commit. This is the whole
    /// reason `--staged` exists.
    #[test]
    fn the_staged_check_reads_the_blob_not_the_worktree() {
        if !have_git() {
            return; // no git here; the hook that calls this needs it anyway
        }
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        let dir_workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&dir_workflows).unwrap();
        let path = dir_workflows.join("ci.yml");
        // A loadable file is staged ...
        std::fs::write(&path, LOADABLE).unwrap();
        git(dir.path(), &["add", ".github/workflows/ci.yml"]);
        // ... and then broken in the worktree only.
        std::fs::write(&path, STEP_INDENTED_SHORT).unwrap();

        let staged = scan(dir.path(), true).expect("reading staged workflows");
        assert_eq!(staged.files, vec![String::from("ci.yml")]);
        assert!(
            staged.problems.is_empty(),
            "the staged blob is the loadable one: {:#?}",
            staged.problems
        );

        let worktree = scan(dir.path(), false).expect("reading the worktree");
        assert!(
            !worktree.problems.is_empty(),
            "the worktree copy is broken, which is the difference"
        );
    }

    /// git quotes a name it will not print plainly, so a staged `café.yml`
    /// arrived as `"caf\303\251.yml"`: a leading quote that no prefix match
    /// survives. The loop then skipped it without a word, and a broken
    /// non-ASCII workflow was committed with the hook reporting success.
    #[test]
    fn a_staged_non_ascii_workflow_is_checked() {
        if !have_git() {
            return; // no git here; the hook that calls this needs it anyway
        }
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        let dir_workflows = dir.path().join(".github").join("workflows");
        std::fs::create_dir_all(&dir_workflows).unwrap();
        // One file the checks accept, and one broken file under a name git
        // has to quote.
        std::fs::write(dir_workflows.join("ci.yml"), LOADABLE).unwrap();
        std::fs::write(
            dir_workflows.join("café.yml"),
            "\
name: Café
on: [pull_request]
jobs:
  check:
    runs-on: windows-latest
    steps:
      - uses:
",
        )
        .unwrap();
        git(dir.path(), &["add", ".github/workflows"]);

        let staged = scan(dir.path(), true).expect("reading staged workflows");
        assert!(
            staged.files.iter().any(|file| file == "café.yml"),
            "the non-ASCII workflow was not read: {:?}",
            staged.files
        );
        assert!(
            staged
                .problems
                .iter()
                .any(|p| p.contains("café.yml") && p.contains("`uses` is not a string")),
            "the broken non-ASCII workflow was not reported: {:#?}",
            staged.problems
        );
    }
}
