//! Repository maintenance. Run with `cargo xtask <command>`.

mod policy;

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
    /// Type-check for the Windows target the crates whose Windows code
    /// builds on any host: the platform the whole project ships on, and
    /// the only one behind `#[cfg(windows)]` compiles for (victory-shader's
    /// GPU test harness among them). A plain `cargo check` from another
    /// host reaches none of that code, so its type errors first appear in
    /// CI; this finds them in one command. The crates left out
    /// (victory-gpu and everything that needs it) bind blake3's C code,
    /// which needs Windows itself or a Windows-capable C toolchain to
    /// compile, so they stay with CI. Needs the pinned target once:
    /// `rustup target add x86_64-pc-windows-msvc`.
    Check,
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
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
            println!("hooks installed: commits now run `cargo xtask policy --staged`");
            Ok(())
        }
        Cmd::Check => {
            let target = "x86_64-pc-windows-msvc";
            let status = Command::new("cargo")
                .args([
                    "check",
                    "--locked",
                    "--all-targets",
                    "--target",
                    target,
                    "-p",
                    "victory-core",
                    "-p",
                    "victory-pe",
                    "-p",
                    "victory-formats",
                    "-p",
                    "victory-shader",
                    "-p",
                    "xtask",
                ])
                .current_dir(&root)
                .status()
                .context("running cargo")?;
            anyhow::ensure!(
                status.success(),
                "cargo check for {target} failed: on a non-Windows host the target\n\
                 needs `rustup target add {target}` once (the toolchain pins it),\n\
                 and a type error found this way is a Windows-only one - invisible\n\
                 to a plain `cargo check` off Windows, and first seen in CI otherwise"
            );
            Ok(())
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
