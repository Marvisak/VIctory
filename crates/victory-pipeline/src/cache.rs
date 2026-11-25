use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::Converter;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// Content-addressed store under the user's cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Opens (creating) a cache at `root`. Refuses locations inside the repo.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        victory_core::ensure_outside_repo(&root)?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Cache { root })
    }

    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Returns the cached output for `input`, converting it first if needed.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<PathBuf> {
        let key = CacheKey::new(converter, input);
        let path = self.path_for(&key);
        // A reader can keep a mismatched result in place on Windows. Share
        // the invalidation through the directory so reopened caches see it too.
        let invalid = path.with_extension("invalid");
        let found = path.is_file();
        let was_invalid = invalid.try_exists()?;
        if found && !was_invalid {
            tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
            return Ok(path);
        }
        let output = converter
            .convert(input)
            .with_context(|| format!("converter {} v{}", converter.id(), converter.version()))?;
        let dir = path.parent().expect("hashed path has a parent");
        std::fs::create_dir_all(dir)?;
        // Write-then-rename so readers never see partial output.
        let (tmp, mut file) = tempfile_in(dir)?;
        let written = file.write_all(&output);
        drop(file);
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(err.into());
        }
        if let Err(err) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            match same_output(&path, &output) {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(mark_error) = File::create_new(&invalid)
                        && mark_error.kind() != io::ErrorKind::AlreadyExists
                    {
                        return Err(anyhow::Error::new(err).context(format!(
                            "marking the mismatched cache entry invalid: {mark_error}"
                        )));
                    }
                    return Err(err.into());
                }
                Err(_) => return Err(err.into()),
            }
        }
        // Leave any invalidation created during a normal miss for the next
        // lookup to check. Clear one we observed only after valid publication.
        if was_invalid
            && let Err(err) = std::fs::remove_file(&invalid)
            && err.kind() != io::ErrorKind::NotFound
        {
            return Err(err.into());
        }
        Ok(path)
    }
}

fn same_output(path: &Path, output: &[u8]) -> io::Result<bool> {
    let mut published = File::open(path)?;
    let metadata = published.metadata()?;
    if !metadata.is_file() || metadata.len() != output.len() as u64 {
        return Ok(false);
    }

    // Cache outputs can be large. Compare bounded chunks without copying the file.
    let mut buffer = [0; 8192];
    for expected in output.chunks(buffer.len()) {
        let actual = &mut buffer[..expected.len()];
        published.read_exact(actual)?;
        if actual != expected {
            return Ok(false);
        }
    }
    let mut extra = [0];
    Ok(published.read(&mut extra)? == 0)
}

fn tempfile_in(dir: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    create_tempfile_in(dir, &COUNTER)
}

fn create_tempfile_in(dir: &Path, counter: &AtomicU64) -> io::Result<(PathBuf, File)> {
    // An interrupted writer can leave a file behind, and process IDs are
    // reused. Keep it intact and try a new name rather than failing a miss.
    for _ in 0..128 {
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".tmp-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique cache staging file after 128 attempts",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    #[test]
    fn converts_once_then_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn concurrent_publication_reuses_only_identical_entries() {
        use std::os::windows::fs::OpenOptionsExt;

        struct RacingUpper {
            cache: Cache,
            published: &'static [u8],
            held: std::sync::Mutex<Option<std::fs::File>>,
        }

        impl Converter for RacingUpper {
            fn id(&self) -> &'static str {
                "test.upper"
            }
            fn version(&self) -> u32 {
                1
            }
            fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
                let output = input.to_ascii_uppercase();
                let key = CacheKey::new(self, input);
                let path = self.cache.path_for(&key);
                let dir = path.parent().unwrap();
                std::fs::create_dir_all(dir)?;
                std::fs::write(&path, self.published)?;
                // FILE_SHARE_READ permits readers but prevents replacement while held.
                *self.held.lock().unwrap() = Some(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .share_mode(1)
                        .open(&path)?,
                );
                let (probe, mut probe_file) = tempfile_in(dir)?;
                probe_file.write_all(&output)?;
                drop(probe_file);
                assert!(
                    std::fs::rename(&probe, &path).is_err(),
                    "the open reader must prevent replacing the published entry"
                );
                std::fs::remove_file(probe)?;
                Ok(output)
            }
        }

        let cases: &[(&[u8], bool)] = &[(b"ABC", true), (b"XYZ", false), (b"ABCD", false)];
        for &(published, expected) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let cache = Cache::open(tmp.path().join("cache")).unwrap();
            let conv = RacingUpper {
                cache: cache.clone(),
                published,
                held: std::sync::Mutex::new(None),
            };
            let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
            let result = cache.get_or_convert(&conv, b"abc");
            assert_eq!(std::fs::read(&path).unwrap(), published);
            let retry_cache = Cache::open(tmp.path().join("cache")).unwrap();
            let retry_converter = Upper {
                version: 1,
                runs: AtomicUsize::new(0),
            };
            if expected {
                assert_eq!(result.unwrap(), path);
                assert_eq!(
                    retry_cache
                        .get_or_convert(&retry_converter, b"abc")
                        .unwrap(),
                    path
                );
                assert_eq!(retry_converter.runs.load(Ordering::Relaxed), 0);
            } else {
                assert!(
                    result.is_err(),
                    "different content was accepted: {published:?}"
                );
                let retry = retry_cache.get_or_convert(&retry_converter, b"abc");
                assert!(
                    retry.is_err(),
                    "a retry treated mismatched content as a hit: {:?}",
                    retry.ok().and_then(|path| std::fs::read(path).ok())
                );
                assert_eq!(std::fs::read(&path).unwrap(), published);
                drop(conv.held.lock().unwrap().take());
                let repaired = retry_cache
                    .get_or_convert(&retry_converter, b"abc")
                    .unwrap();
                assert_eq!(repaired, path);
                assert_eq!(std::fs::read(&repaired).unwrap(), b"ABC");
                assert_eq!(
                    cache.get_or_convert(&retry_converter, b"abc").unwrap(),
                    path
                );
                assert_eq!(retry_converter.runs.load(Ordering::Relaxed), 2);
            }
            assert_eq!(
                std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
                1
            );
        }
    }

    #[test]
    fn staging_retries_collisions_without_changing_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        for n in 0..2 {
            std::fs::write(
                dir.path().join(format!(".tmp-{}-{n}", std::process::id())),
                b"stale",
            )
            .unwrap();
        }

        let (path, mut file) = create_tempfile_in(dir.path(), &counter).unwrap();
        file.write_all(b"fresh").unwrap();
        drop(file);
        assert_eq!(
            path.file_name().unwrap(),
            format!(".tmp-{}-2", std::process::id()).as_str()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh");
        for n in 0..2 {
            assert_eq!(
                std::fs::read(dir.path().join(format!(".tmp-{}-{n}", std::process::id()))).unwrap(),
                b"stale"
            );
        }
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn staging_collision_retries_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        for n in 0..128 {
            std::fs::write(
                dir.path().join(format!(".tmp-{}-{n}", std::process::id())),
                b"stale",
            )
            .unwrap();
        }

        let error = create_tempfile_in(dir.path(), &counter).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(counter.load(Ordering::Relaxed), 128);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 128);
    }

    #[test]
    fn staging_does_not_retry_other_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        let missing = dir.path().join("missing");

        let error = create_tempfile_in(&missing, &counter).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert!(!missing.exists());
    }

    #[test]
    fn version_bump_invalidates() {
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }
}
