//! Shaders and pipelines made before the title starts, so that it neither
//! waits for translations nor drops draws while pipelines are made.
//!
//! `run --execute` does what `translate --cache --pipelines` does, on the
//! title's shader files (its `shaders.rpf`, its executable and its DLLs),
//! before it starts the title, with a progress window
//! ([`crate::splash`]): shaders first, then the pipelines of the title's
//! shader pack. Both are kept on disk (the shader cache and the pipeline
//! library in the cache folder), so this is work once; a stamp file says
//! what is done for which files, translator and GPU, and a start with
//! nothing new skips it all in a few milliseconds.
//!
//! `VICTORY_PREBUILD` chooses how long the title waits:
//! - `shaders` (the default): for the shaders (minutes, once per
//!   translator); pipelines are made meanwhile on half the processor's
//!   threads while the title runs, their progress in the window caption;
//! - `all`: for the pipelines too (about 20 minutes on 16 threads; hours
//!   after a translator change that touches most programs);
//! - `off`: for nothing; nothing is made ahead.
//!
//! Esc (or closing the progress window) starts the title at once; what is
//! not done yet goes on behind it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;

/// What the title waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Shaders,
    All,
}

impl Mode {
    pub(crate) fn from_env(value: Option<&str>) -> Mode {
        match value.map(str::trim) {
            Some("off" | "0") => Mode::Off,
            Some("all" | "wait") => Mode::All,
            _ => Mode::Shaders,
        }
    }
}

/// What a stamp records as done: the shaders' and the pipelines'
/// fingerprints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) shaders: String,
    pub(crate) pipelines: String,
}

impl Stamp {
    pub(crate) fn parse(text: &str) -> Stamp {
        let mut stamp = Stamp::default();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("shaders", v)) => stamp.shaders = v.trim().to_owned(),
                Some(("pipelines", v)) => stamp.pipelines = v.trim().to_owned(),
                _ => {}
            }
        }
        stamp
    }

    pub(crate) fn text(&self) -> String {
        format!("shaders {}\npipelines {}\n", self.shaders, self.pipelines)
    }
}

/// A fingerprint of `files` (each one's path, size and time written) and
/// of `parts`: what a translation of them depends on.
pub(crate) fn fingerprint(files: &[PathBuf], parts: &[&str]) -> String {
    let mut h = blake3::Hasher::new();
    for f in files {
        h.update(f.to_string_lossy().as_bytes());
        let meta = std::fs::metadata(f).ok();
        let len = meta.as_ref().map_or(0, std::fs::Metadata::len);
        let written = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        h.update(&len.to_le_bytes());
        h.update(&written.to_le_bytes());
    }
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// The files the title's shaders are in: its shader archive, executable and
/// DLLs (those that exist).
pub(crate) fn shader_files(title_dir: &Path, exe: &Path, dlls: &[PathBuf]) -> Vec<PathBuf> {
    std::iter::once(title_dir.join("shaders.rpf"))
        .chain(std::iter::once(exe.to_owned()))
        .chain(dlls.iter().cloned())
        .filter(|p| p.is_file())
        .collect()
}

/// Whether the title can start: the work is over (done, failed or
/// panicked), or with [`Mode::Shaders`] the shaders are done.
pub(crate) fn ready(mode: Mode, finished: bool, shaders_done: bool) -> bool {
    finished || mode != Mode::All && shaders_done
}

/// Makes what is not made yet of the title's shaders and pipelines, waiting
/// as [`Mode`] says (with a progress window), and goes on with the rest
/// behind the title.
pub(crate) fn prepare(title_dir: &Path, files: Vec<PathBuf>) -> Result<()> {
    let mode = Mode::from_env(std::env::var("VICTORY_PREBUILD").ok().as_deref());
    if mode == Mode::Off || files.is_empty() {
        return Ok(());
    }
    let cfg = victory_core::Config::load()?;
    let shader_dir = victory_gpu::shader_cache::use_shader_cache(cfg.cache_subdir("shaders")?);
    let Some(shader_dir) = shader_dir else {
        tracing::warn!("startup: no shader cache, nothing is made ahead");
        return Ok(());
    };
    let shaders = fingerprint(&files, &[&shader_dir.to_string_lossy()]);
    let gpu = victory_gpu::adapter_identity().unwrap_or_default();
    let pipelines = fingerprint(&files, &[&shaders, &gpu]);
    let stamp_path = cfg.cache_subdir("prebuild")?.join(format!(
        "{}.txt",
        &blake3::hash(title_dir.to_string_lossy().as_bytes()).to_hex()[..16]
    ));
    let stamp = Stamp::parse(&std::fs::read_to_string(&stamp_path).unwrap_or_default());
    let (need_shaders, need_pipelines) = (stamp.shaders != shaders, stamp.pipelines != pipelines);
    if !need_shaders && !need_pipelines {
        println!("prepared       shaders and pipelines, made by an earlier start");
        return Ok(());
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let shaders_done = Arc::new(AtomicBool::new(!need_shaders));
    let started = std::time::Instant::now();
    let work = {
        let shaders_done = shaders_done.clone();
        let files = files.clone();
        std::thread::Builder::new()
            .name("victory prebuild".into())
            .spawn(move || {
                let mut done = stamp;
                let write = |done: &Stamp| {
                    if let Err(e) = std::fs::write(&stamp_path, done.text()) {
                        tracing::warn!(error = %e, "startup: the stamp was not written");
                    }
                };
                if need_shaders {
                    crate::progress::phase("Translating shaders");
                    match crate::translate::warm(files.clone(), Some(threads), false) {
                        Ok(()) => {
                            done.shaders = shaders;
                            write(&done);
                        }
                        Err(e) => crate::progress::say(format!("shaders        not made: {e:#}")),
                    }
                }
                shaders_done.store(true, Ordering::Release);
                if need_pipelines {
                    crate::progress::phase("Building pipelines");
                    // Half the threads once the title may be running.
                    let n = if mode == Mode::All {
                        threads
                    } else {
                        (threads / 2).max(1)
                    };
                    match crate::prebuild::run(&files, n) {
                        Ok(()) => {
                            done.pipelines = pipelines;
                            write(&done);
                        }
                        Err(e) => crate::progress::say(format!("pipelines      not made: {e:#}")),
                    }
                }
            })?
    };
    let waited = |work: &std::thread::JoinHandle<()>| {
        ready(
            mode,
            work.is_finished(),
            shaders_done.load(Ordering::Acquire),
        )
    };
    if !waited(&work) {
        #[cfg(windows)]
        let splash = crate::splash::show("VIctory: preparing shaders");
        while !waited(&work) {
            #[cfg(windows)]
            if crate::splash::skipped() {
                crate::progress::say("startup        Esc: the title starts now".into());
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        #[cfg(windows)]
        splash.close();
    }
    println!(
        "prepared       in {:.1} s before the title",
        started.elapsed().as_secs_f64()
    );
    if !work.is_finished() {
        // What goes on behind the title, in its window's caption.
        std::thread::Builder::new()
            .name("victory prebuild status".into())
            .spawn(move || {
                while !work.is_finished() {
                    let s = crate::progress::snapshot();
                    let status = format!("{}: {}", s.phase, s.line());
                    victory_gpu::set_caption_status(Some(status));
                    std::thread::sleep(Duration::from_secs(1));
                }
                victory_gpu::set_caption_status(None);
                let _ = work.join();
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_is_shaders_unless_said() {
        assert_eq!(Mode::from_env(None), Mode::Shaders);
        assert_eq!(Mode::from_env(Some("off")), Mode::Off);
        assert_eq!(Mode::from_env(Some("0")), Mode::Off);
        assert_eq!(Mode::from_env(Some("all")), Mode::All);
        assert_eq!(Mode::from_env(Some(" wait ")), Mode::All);
        assert_eq!(Mode::from_env(Some("anything")), Mode::Shaders);
    }

    /// The title starts when the work is over however it ended (a worker
    /// that panicked before the shaders were done does not hold it
    /// forever), or once the shaders are done unless all is waited for.
    #[test]
    fn the_title_starts_when_the_work_is_over_or_its_shaders_are_done() {
        for mode in [Mode::Shaders, Mode::All] {
            assert!(ready(mode, true, false), "{mode:?}: the worker ended early");
            assert!(!ready(mode, false, false), "{mode:?}");
        }
        assert!(ready(Mode::Shaders, false, true));
        assert!(!ready(Mode::All, false, true));
    }

    #[test]
    fn a_stamp_reads_back_what_it_wrote() {
        let stamp = Stamp {
            shaders: "abc".into(),
            pipelines: "def".into(),
        };
        assert_eq!(Stamp::parse(&stamp.text()), stamp);
        assert_eq!(Stamp::parse(""), Stamp::default());
        assert_eq!(Stamp::parse("shaders x\nnoise\n").shaders, "x");
    }

    /// A fingerprint changes with a file's contents (its size or time), the
    /// list of files, and the parts (translator, GPU); not otherwise.
    #[test]
    fn a_fingerprint_changes_with_files_and_parts() {
        let dir = std::env::temp_dir().join(format!("victory-startup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.bin"), dir.join("b.bin"));
        std::fs::write(&a, b"one").unwrap();
        std::fs::write(&b, b"two").unwrap();
        let files = vec![a.clone(), b.clone()];
        let first = fingerprint(&files, &["translator", "gpu"]);
        assert_eq!(first, fingerprint(&files, &["translator", "gpu"]));
        assert_ne!(first, fingerprint(&files, &["translator", "another gpu"]));
        assert_ne!(first, fingerprint(&files[..1], &["translator", "gpu"]));
        std::fs::write(&a, b"one more").unwrap();
        assert_ne!(
            first,
            fingerprint(&files, &["translator", "gpu"]),
            "a changed file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shader files are the archive, the executable and the DLLs, those
    /// that exist, in that order.
    #[test]
    fn shader_files_are_the_archive_exe_and_dlls_that_exist() {
        let dir = std::env::temp_dir().join(format!("victory-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("title.exe");
        let dll = dir.join("one.dll");
        std::fs::write(&exe, b"x").unwrap();
        std::fs::write(&dll, b"x").unwrap();
        let missing = dir.join("missing.dll");
        assert_eq!(
            shader_files(&dir, &exe, &[dll.clone(), missing]),
            vec![exe.clone(), dll.clone()]
        );
        std::fs::write(dir.join("shaders.rpf"), b"x").unwrap();
        let first = shader_files(&dir, &exe, std::slice::from_ref(&dll))[0].clone();
        assert_eq!(first, dir.join("shaders.rpf"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
