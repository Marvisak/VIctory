//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with (made in time or
//!    not) are listed by a key known when the title creates them (their
//!    shader containers and graphics state); the next run hands those to
//!    the workers at creation, during loading. A full list keeps the
//!    latest run's first.
//!
//! 3. The pre-build (`victory translate --cache --pipelines`) lists the
//!    title pipelines it has put in the library, by the same key, so a
//!    second pre-build skips them without translating or loading anything.
//!    The list holds only for the library bytes it was saved with and for
//!    what names the pipelines (the translator, this module and `host`).
//!
//! The library is free-threaded but for loads of one pipeline on several
//! threads (`ID3D12Device1::CreatePipelineLibrary`, "Thread Safety"), so
//! pipelines of different names load at the same time. A load is not always
//! quick: the library of an AMD driver holds about 140 bytes per pipeline,
//! and what its own cache lacks is compiled inside the load, for seconds.
//! Loaded one at a time, 650 pipelines kept the eight workers to one core
//! for minutes, while each title frame could wait out its 500 ms for
//! them (`make`). Stores and serialising keep the lock: the size asked for
//! must still be right when the bytes are written. Loads take no lock at
//! all, since a load adds nothing to the library: a save holds the lock
//! while it serialises the whole library (hundreds of megabytes in the
//! world), and loads that waited on it stalled every worker for as long.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

mod mapped;
use mapped::MappedFile;

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on.
pub fn use_pipeline_cache(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    blob: BlobBacking,
    /// The file the blob was read from, for its digest's sidecar.
    source: Option<PathBuf>,
    /// Digest of the blob (empty for none), once `digest` was asked.
    digest: String,
    /// The generation the blob was read from (0 for the legacy file or none);
    /// a save writes the generation after the highest one seen.
    generation: u64,
    /// Pipelines stored since the last save.
    added: u64,
    /// Title pipelines the pre-build put in the library or found there, by
    /// key, and whether that list changed since the last save.
    prebuilt: HashSet<String>,
    prebuilt_changed: bool,
}

// SAFETY: D3D12 objects are free-threaded (a pipeline library with the
// exception `NAMES` takes care of).
unsafe impl Send for Library {}

/// The library for loads, beside the one stores and saves lock.
struct Shared {
    /// The same library as `state`'s, loaded from without its lock.
    loads: ID3D12PipelineLibrary1,
    state: Mutex<Library>,
}

// SAFETY: as for `Library`.
unsafe impl Send for Shared {}
// SAFETY: the library is free-threaded but for loads of one name, which
// `NAMES` keeps apart; the rest is behind `state`'s lock.
unsafe impl Sync for Shared {}

/// Names one thread at a time holds.
struct Names {
    held: Mutex<Option<HashSet<String>>>,
    released: Condvar,
}

/// A name held until this is dropped.
struct Held<'a> {
    names: &'a Names,
    name: &'a str,
}

impl Names {
    const fn new() -> Names {
        Names {
            held: Mutex::new(None),
            released: Condvar::new(),
        }
    }

    /// Holds `name`, once no other thread does.
    fn hold<'a>(&'a self, name: &'a str) -> Held<'a> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while held.as_ref().is_some_and(|h| h.contains(name)) {
            held = self.released.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        held.get_or_insert_default().insert(name.to_owned());
        Held { names: self, name }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let mut held = self.names.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = held.as_mut() {
            h.remove(self.name);
        }
        drop(held);
        self.names.released.notify_all();
    }
}

/// Pipelines being loaded, or made and stored.
static NAMES: Names = Names::new();

/// The library file earlier builds saved (generation 0).
const LIBRARY_FILE: &str = "host-pipelines.bin";
/// Each save is a new generation, `host-pipelines.gen-N.bin`.
const GENERATION_PREFIX: &str = "host-pipelines.gen-";
const EARLY_FILE: &str = "early-pipelines.txt";
const PREBUILT_FILE: &str = "prebuilt-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 8192;

fn library() -> Option<&'static Shared> {
    static LIBRARY: OnceLock<Option<Shared>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let dir = DIR.get();
            let mut library = match dir {
                Some(dir) => open_dir(&device, dir, mmap_enabled()),
                None => open(&device, BlobBacking::Owned(Vec::new())),
            };
            // The pre-build's list, if it was saved with these bytes. Only
            // here is the blob's digest needed: with a sidecar it costs no
            // page of a mapped library.
            if let (Some(l), Some(dir)) = (library.as_mut(), dir)
                && !l.blob.is_empty()
                && let Ok(list) = std::fs::read(dir.join(PREBUILT_FILE))
            {
                l.prebuilt = listed(&list, &tag(&l.digest(dir)));
            }
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            }
            library.map(|l| Shared { loads: l.library.clone(), state: Mutex::new(l) })
        })
        .as_ref()
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, blob: BlobBacking) -> Option<Library> {
    if !blob.is_empty() {
        match adopt(device, blob) {
            Ok(library) => return Some(library),
            Err(e) => tracing::info!(error = %e, "host pipeline library rejected; starting an empty one"),
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    empty(device)
}

/// A library with no pipelines.
fn empty(device: &ID3D12Device1) -> Option<Library> {
    Some(Library {
        library: create(device, &[]).ok()?,
        blob: BlobBacking::Owned(Vec::new()),
        source: None,
        digest: String::new(),
        generation: 0,
        added: 0,
        prebuilt: HashSet::new(),
        prebuilt_changed: false,
    })
}

fn create(device: &ID3D12Device1, bytes: &[u8]) -> windows::core::Result<ID3D12PipelineLibrary1> {
    // SAFETY: the callers keep the bytes beside the library, which they
    // outlive.
    unsafe { device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?.cast() }
}

/// The library made of `blob`, which it keeps for as long as it lives.
fn adopt(device: &ID3D12Device1, blob: BlobBacking) -> windows::core::Result<Library> {
    let started = std::time::Instant::now();
    let library = create(device, blob.as_slice())?;
    let ms = started.elapsed().as_millis() as u64;
    tracing::info!(bytes = blob.len(), mapped = blob.is_mapped(), ms, "host pipeline library loaded");
    Ok(Library {
        library,
        blob,
        source: None,
        digest: String::new(),
        generation: 0,
        added: 0,
        prebuilt: HashSet::new(),
        prebuilt_changed: false,
    })
}

/// The library saved in `dir`: the newest generation the host accepts (the
/// legacy file counts as generation 0), else an empty one. Generations the
/// host rejected, and older ones, are deleted (best effort).
fn open_dir(device: &ID3D12Device1, dir: &Path, mmap: bool) -> Option<Library> {
    let found = generations(dir);
    let highest = found.first().map_or(0, |&(n, _)| n);
    for (n, path) in &found {
        let Some(blob) = BlobBacking::read(path, mmap) else { continue };
        match adopt(device, blob) {
            Ok(mut library) => {
                // A save never reuses the number of a generation seen here.
                library.generation = highest;
                library.source = Some(path.clone());
                remove_other_generations(dir, *n);
                return Some(library);
            }
            Err(e) => tracing::info!(error = %e, file = %path.display(), "host pipeline library rejected"),
        }
    }
    if found.is_empty() {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    let mut library = empty(device)?;
    library.generation = highest;
    remove_other_generations(dir, u64::MAX);
    Some(library)
}

/// What the pre-build's list holds for: the library's bytes (by their
/// digest, so that a mapped library need not be read to know), and what
/// names its pipelines (the translation, as the shader cache keys it; the
/// host descriptions).
fn tag(digest: &str) -> String {
    let limit = victory_shader::emit::loop_limit().to_le_bytes();
    let translator = victory_shader::SOURCE_HASH.as_bytes();
    let mut h = blake3::Hasher::new();
    for part in [
        digest.as_bytes(),
        translator,
        include_bytes!("../shader_cache.rs"),
        &limit,
        include_bytes!("host.rs"),
        include_bytes!("library.rs"),
    ] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex().to_string()
}

impl Library {
    /// The blob's digest: from the sidecar a save left beside a mapped
    /// blob (no page of it is read), else hashed (and kept in a sidecar for
    /// the next run). An owned blob, read whole already, is hashed.
    fn digest(&mut self, dir: &Path) -> String {
        if self.digest.is_empty() && !self.blob.is_empty() {
            let sidecar = self.source.as_deref().map(sidecar).filter(|_| self.blob.is_mapped());
            let saved = sidecar.as_deref().and_then(|s| read_sidecar(s, self.blob.len()));
            self.digest = saved.unwrap_or_else(|| {
                let digest = digest_of(self.blob.as_slice());
                if let Some(sidecar) = &sidecar {
                    let _ = std::fs::create_dir_all(dir)
                        .and_then(|()| write_sidecar(sidecar, self.blob.len(), &digest));
                }
                digest
            });
        }
        self.digest.clone()
    }
}

/// The bytes a library was made from, which must outlive it: its file
/// mapped (the pages come from the file as the host reads them, and are
/// not committed memory), or read whole when that is not possible or
/// `VICTORY_PIPELINE_LIB_MMAP=0` says not to.
enum BlobBacking {
    Mapped(MappedFile),
    Owned(Vec<u8>),
}

impl BlobBacking {
    /// The file's bytes, or `None` if it cannot be read or holds none.
    fn read(path: &Path, mmap: bool) -> Option<BlobBacking> {
        if mmap {
            match MappedFile::open(path) {
                Ok(mapped) => return Some(BlobBacking::Mapped(mapped)),
                // An empty file is skipped below; others are read whole.
                Err(e) => tracing::debug!(error = %e, file = %path.display(), "not mapped; reading it"),
            }
        }
        std::fs::read(path).ok().filter(|b| !b.is_empty()).map(BlobBacking::Owned)
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            BlobBacking::Mapped(m) => m.as_slice(),
            BlobBacking::Owned(v) => v,
        }
    }

    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn is_mapped(&self) -> bool {
        matches!(self, BlobBacking::Mapped(_))
    }
}

/// Whether the library is mapped from its file (the default) and saved
/// through a mapping; `VICTORY_PIPELINE_LIB_MMAP=0` reads and writes whole
/// buffers instead.
fn mmap_enabled() -> bool {
    mmap_set(std::env::var_os("VICTORY_PIPELINE_LIB_MMAP").as_deref())
}

fn mmap_set(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_none_or(|v| v != "0")
}

fn digest_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The file beside `path` that keeps the digest of its bytes.
fn sidecar(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".b3");
    path.with_file_name(name)
}

/// The digest in `sidecar`, if it was made for a file of `len` bytes.
fn read_sidecar(sidecar: &Path, len: usize) -> Option<String> {
    let text = std::fs::read_to_string(sidecar).ok()?;
    let (saved_len, digest) = text.trim().split_once(' ')?;
    (saved_len.parse() == Ok(len) && digest.len() == 64).then(|| digest.to_owned())
}

fn write_sidecar(sidecar: &Path, len: usize, digest: &str) -> std::io::Result<()> {
    write_atomically(sidecar, format!("{len} {digest}").as_bytes())
}

/// The library's generations in `dir`, newest first: `host-pipelines.gen-N.bin`
/// is generation N, and the file earlier builds saved is generation 0.
fn generations(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let n = match name == LIBRARY_FILE {
                true => 0,
                false => name.strip_prefix(GENERATION_PREFIX)?.strip_suffix(".bin")?.parse().ok()?,
            };
            Some((n, entry.path()))
        })
        .collect();
    found.sort_unstable_by_key(|&(n, _)| std::cmp::Reverse(n));
    found
}

fn generation_file(n: u64) -> String {
    format!("{GENERATION_PREFIX}{n}.bin")
}

/// Deletes the generations matching `remove` (best effort: where Windows
/// refuses to delete the one a process has mapped, a sharing violation, it
/// stays for the next start).
fn remove_generations_where(dir: &Path, mut remove: impl FnMut(u64) -> bool) {
    for (n, path) in generations(dir).into_iter().filter(|&(n, _)| remove(n)) {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                let _ = std::fs::remove_file(sidecar(&path));
            }
            Err(e) => tracing::debug!(generation = n, error = %e, "old host pipeline library kept"),
        }
    }
}

/// Deletes every generation other than `keep` (used on open after falling
/// back past damaged newer generations).
fn remove_other_generations(dir: &Path, keep: u64) {
    remove_generations_where(dir, |n| n != keep);
}

/// Deletes generations strictly older than `keep` (used on save so a save
/// never deletes a newer generation).
fn remove_older_generations(dir: &Path, keep: u64) {
    remove_generations_where(dir, |n| n < keep);
}

/// Serialises `library` into the file of generation `generation` in `dir`
/// and returns its size and (if `digest`) its digest, also saved in a
/// sidecar. Windows does not let a file a process has mapped be written, so
/// a save makes a new generation, through a staging file renamed once
/// whole. With `mmap` the staging file is mapped and serialised into, so
/// no buffer of the library's size is made; without it (or if mapping
/// fails) a buffer is.
fn write_generation(
    dir: &Path,
    library: &ID3D12PipelineLibrary1,
    generation: u64,
    mmap: bool,
    digest: bool,
) -> std::io::Result<(usize, Option<String>)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(generation_file(generation));
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    let staging = path.with_file_name(name);
    // SAFETY: a plain size query on a live library.
    let size = unsafe { library.GetSerializedSize() };
    let staged = match mmap && size > 0 {
        true => serialize_mapped(library, &staging, size, digest).or_else(|e| {
            tracing::info!(error = %e, "host pipeline library not saved through a mapping");
            serialize_owned(library, &staging, size, digest)
        }),
        false => serialize_owned(library, &staging, size, digest),
    };
    let digest = staged
        .and_then(|digest| {
            // A sidecar left by an earlier file of this name no longer holds.
            let _ = std::fs::remove_file(sidecar(&path));
            std::fs::rename(&staging, &path).map(|()| digest)
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&staging);
        })?;
    if let Some(digest) = &digest {
        write_sidecar(&sidecar(&path), size, digest)?;
    }
    Ok((size, digest))
}

fn serialize_mapped(
    library: &ID3D12PipelineLibrary1,
    file: &Path,
    size: usize,
    digest: bool,
) -> std::io::Result<Option<String>> {
    let mut mapped = MappedFile::create(file, size)?;
    // SAFETY: a mapping of the size the library asks for.
    unsafe { library.Serialize(mapped.as_mut_slice()) }.map_err(std::io::Error::other)?;
    mapped.flush()?;
    Ok(digest.then(|| digest_of(mapped.as_slice())))
}

fn serialize_owned(
    library: &ID3D12PipelineLibrary1,
    file: &Path,
    size: usize,
    digest: bool,
) -> std::io::Result<Option<String>> {
    let Some(mut bytes) = zeroed(size) else {
        // Left for a later save: the pipelines stay counted as added.
        tracing::warn!(bytes = size, "no memory to serialise the host pipeline library; saved later");
        return Err(std::io::ErrorKind::OutOfMemory.into());
    };
    // SAFETY: a buffer of the size the library asks for.
    unsafe { library.Serialize(&mut bytes) }.map_err(std::io::Error::other)?;
    std::fs::write(file, &bytes)?;
    Ok(digest.then(|| digest_of(&bytes)))
}

/// The pre-build's list as saved: the tag it holds for, then its keys.
fn list(tag: &str, keys: &HashSet<String>) -> String {
    let keys = keys.iter().map(String::as_str);
    std::iter::once(tag).chain(keys).collect::<Vec<_>>().join("\n")
}

/// The keys of a saved list, if it holds for `tag` (else none).
fn listed(list: &[u8], tag: &str) -> HashSet<String> {
    let list = String::from_utf8_lossy(list);
    let mut lines = list.lines();
    match lines.next() == Some(tag) {
        true => lines.map(str::to_owned).collect(),
        false => HashSet::new(),
    }
}

/// Whether the pre-build has the title pipeline `key` in the library.
pub(crate) fn prebuilt(key: &str) -> bool {
    library().is_some_and(|l| l.state.lock().unwrap_or_else(|e| e.into_inner()).prebuilt.contains(key))
}

/// The pre-build has the title pipeline `key` in the library now.
pub(crate) fn note_prebuilt(key: String) {
    if let Some(l) = library() {
        let mut l = l.state.lock().unwrap_or_else(|e| e.into_inner());
        l.prebuilt_changed |= l.prebuilt.insert(key);
    }
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root].into_iter().chain(code.iter().copied()).chain([state.as_bytes()]) {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored. A
/// name asked for by several threads is loaded or made by one at a time
/// (the others then find it in the library); different names overlap.
pub(crate) fn pipeline(
    name: &str,
    load: impl FnOnce(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let wide = HSTRING::from(name);
    #[cfg(feature = "diagnostics")]
    LAST.set(Some((name.to_owned(), true)));
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    let _held = NAMES.hold(name);
    // Outside the lock: loads take up to seconds, and a save holds the lock
    // for as long as it serialises. The library's blob lives as long as it
    // does, which is never dropped.
    if let Ok(pso) = load(&library.loads, PCWSTR(wide.as_ptr())) {
        LOADED.fetch_add(1, Ordering::Relaxed);
        return Ok(pso);
    }
    // Compiled outside the lock: compiles take up to seconds.
    #[cfg(feature = "diagnostics")]
    LAST.set(Some((name.to_owned(), false)));
    let pso = create()?;
    MADE.fetch_add(1, Ordering::Relaxed);
    let mut l = library.state.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    if unsafe { l.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) }.is_ok() {
        l.added += 1;
    }
    Ok(pso)
}

#[cfg(feature = "diagnostics")]
thread_local! {
    /// This thread's last pipeline: its name, and whether it was loaded
    /// (else made), for the pipeline log (diagnostics).
    pub(crate) static LAST: std::cell::Cell<Option<(String, bool)>> =
        const { std::cell::Cell::new(None) };
}

/// Writes the library and the early list if pipelines were added since the
/// last save and `SAVE_EVERY` has passed (or `now`).
pub(crate) fn save(now: bool) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    // Serialises the full save sequence (generation write, metadata files,
    // old-generation cleanup) without holding `library.state` during post-
    // serialisation file updates.
    static SAVING: Mutex<()> = Mutex::new(());
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
    let (Some(dir), Some(library)) = (DIR.get(), library()) else { return };
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if !now && last.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let _saving = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let mmap = mmap_enabled();
    let (generation, bytes, prebuilt) = {
        let mut l = library.state.lock().unwrap_or_else(|e| e.into_inner());
        if l.added == 0 && !l.prebuilt_changed && !USED_CHANGED.swap(false, Ordering::Relaxed) {
            return;
        }
        let generation = l.generation + 1;
        // The digest is the pre-build's list's tag (needed only with one).
        let digest = !l.prebuilt.is_empty();
        match write_generation(dir, &l.library, generation, mmap, digest) {
            Ok((bytes, digest)) => {
                l.generation = generation;
                l.added = 0;
                l.prebuilt_changed = false;
                // Kept beside the bytes it holds for (also when a run adds
                // pipelines).
                let prebuilt = digest.map(|d| list(&tag(&d), &l.prebuilt));
                (generation, bytes, prebuilt)
            }
            Err(e) => {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                return;
            }
        }
    };
    let used = used().as_ref().map(Used::list).unwrap_or_default();
    let written = write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()).and_then(|()| {
        match &prebuilt {
            Some(list) => write_atomically(&dir.join(PREBUILT_FILE), list.as_bytes()),
            None => Ok(()),
        }
    });
    // The generation this process maps may stay (if Windows refuses to
    // delete it); the next start removes it. Only strictly older generations
    // are removed so a save can never delete a newer generation.
    remove_older_generations(dir, generation);
    let (created, distinct) = created();
    let (shaders_cached, shaders_translated) = crate::shader_cache::counts();
    match written {
        Ok(()) => tracing::info!(
            bytes,
            generation,
            early = used.len(),
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            created,
            distinct,
            shaders_cached,
            shaders_translated,
            "host pipeline library saved"
        ),
        Err(e) => tracing::warn!(error = %e, "host pipeline library not saved"),
    }
}

/// `n` zero bytes, or `None` when the host has no memory for them. The
/// library is about 1.7 GB on NVIDIA (each pipeline's machine code), and an
/// infallible allocation of that size aborted the whole process mid-run
/// ("memory allocation of 2650101862 bytes failed", RTX 4050 Laptop).
fn zeroed(n: usize) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.resize(n, 0);
    Some(v)
}

/// Writes `bytes` to a temporary file beside `path`, then renames it over
/// `path`: a reader never sees a part. Serialised so concurrent worker and
/// shutdown saves do not race on the same staging file or target rename.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::AtomicU64;
    static WRITING: Mutex<()> = Mutex::new(());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let _guard = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}-{n}", std::process::id()));
    let tmp = path.with_file_name(name);
    let res = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Title pipelines created, and their distinct keys.
static CREATED: Mutex<(u64, Option<HashSet<String>>)> = Mutex::new((0, None));

/// The title created a pipeline with `key`.
pub(crate) fn note_created(key: &str) {
    let mut created = CREATED.lock().unwrap_or_else(|e| e.into_inner());
    created.0 += 1;
    created.1.get_or_insert_with(HashSet::new).insert(key.to_owned());
}

/// Title pipelines created so far, and how many distinct ones.
fn created() -> (u64, usize) {
    let created = CREATED.lock().unwrap_or_else(|e| e.into_inner());
    (created.0, created.1.as_ref().map_or(0, HashSet::len))
}

/// Title pipeline keys drawn or dispatched with: in earlier runs (the
/// list as loaded), and in this one.
#[derive(Default)]
struct Used {
    /// Each earlier key with its place in the list as loaded: the list is
    /// kept most recent run first, so a lower place is a more recent run.
    before: HashMap<String, usize>,
    now: HashSet<String>,
}

impl Used {
    /// The list as saved: one key a line, most recent run first (a list
    /// in no order, as earlier builds saved it, reads as well).
    fn read(listed: &str) -> Used {
        let mut before = HashMap::new();
        for (place, key) in listed.lines().filter(|l| !l.is_empty()).enumerate() {
            before.entry(key.to_owned()).or_insert(place);
        }
        Used { before, now: HashSet::new() }
    }

    /// The list to keep: this run's keys, then earlier runs' others, most
    /// recent first, at most `EARLY_MAX` (the oldest go once it is full).
    fn list(&self) -> Vec<String> {
        let mut earlier: Vec<_> = self.before.iter().filter(|(k, _)| !self.now.contains(*k)).collect();
        earlier.sort_unstable_by_key(|&(_, &place)| place);
        let earlier = earlier.into_iter().map(|(k, _)| k);
        self.now.iter().chain(earlier).take(EARLY_MAX).cloned().collect()
    }
}

static USED: Mutex<Option<Used>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<Used>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let read = Used::read(&listed);
        tracing::info!(pipelines = read.before.len(), "title pipelines to start early");
        *used = Some(read);
    }
    used
}

/// Reads the early list now if not yet (a pre-build asks for no title
/// pipeline).
pub(crate) fn keep_early_list() {
    drop(used());
}

/// Whether the title pipeline with `key` was drawn with in an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|u| u.before.contains_key(key))
}

/// The title pipeline with `key` was drawn or dispatched with.
pub(crate) fn note_used(key: &str) {
    if used().get_or_insert_with(Used::default).now.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A size the host cannot give is refused, not fatal; others are zeros.
    #[test]
    fn a_save_buffer_the_host_cannot_give_is_refused() {
        assert!(zeroed(usize::MAX / 2).is_none());
        assert_eq!(zeroed(100).as_deref(), Some(&[0u8; 100][..]));
    }

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    /// This run's keys stay in the list; earlier runs' fill it up to the cap.
    #[test]
    fn the_early_list_keeps_this_run_first() {
        let keys = |r: std::ops::Range<usize>| r.map(|i| format!("{i:032x}")).collect::<HashSet<_>>();
        let before = keys(0..EARLY_MAX).into_iter().enumerate().map(|(i, k)| (k, i)).collect();
        let used = Used { before, now: keys(EARLY_MAX - 10..EARLY_MAX + 10) };
        let list = used.list();
        assert_eq!(list.len(), EARLY_MAX);
        assert!(used.now.iter().all(|k| list.contains(k)));
        assert_eq!(list.iter().collect::<HashSet<_>>().len(), EARLY_MAX, "no key twice");
    }

    /// A full list makes room by the oldest runs' keys, and keeps that
    /// order across saves.
    #[test]
    fn a_full_early_list_drops_the_oldest_runs_keys() {
        let key = |i: usize| format!("{i:032x}");
        // As saved: the last run's keys, then the run before's.
        let saved: Vec<String> = (0..EARLY_MAX).map(key).collect();
        let mut used = Used::read(&saved.join("\n"));
        used.now = (EARLY_MAX..EARLY_MAX + 10).map(key).collect();
        used.now.insert(key(EARLY_MAX - 1)); // the oldest key, used again
        let list = used.list();
        assert_eq!(list.len(), EARLY_MAX);
        assert!(used.now.iter().all(|k| list.contains(k)));
        // The ten that go are the oldest ones not used again.
        let dropped: HashSet<String> = (EARLY_MAX - 11..EARLY_MAX - 1).map(key).collect();
        assert!(list.iter().all(|k| !dropped.contains(k)));
        // Read back, this run's keys come first, then the earlier ones in order.
        let again = Used::read(&list.join("\n"));
        assert!(used.now.iter().all(|k| again.before[k] < used.now.len()));
        assert!(again.before[&key(0)] < again.before[&key(1)]);
    }

    /// The pre-build's list holds only for the library bytes it was
    /// saved with.
    #[test]
    fn the_prebuilt_list_holds_for_its_library_only() {
        let keys: HashSet<String> = ["a".repeat(32), "b".repeat(32)].into();
        let saved = list(&tag(&digest_of(b"library")), &keys);
        assert_eq!(listed(saved.as_bytes(), &tag(&digest_of(b"library"))), keys);
        assert!(listed(saved.as_bytes(), &tag(&digest_of(b"library2"))).is_empty());
        assert!(listed(b"", &tag(&digest_of(b"library"))).is_empty());
    }

    #[test]
    fn different_names_are_held_at_the_same_time() {
        static NAMES: Names = Names::new();
        // Each thread holds its name until every thread holds its own.
        let all = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for i in 0..8 {
                let all = &all;
                s.spawn(move || {
                    let name = format!("pipeline {i}");
                    let _held = NAMES.hold(&name);
                    all.wait();
                });
            }
        });
        assert!(NAMES.held.lock().unwrap().as_ref().unwrap().is_empty());
    }

    #[test]
    fn one_name_is_held_by_one_thread_at_a_time() {
        static NAMES: Names = Names::new();
        static INSIDE: AtomicU64 = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..500 {
                        let _held = NAMES.hold("pipeline");
                        assert_eq!(INSIDE.fetch_add(1, Ordering::SeqCst), 0);
                        std::hint::spin_loop();
                        INSIDE.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });
        // A holder that panics lets go of it.
        let panicked = std::thread::spawn(|| {
            let _held = NAMES.hold("pipeline");
            panic!("on purpose");
        });
        assert!(panicked.join().is_err());
        drop(NAMES.hold("pipeline"));
    }

    /// A library's pipelines load on many threads at once, each with its
    /// own description (a load with another's is refused), while others are
    /// stored and the library is serialised.
    #[test]
    fn pipelines_load_on_many_threads_at_once() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::core::s;
        const N: usize = 48;
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok())
        else {
            return; // no host GPU here
        };
        let shader = |i: usize| {
            let source = format!(
                "#define RS \"UAV(u0)\"\nRWByteAddressBuffer b : register(u0);\n\
                 [RootSignature(RS)] [numthreads(1, 1, 1)] void main() {{ b.Store(0, b.Load(4) * {i} + {i}); }}"
            );
            let mut blob = None;
            // SAFETY: the source outlives the call; the blob is read within it.
            unsafe {
                D3DCompile(
                    source.as_ptr().cast(),
                    source.len(),
                    None,
                    None,
                    None,
                    s!("main"),
                    s!("cs_5_1"),
                    0,
                    0,
                    &mut blob,
                    None,
                )
                .ok()?;
                let blob = blob?;
                Some(
                    std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast::<u8>(),
                        blob.GetBufferSize(),
                    )
                    .to_vec(),
                )
            }
        };
        let Some(codes) = (0..2 * N).map(shader).collect::<Option<Vec<_>>>() else {
            return; // no shader compiler here
        };
        // SAFETY: the code holds its root signature.
        let root: ID3D12RootSignature =
            unsafe { device.CreateRootSignature(0, &codes[0]) }.unwrap();
        struct Shared(ID3D12Device1, ID3D12RootSignature, ID3D12PipelineLibrary1);
        // SAFETY: D3D12 objects are free-threaded.
        unsafe impl Sync for Shared {}
        impl Shared {
            fn desc(&self, code: &[u8]) -> D3D12_COMPUTE_PIPELINE_STATE_DESC {
                D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    // SAFETY: a borrowed pointer the description never releases.
                    pRootSignature: unsafe { std::mem::transmute_copy(&self.1) },
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                }
            }
            fn store(&self, i: usize, code: &[u8]) {
                let name = HSTRING::from(format!("p{i}"));
                // SAFETY: the description points at live data.
                unsafe {
                    let pso: ID3D12PipelineState =
                        self.0.CreateComputePipelineState(&self.desc(code)).unwrap();
                    self.2.StorePipeline(&name, &pso).unwrap();
                }
            }
            fn load(&self, i: usize, code: &[u8]) -> windows::core::Result<ID3D12PipelineState> {
                let name = HSTRING::from(format!("p{i}"));
                // SAFETY: as above.
                unsafe { self.2.LoadComputePipeline(&name, &self.desc(code)) }
            }
            fn serialized(&self) -> Vec<u8> {
                // SAFETY: a buffer of the size asked for; nothing is stored
                // meanwhile.
                unsafe {
                    let mut blob = vec![0; self.2.GetSerializedSize()];
                    self.2.Serialize(&mut blob).unwrap();
                    blob
                }
            }
        }
        let first = open(&device, BlobBacking::Owned(Vec::new())).expect("an empty library");
        let made = Shared(device.clone(), root.clone(), first.library.clone());
        (0..N).for_each(|i| made.store(i, &codes[i]));
        let again = open(&device, BlobBacking::Owned(made.serialized())).expect("the library again");
        let shared = Shared(device, root, again.library.clone());
        let stores = Mutex::new(());
        std::thread::scope(|s| {
            for t in 0..8 {
                let (shared, codes, stores) = (&shared, &codes, &stores);
                s.spawn(move || {
                    for i in (t..N).step_by(8) {
                        shared.load(i, &codes[i]).expect("a stored pipeline");
                        assert!(
                            shared.load(i, &codes[(i + 1) % N]).is_err(),
                            "another description"
                        );
                        assert!(shared.load(N + i, &codes[N + i]).is_err(), "not stored yet");
                        let _one = stores.lock().unwrap();
                        shared.store(N + i, &codes[N + i]);
                        shared.serialized();
                    }
                });
            }
        });
        for (i, code) in codes.iter().enumerate() {
            shared.load(i, code).expect("every pipeline");
        }
    }

    /// A stored pipeline loads while a save holds the library's lock (one
    /// serialises the whole library, for seconds in the world).
    #[test]
    fn a_pipeline_loads_while_a_save_holds_the_library() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::core::s;
        let (Some(device), Some(library)) = (
            crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()),
            library(),
        ) else {
            return; // no host GPU here
        };
        // No resources, and the root signature made apart: a compiler need
        // not keep one the source names (Wine's does not).
        let source = "[numthreads(1, 1, 1)] void main() {}";
        let mut blob = None;
        // SAFETY: the source outlives the call; the blob is read within it.
        let code = unsafe {
            if D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                None,
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut blob,
                None,
            )
            .is_err()
            {
                return; // no shader compiler here
            }
            let blob = blob.unwrap();
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
                .to_vec()
        };
        // SAFETY: an empty description; the blob is read while it lives.
        let root: ID3D12RootSignature = unsafe {
            let mut blob = None;
            D3D12SerializeRootSignature(
                &D3D12_ROOT_SIGNATURE_DESC::default(),
                D3D_ROOT_SIGNATURE_VERSION_1,
                &mut blob,
                None,
            )
            .unwrap();
            let blob = blob.unwrap();
            let bytes = std::slice::from_raw_parts(
                blob.GetBufferPointer().cast::<u8>(),
                blob.GetBufferSize(),
            );
            device.CreateRootSignature(0, bytes).unwrap()
        };
        struct Made(ID3D12Device1, ID3D12RootSignature, Vec<u8>);
        // SAFETY: D3D12 objects are free-threaded.
        unsafe impl Sync for Made {}
        impl Made {
            fn get(&self, made: bool) -> windows::core::Result<ID3D12PipelineState> {
                let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    // SAFETY: a borrowed pointer the description never releases.
                    pRootSignature: unsafe { std::mem::transmute_copy(&self.1) },
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: self.2.as_ptr().cast(),
                        BytecodeLength: self.2.len(),
                    },
                    ..Default::default()
                };
                pipeline(
                    "test: a pipeline loads while a save holds the library",
                    // SAFETY: the description points at live data.
                    |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                    || match made {
                        // SAFETY: as above.
                        true => unsafe { self.0.CreateComputePipelineState(&desc) },
                        false => panic!("made again, not loaded"),
                    },
                )
            }
        }
        let made = Made(device, root, code);
        made.get(true).expect("made and stored");
        let saving = library.state.lock().unwrap_or_else(|e| e.into_inner());
        let (done, loaded) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            let made = &made;
            s.spawn(move || done.send(made.get(false).is_ok()));
            let loaded = loaded.recv_timeout(std::time::Duration::from_secs(30));
            drop(saving);
            assert_eq!(loaded, Ok(true), "the load waited for the save");
        });
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            return; // no host GPU here
        };
        let library = open(&device, BlobBacking::Owned(b"not a pipeline library".repeat(16)))
            .expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(library.blob.is_empty());
    }

    fn host_device() -> Option<ID3D12Device1> {
        crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok())
    }

    /// An empty library's bytes: a blob the host accepts.
    fn valid_blob(device: &ID3D12Device1) -> Vec<u8> {
        let library = empty(device).expect("an empty library").library;
        // SAFETY: a buffer of the size asked for.
        unsafe {
            let mut bytes = vec![0; library.GetSerializedSize()];
            library.Serialize(&mut bytes).unwrap();
            bytes
        }
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_env_var_opts_out_of_mapping() {
        use std::ffi::OsStr;
        assert!(mmap_set(None));
        assert!(mmap_set(Some(OsStr::new("1"))));
        assert!(mmap_set(Some(OsStr::new(""))));
        assert!(!mmap_set(Some(OsStr::new("0"))));
    }

    #[test]
    fn a_mapped_file_reads_and_writes_what_the_file_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapped.bin");
        let mut out = MappedFile::create(&path, 100_000).unwrap();
        out.as_mut_slice().iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
        out.flush().unwrap();
        drop(out);
        let expected: Vec<u8> = (0..100_000).map(|i| i as u8).collect();
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        let read = MappedFile::open(&path).unwrap();
        assert_eq!(read.as_slice(), &expected[..]);
        // Deleting the file (Rust unlinks it, as Windows 10 allows) leaves
        // the mapping readable.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read.as_slice(), &expected[..]);
        drop(read);
        std::fs::write(&path, &expected).unwrap();
        std::fs::remove_file(&path).unwrap();
        // Nothing to map in a file with no bytes.
        std::fs::write(&path, b"").unwrap();
        assert!(MappedFile::open(&path).is_err());
        assert!(BlobBacking::read(&path, true).is_none());
    }

    #[test]
    fn generations_are_listed_newest_first_with_the_legacy_file_last() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "host-pipelines.bin",
            "host-pipelines.gen-2.bin",
            "host-pipelines.gen-10.bin",
            "host-pipelines.gen-10.bin.b3",
            "host-pipelines.gen-3.bin.tmp-1",
            "host-pipelines.gen-x.bin",
            "early-pipelines.txt",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let found: Vec<_> = generations(dir.path()).into_iter().map(|(n, _)| n).collect();
        assert_eq!(found, [10, 2, 0]);
        remove_other_generations(dir.path(), 2);
        assert_eq!(
            files(dir.path()),
            [
                "early-pipelines.txt",
                "host-pipelines.gen-2.bin",
                "host-pipelines.gen-3.bin.tmp-1",
                "host-pipelines.gen-x.bin"
            ]
        );
    }

    #[test]
    fn a_sidecar_holds_for_its_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let side = dir.path().join("a.bin.b3");
        let digest = digest_of(b"bytes");
        write_sidecar(&side, 5, &digest).unwrap();
        assert_eq!(read_sidecar(&side, 5), Some(digest));
        assert_eq!(read_sidecar(&side, 6), None);
        assert_eq!(read_sidecar(&dir.path().join("none.b3"), 5), None);
        std::fs::write(&side, b"5 short").unwrap();
        assert_eq!(read_sidecar(&side, 5), None);
    }

    /// A library saved to a generation opens again from it, mapped, with
    /// the digest of its bytes taken from the sidecar.
    #[test]
    fn a_saved_library_opens_again_mapped() {
        let Some(device) = host_device() else { return };
        let dir = tempfile::tempdir().unwrap();
        let library = empty(&device).unwrap();
        // SAFETY: a plain query on a live library.
        let size = unsafe { library.library.GetSerializedSize() };
        let (bytes, digest) = write_generation(dir.path(), &library.library, 1, true, true).unwrap();
        assert_eq!(bytes, size);
        assert_eq!(files(dir.path()), ["host-pipelines.gen-1.bin", "host-pipelines.gen-1.bin.b3"]);
        let mut again = open_dir(&device, dir.path(), true).expect("the library again");
        assert!(again.blob.is_mapped());
        assert_eq!(again.blob.len(), size);
        assert_eq!(again.generation, 1);
        let digest = digest.unwrap();
        assert_eq!(again.digest(dir.path()), digest);
        assert_eq!(digest_of(again.blob.as_slice()), digest);
        // The mapping serves the bytes even if the file is deleted meanwhile
        // (a cleanup does that to the generation a run still maps).
        std::fs::remove_file(dir.path().join("host-pipelines.gen-1.bin")).unwrap();
        assert_eq!(digest_of(again.blob.as_slice()), digest);
    }

    /// Each save is a generation after the last, and the older ones go (the
    /// one a library has mapped too, where Windows lets a mapped file go;
    /// else at the next start).
    #[test]
    fn saves_rotate_generations_and_clean_up_the_old_ones() {
        let Some(device) = host_device() else { return };
        let dir = tempfile::tempdir().unwrap();
        let first = empty(&device).unwrap();
        write_generation(dir.path(), &first.library, 1, true, false).unwrap();
        // A run maps generation 1 and saves generation 2 beside it.
        let running = open_dir(&device, dir.path(), true).unwrap();
        assert!(running.blob.is_mapped());
        write_generation(dir.path(), &running.library, running.generation + 1, true, false).unwrap();
        remove_other_generations(dir.path(), 2);
        // Windows may keep the one still mapped (a sharing violation is
        // ignored); the newest stays either way.
        assert!(files(dir.path()).contains(&"host-pipelines.gen-2.bin".to_owned()));
        // The next start opens the newest and removes the old one.
        drop(running);
        let next = open_dir(&device, dir.path(), true).unwrap();
        assert_eq!(next.generation, 2);
        assert_eq!(files(dir.path()), ["host-pipelines.gen-2.bin"]);
    }

    #[test]
    fn the_owned_fallback_reads_and_saves_whole_buffers() {
        let Some(device) = host_device() else { return };
        let dir = tempfile::tempdir().unwrap();
        let library = empty(&device).unwrap();
        write_generation(dir.path(), &library.library, 1, false, true).unwrap();
        let mut again = open_dir(&device, dir.path(), false).expect("the library again");
        assert!(!again.blob.is_mapped());
        assert!(!again.blob.is_empty());
        // The digest of an owned blob is hashed, not taken on trust.
        std::fs::write(sidecar(&dir.path().join("host-pipelines.gen-1.bin")), b"1 wrong").unwrap();
        assert_eq!(again.digest(dir.path()), digest_of(again.blob.as_slice()));
        // The same file maps when asked to.
        drop(again);
        assert!(open_dir(&device, dir.path(), true).unwrap().blob.is_mapped());
    }

    /// The file earlier builds saved opens (as generation 0, with a sidecar
    /// made on the way); a newer generation the host rejects falls back to
    /// it, and a rejected blob alone gives an empty library.
    #[test]
    fn a_legacy_file_opens_and_a_corrupt_generation_is_rejected() {
        let Some(device) = host_device() else { return };
        let dir = tempfile::tempdir().unwrap();
        let blob = valid_blob(&device);
        std::fs::write(dir.path().join(LIBRARY_FILE), &blob).unwrap();
        let mut legacy = open_dir(&device, dir.path(), true).expect("the legacy library");
        assert!(legacy.blob.is_mapped());
        assert_eq!(legacy.generation, 0);
        assert_eq!(legacy.digest(dir.path()), digest_of(&blob));
        assert!(dir.path().join("host-pipelines.bin.b3").exists());
        drop(legacy);
        // A newer generation that is damaged: the legacy one serves, the
        // damaged one goes, and the next save does not reuse its number.
        std::fs::write(dir.path().join("host-pipelines.gen-4.bin"), b"damaged".repeat(100)).unwrap();
        let library = open_dir(&device, dir.path(), true).expect("the legacy library again");
        assert_eq!(library.blob.len(), blob.len());
        assert_eq!(library.generation, 4);
        assert!(!dir.path().join("host-pipelines.gen-4.bin").exists());
        drop(library);
        // Only damage: an empty library, and nothing left of it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("host-pipelines.gen-1.bin"), b"damaged".repeat(100)).unwrap();
        let library = open_dir(&device, dir.path(), true).expect("an empty library");
        assert!(library.blob.is_empty());
        assert_eq!(library.generation, 1);
        assert!(files(dir.path()).is_empty());
    }

    #[test]
    fn concurrent_write_atomically_never_fails_or_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("host-pipelines.bin");
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let target = target.clone();
                std::thread::spawn(move || {
                    let payload = vec![b'a' + t as u8; 4096];
                    for _ in 0..30 {
                        write_atomically(&target, &payload).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let final_bytes = std::fs::read(&target).unwrap();
        assert_eq!(final_bytes.len(), 4096);
        assert!(final_bytes.iter().all(|&b| b == final_bytes[0]));
    }

    #[test]
    fn remove_older_generations_never_deletes_newer_generations() {
        let dir = tempfile::tempdir().unwrap();
        for n in [1u64, 2, 3] {
            let bin = dir.path().join(generation_file(n));
            std::fs::write(&bin, b"blob").unwrap();
            std::fs::write(sidecar(&bin), b"4 deadbeef").unwrap();
        }
        remove_older_generations(dir.path(), 2);
        assert!(!dir.path().join(generation_file(1)).exists());
        assert!(!sidecar(&dir.path().join(generation_file(1))).exists());
        assert!(dir.path().join(generation_file(2)).exists());
        assert!(dir.path().join(generation_file(3)).exists());
        assert!(sidecar(&dir.path().join(generation_file(3))).exists());
    }
}
