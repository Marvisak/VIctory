//! Repository maintenance. Run with `cargo xtask <command>`.

mod gate;
mod policy;
mod workflows;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
    /// Run the tests, then fail unless the run executed what it announced
    /// (see `gate`).
    TestGate {
        /// Arguments for `cargo test`; everything after the subcommand.
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Fail unless every file under `.github/workflows/` passes the shape
    /// checks this repository can make, since a file GitHub cannot load
    /// produces no run at all (see `workflows`).
    Workflows {
        /// Check only the workflow files staged for commit (used by the
        /// pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::TestGate { args } => gate::run(&root, &args),
        Cmd::Workflows { staged } => workflows::run(&root, staged),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            // git skips a hook it is not allowed to run and says nothing
            // about it, so on a POSIX filesystem the config alone can leave
            // every commit unchecked.
            #[cfg(unix)]
            {
                let hook = root.join(".githooks").join("pre-commit");
                make_executable(&hook)?;
                println!("made {} executable", hook.display());
            }
            println!(
                "hooks installed: commits now run `cargo xtask policy --staged` and `cargo xtask workflows --staged`"
            );
            Ok(())
        }
    }
}

/// Let git run `path` as a hook: it only executes a file that carries an
/// execute bit, and a file copied or checked out without one is skipped in
/// silence.
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("making {} executable", path.display()))
}

pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell that can run the hook. The hook is a POSIX script, so where no
    /// such shell is reachable the test cannot exercise it and says so rather
    /// than passing on nothing it ran.
    fn shell() -> Option<&'static str> {
        ["sh", "bash"].into_iter().find(|shell| {
            Command::new(shell)
                .arg("-c")
                .arg("exit 0")
                .output()
                .is_ok_and(|out| out.status.success())
        })
    }

    /// PATH with every directory that holds a `cargo` removed, which is the
    /// machine the hook has to survive: git runs it there too.
    fn path_without_cargo() -> Option<std::ffi::OsString> {
        let kept: Vec<_> = std::env::split_paths(&std::env::var_os("PATH")?)
            .filter(|dir| {
                ["cargo", "cargo.exe", "cargo.cmd", "cargo.bat"]
                    .iter()
                    .all(|name| !dir.join(name).exists())
            })
            .collect();
        std::env::join_paths(kept).ok()
    }

    /// Without cargo on PATH the hook used to fail every commit with
    /// `cargo: command not found`, and nothing said that the checks had not
    /// run. It must step aside and say so, because a commit git refuses for a
    /// missing tool is worse than one that skipped a check it could not run.
    #[test]
    fn the_hook_skips_itself_when_cargo_is_missing() {
        let Some(shell) = shell() else {
            eprintln!("no POSIX shell on PATH; .githooks/pre-commit is a shell script");
            return;
        };
        let Some(path) = path_without_cargo() else {
            eprintln!("no PATH to remove cargo from");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let hook = workspace_root().join(".githooks").join("pre-commit");
        let out = Command::new(shell)
            .arg(&hook)
            .env("PATH", &path)
            .current_dir(dir.path())
            .output()
            .expect("running the hook");
        assert!(
            out.status.success(),
            "the hook failed instead of skipping without cargo: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("cargo not on PATH"),
            "the hook did not say why it did nothing: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// git runs a hook only if it is executable, and says nothing when it is
    /// not. Setting `core.hooksPath` must therefore leave the hook runnable.
    #[cfg(unix)]
    #[test]
    fn installing_the_hooks_makes_them_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\n").unwrap();
        let mut plain = std::fs::metadata(&hook).unwrap().permissions();
        plain.set_mode(0o644);
        std::fs::set_permissions(&hook, plain).unwrap();

        make_executable(&hook).unwrap();

        let mode = std::fs::metadata(&hook).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "mode {mode:o} has no execute bit");
    }

    /// The mode is a property of the commit, not of a Windows checkout: a
    /// split commit reset it once already (`7878c324`), and no Windows test
    /// can see it. Assert the tree git will hand a POSIX clone.
    #[test]
    fn the_hook_is_committed_executable() {
        let output = Command::new("git")
            .current_dir(workspace_root())
            .args(["ls-tree", "HEAD", ".githooks/pre-commit"])
            .output();
        let Ok(output) = output else {
            eprintln!("no git; the committed hook mode cannot be checked here");
            return;
        };
        if !output.status.success() {
            eprintln!("git ls-tree failed; the committed hook mode cannot be checked here");
            return;
        }
        let tree = String::from_utf8_lossy(&output.stdout);
        assert!(
            tree.starts_with("100755 "),
            "the hook is not committed executable: {tree}"
        );
    }
}
