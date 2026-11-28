//! Comparable Windows title runs. Reports contain counters, timings and a crash RVA.
#[cfg(any(windows, test))]
mod report;
#[cfg(windows)]
mod windows;

use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Args {
    /// Local title executable to run.
    #[arg(long)]
    title: PathBuf,
    /// VIctory executable under test (defaults to the workspace release build).
    #[arg(long)]
    exe: Option<PathBuf>,
    #[arg(long, default_value = "series-x", value_parser = ["series-x", "series-s"])]
    console: String,
    /// Maximum duration in seconds.
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..=86400))]
    seconds: u64,
    /// Stop above this sampled process working set in GiB.
    #[arg(long, default_value_t = 8.0)]
    max_ram_gb: f64,
    /// Stop below this available system memory in GiB, also checked before launch.
    #[arg(long, default_value_t = 3.0)]
    min_sys_gb: f64,
    #[arg(long, default_value = "run")]
    label: String,
    /// Previous Markdown report to compare.
    #[arg(long)]
    baseline: Option<PathBuf>,
    /// Report directory (defaults to %LOCALAPPDATA%\victory\bench).
    #[arg(long)]
    output_directory: Option<PathBuf>,
}

#[cfg(not(windows))]
pub fn run(_: Args) -> anyhow::Result<()> {
    anyhow::bail!("the title benchmark requires Windows")
}

#[cfg(any(windows, test))]
fn default_exe(xtask: &std::path::Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    // cargo xtask runs <target directory>/<profile>/xtask.exe.
    let target = xtask
        .parent()
        .and_then(std::path::Path::parent)
        .context("locating Cargo's target directory from xtask")?;
    Ok(target.join("release/victory.exe"))
}

#[cfg(test)]
#[test]
fn finds_the_release_build_in_cargos_target_directory() {
    let target = PathBuf::from("custom-target");
    assert_eq!(
        default_exe(&target.join("debug/xtask.exe")).unwrap(),
        target.join("release/victory.exe")
    );
}

#[cfg(windows)]
pub fn run(args: Args) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use std::fs;
    use std::io::Write;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    ensure!(
        (0.000001..=1048576.).contains(&args.max_ram_gb)
            && (0.0..=1048576.).contains(&args.min_sys_gb),
        "invalid memory limits"
    );
    ensure!(
        !args.label.is_empty()
            && args.label.len() <= 120
            && !args.label.chars().any(|c| c.is_control() || c == '|'),
        "invalid label"
    );
    let directory = match &args.output_directory {
        Some(dir) => dir.clone(),
        None => PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unset")?)
            .join("victory")
            .join("bench"),
    };
    victory_core::ensure_outside_repo(&directory)?;
    let baseline = args
        .baseline
        .as_ref()
        .map(|p| report::baseline(&fs::read_to_string(p).context("reading baseline")?))
        .transpose()?;
    let exe = match &args.exe {
        Some(path) => path.clone(),
        None => default_exe(&std::env::current_exe()?)?,
    };
    ensure!(
        exe.is_file(),
        "VIctory executable missing: {}",
        exe.display()
    );
    ensure!(
        args.title.is_file(),
        "title executable missing: {}",
        args.title.display()
    );
    fs::create_dir_all(&directory)?;
    let mut command = Command::new(exe);
    command
        .arg("run")
        .arg(&args.title)
        .args(["--execute", "--console", &args.console])
        .env(
            "VICTORY_LOG",
            "off,victory_gpu::host=info,victory_gpu::resource=warn,victory_gpu::pipeline=warn",
        );
    let (report, stop) =
        windows::monitor(&mut command, args.seconds, args.max_ram_gb, args.min_sys_gb)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let file = directory.join(format!("report-{stamp}-{}.md", std::process::id()));
    let mut output = fs::File::create_new(&file)?;
    output.write_all(
        report
            .markdown(&args, &stop, baseline.as_deref())
            .as_bytes(),
    )?;
    println!("{}", file.display());
    Ok(())
}
