//! Warm the shader cache before a title runs.
//!
//! Containers found on disk, and containers kept from earlier runs, are
//! translated with the same cache the title uses. Mesh shaders take
//! compute-to-vertex when this GPU has no mesh shaders, and the native mesh
//! path when it does. A container that does not translate is counted and the
//! walk continues. The driver's pipeline library is not written here: it is
//! tied to one adapter, and a pipeline still needs the title's graphics state.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};

use crate::shader::{MAX_FILE, sources};

#[derive(Default)]
struct Tally {
    containers: usize,
    cached: usize,
    translated: usize,
    mesh: usize,
    failed: usize,
    reasons: BTreeMap<String, usize>,
}

impl Tally {
    fn add(&mut self, from_cache: bool, mesh_from_cache: Option<bool>, error: Option<String>) {
        self.containers += 1;
        if from_cache {
            self.cached += 1;
        } else if error.is_none() {
            self.translated += 1;
        }
        if mesh_from_cache.is_some() {
            self.mesh += 1;
        }
        if let Some(error) = error {
            self.failed += 1;
            let reason = shorten(&error);
            *self.reasons.entry(reason).or_default() += 1;
        }
    }
}

fn shorten(error: &str) -> String {
    let flat: String = error.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(160).collect()
}

fn mesh_path(compute_to_vertex: bool) -> &'static str {
    if compute_to_vertex {
        "compute-to-vertex"
    } else {
        "mesh shaders"
    }
}

fn report(compute_to_vertex: bool, workers: usize, tally: &Tally) -> String {
    format!(
        "{}, {workers} workers: {} shaders, {} cached, {} translated, {} mesh compute, {} failed",
        mesh_path(compute_to_vertex),
        tally.containers,
        tally.cached,
        tally.translated,
        tally.mesh,
        tally.failed
    )
}

/// Translate shaders under `paths` (or the configured game directory) into
/// the cache. Saved containers from earlier runs are included.
pub fn run(paths: Vec<PathBuf>, jobs: Option<usize>) -> Result<()> {
    #[cfg(windows)]
    {
        warm_paths(paths, jobs)
    }
    #[cfg(not(windows))]
    {
        let _ = (paths, jobs);
        anyhow::bail!("shader warmup runs on Windows");
    }
}

/// Translate containers kept from earlier runs. Empty when none are saved.
pub fn warm_seen() -> String {
    #[cfg(windows)]
    {
        let files = victory_hle::seen_shader_files();
        if files.is_empty() {
            return "none saved yet; `victory shaders` translates them before a launch".into();
        }
        println!("shaders        warming {} saved shaders", files.len());
        match warm_saved(&files) {
            Ok(line) => line,
            Err(e) => format!("warmup skipped: {e:#}"),
        }
    }
    #[cfg(not(windows))]
    {
        String::new()
    }
}

#[cfg(windows)]
fn warm_saved(files: &[PathBuf]) -> Result<String> {
    let compute_to_vertex = victory_hle::compute_to_vertex();
    let workers = victory_hle::shader_workers(None);
    let tally = Mutex::new(Tally::default());
    let seen = Mutex::new(HashSet::new());
    warm_files(files, workers, compute_to_vertex, &tally, &seen)?;
    let tally = tally.into_inner().unwrap_or_else(|e| e.into_inner());
    Ok(report(compute_to_vertex, workers, &tally))
}

#[cfg(windows)]
fn warm_paths(paths: Vec<PathBuf>, jobs: Option<usize>) -> Result<()> {
    let cfg = victory_core::Config::load()?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let paths = crate::default_paths(paths)?;
    let compute_to_vertex = victory_hle::compute_to_vertex();
    let workers = victory_hle::shader_workers(jobs);
    println!(
        "shader warmup  {}, {workers} workers",
        mesh_path(compute_to_vertex)
    );
    let tally = Mutex::new(Tally::default());
    let seen = Mutex::new(HashSet::new());
    let saved = victory_hle::seen_shader_files();
    warm_files(&saved, workers, compute_to_vertex, &tally, &seen)?;
    for root in &paths {
        walk_root(root, workers, compute_to_vertex, &tally, &seen)?;
    }
    let tally = tally.into_inner().unwrap_or_else(|e| e.into_inner());
    println!("{}", report(compute_to_vertex, workers, &tally));
    let mut reasons: Vec<_> = tally.reasons.iter().collect();
    reasons.sort_by_key(|(reason, n)| (std::cmp::Reverse(**n), *reason));
    for (reason, n) in reasons.into_iter().take(8) {
        println!("  {n}  {reason}");
    }
    Ok(())
}

#[cfg(windows)]
fn walk_root(
    root: &Path,
    workers: usize,
    compute_to_vertex: bool,
    tally: &Mutex<Tally>,
    seen: &Mutex<HashSet<[u8; 32]>>,
) -> Result<()> {
    anyhow::ensure!(root.exists(), "no such path {}", root.display());
    let mut batch = Vec::new();
    let chunk = workers.max(1).saturating_mul(2);
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE {
            eprintln!("skipping large file {}", entry.path().display());
            continue;
        }
        let data = match std::fs::read(entry.path()) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("skipping {}: {e}", entry.path().display());
                continue;
            }
        };
        push_containers(
            &data,
            seen,
            &mut batch,
            chunk,
            workers,
            compute_to_vertex,
            tally,
        );
    }
    warm_batch(&batch, workers, compute_to_vertex, tally);
    Ok(())
}

#[cfg(windows)]
fn warm_files(
    files: &[PathBuf],
    workers: usize,
    compute_to_vertex: bool,
    tally: &Mutex<Tally>,
    seen: &Mutex<HashSet<[u8; 32]>>,
) -> Result<()> {
    let mut batch = Vec::new();
    let chunk = workers.max(1).saturating_mul(2);
    for path in files {
        if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > MAX_FILE {
            eprintln!("skipping large file {}", path.display());
            continue;
        }
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        push_containers(
            &data,
            seen,
            &mut batch,
            chunk,
            workers,
            compute_to_vertex,
            tally,
        );
    }
    warm_batch(&batch, workers, compute_to_vertex, tally);
    Ok(())
}

#[cfg(windows)]
fn push_containers(
    data: &[u8],
    seen: &Mutex<HashSet<[u8; 32]>>,
    batch: &mut Vec<Vec<u8>>,
    chunk: usize,
    workers: usize,
    compute_to_vertex: bool,
    tally: &Mutex<Tally>,
) {
    for source in sources(data) {
        let Some(bytes) = source.bytes() else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let hash = *blake3::hash(&bytes).as_bytes();
        let new = seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(hash);
        if !new {
            continue;
        }
        batch.push(bytes.into_owned());
        if batch.len() >= chunk {
            warm_batch(batch, workers, compute_to_vertex, tally);
            batch.clear();
        }
    }
}

#[cfg(windows)]
fn warm_batch(batch: &[Vec<u8>], workers: usize, compute_to_vertex: bool, tally: &Mutex<Tally>) {
    if batch.is_empty() {
        return;
    }
    let next = AtomicUsize::new(0);
    let threads = workers.max(1).min(batch.len());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(container) = batch.get(index) else {
                        return;
                    };
                    let warmth = victory_hle::warm_shader(container, compute_to_vertex);
                    let mut tally = tally.lock().unwrap_or_else(|e| e.into_inner());
                    tally.add(warmth.from_cache, warmth.mesh_from_cache, warmth.error);
                    if tally.containers.is_multiple_of(250) {
                        println!(
                            "shader warmup  {} shaders, {} cached, {} translated, {} failed",
                            tally.containers, tally.cached, tally.translated, tally.failed
                        );
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_report_names_the_gpu_path() {
        let tally = Tally {
            containers: 4,
            cached: 3,
            translated: 1,
            mesh: 2,
            failed: 0,
            reasons: BTreeMap::new(),
        };
        let line = report(true, 12, &tally);
        assert_eq!(
            line,
            "compute-to-vertex, 12 workers: 4 shaders, 3 cached, 1 translated, 2 mesh compute, 0 failed"
        );
        assert!(report(false, 4, &tally).starts_with("mesh shaders, 4 workers:"));
    }
}
