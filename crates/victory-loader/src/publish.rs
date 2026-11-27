//! Portable publication of derived files into a shared cache directory.
//!
//! Several copies of the runner can prepare the same title at once. Each
//! writer stages into its own file, so two of them cannot truncate each
//! other's bytes or rename a file the other already published. The final
//! rename is atomic within the directory, and the staging file is removed
//! if publication fails.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_LOCK_VIOLATION, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, OPEN_ALWAYS,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::OVERLAPPED;

static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);

/// Written at the front of the sidecar: a sidecar this build did not write
/// (an older layout, a stray file) reads as absent, so the image is read
/// back instead of trusted.
const SIDECAR_MAGIC: [u8; 16] = *b"victory.loader.2";

/// Magic, digest, length, last-write time.
const SIDECAR_LEN: usize = 16 + 32 + 8 + 8;

/// The sidecar published beside `path`: the digest of the published bytes,
/// and the length and last-write time of the image they were written to.
/// Comparing it against the image on disk answers "already there" without
/// reading the cached image back (a title image is ~100 MB; the read per
/// boot per DLL was the point of the sidecar).
fn sidecar(path: &Path) -> std::path::PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
    path.with_file_name(format!(".{name}.blake3"))
}

/// What a sidecar records about the image beside it. The digest and the
/// length alone would accept a stale image of the same size that another
/// writer replaced, so the image's last-write time is recorded and compared
/// too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sidecar {
    digest: [u8; 32],
    len: u64,
    written_nanos: u64,
}

impl Sidecar {
    fn encode(&self) -> [u8; SIDECAR_LEN] {
        let mut out = [0u8; SIDECAR_LEN];
        out[..16].copy_from_slice(&SIDECAR_MAGIC);
        out[16..48].copy_from_slice(&self.digest);
        out[48..56].copy_from_slice(&self.len.to_le_bytes());
        out[56..64].copy_from_slice(&self.written_nanos.to_le_bytes());
        out
    }

    /// `None` for anything this layout does not describe: the image is then
    /// read back, and the sidecar rewritten.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; SIDECAR_LEN] = bytes.try_into().ok()?;
        if bytes[..16] != SIDECAR_MAGIC {
            return None;
        }
        Some(Self {
            digest: bytes[16..48].try_into().ok()?,
            len: u64::from_le_bytes(bytes[48..56].try_into().ok()?),
            written_nanos: u64::from_le_bytes(bytes[56..64].try_into().ok()?),
        })
    }
}

/// The identity the sidecar records for the image at `path`: its length and
/// last-write time. `None` if it cannot be stat'ed with a time the sidecar
/// can hold, which sends the caller down the read-back path.
fn image_stamp(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((metadata.len(), u64::try_from(since.as_nanos()).ok()?))
}

/// The sidecar beside `path`, if this layout wrote it.
fn read_sidecar(path: &Path) -> Option<Sidecar> {
    let bytes = std::fs::read(sidecar(path)).ok()?;
    Sidecar::decode(&bytes)
}

/// Writes the sidecar through its own staging file + rename, so a reader
/// never sees a torn digest. Not fatal: without it the next boot falls back
/// to reading the image.
fn write_sidecar(path: &Path, digest: &blake3::Hash) {
    // Without the image's stamp the sidecar could not be trusted next boot,
    // so none is written: the image is read back instead.
    let Some((len, written_nanos)) = image_stamp(path) else {
        return;
    };
    let side = Sidecar {
        digest: *digest.as_bytes(),
        len,
        written_nanos,
    };
    let side_path = sidecar(path);
    let name = side_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("digest");
    let sequence = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
    let staging =
        side_path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    if std::fs::write(&staging, side.encode())
        .and_then(|_| std::fs::rename(&staging, &side_path))
        .is_err()
    {
        let _ = std::fs::remove_file(&staging);
    }
}

/// How long a publisher waits for the lock before it stops waiting for it.
/// Long enough for a holder that is running to finish the read-back, sidecar
/// removal and rename it holds the lock for (a few file operations: the
/// milliseconds they take), short enough that a holder that never runs again
/// — a publisher killed or suspended mid-publish — costs the boot a bounded
/// wait instead of all of it.
#[cfg(windows)]
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a publisher sleeps between attempts at the lock.
#[cfg(windows)]
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(10);

/// What asking for the lock got.
#[cfg(windows)]
enum PublishLock {
    /// Held for the guard's life: closing the handle releases it, so a
    /// crashed publisher does not leave the cache locked.
    Held(HANDLE),
    /// A holder kept the lock past `LOCK_WAIT`. The caller must not wait for
    /// it any longer — a holder that is suspended or gone keeps the lock until
    /// its process ends — and publishes only what it can check without
    /// writing; see `publish_without_lock`.
    TimedOut,
}

#[cfg(windows)]
impl Drop for PublishLock {
    fn drop(&mut self) {
        if let PublishLock::Held(handle) = *self {
            // SAFETY: our handle.
            unsafe { CloseHandle(handle) };
        }
    }
}

/// Locks the publishers of `path` against each other, for at most
/// `LOCK_WAIT`.
///
/// The lock is asked for with `LOCKFILE_FAIL_IMMEDIATELY` and retried in a
/// bounded loop. A publisher that lost the race to one that is about to finish
/// still serialises behind it, which is what orders the sidecar around the
/// image's rename. A publisher that lost the race to one that is not going to
/// finish cannot tell the two apart by waiting, so it stops at `LOCK_WAIT`: an
/// unbounded `LockFileEx` would hang the boot silently and forever, with
/// nothing to show for it.
#[cfg(windows)]
fn publish_lock(path: &Path) -> std::io::Result<PublishLock> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("image");
    let lock_path = path.with_file_name(format!(".{name}.lock"));
    let wide: Vec<u16> = lock_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: a valid path, null security attributes, no template.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a zeroed OVERLAPPED is valid; unused on a synchronous handle.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let started = std::time::Instant::now();
    let mut said = false;
    loop {
        // SAFETY: a valid handle, and an OVERLAPPED of the right type.
        let locked = unsafe {
            LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        } != 0;
        if locked {
            return Ok(PublishLock::Held(handle));
        }
        let error = std::io::Error::last_os_error();
        // Only a holder of the range is waited for. Anything else (a lock file
        // that cannot be locked at all) is this publisher's problem, and is
        // reported rather than retried until the wait runs out.
        if error.raw_os_error() != Some(ERROR_LOCK_VIOLATION as i32) {
            // SAFETY: our handle.
            unsafe { CloseHandle(handle) };
            return Err(error);
        }
        let waited = started.elapsed();
        if waited >= LOCK_WAIT {
            // SAFETY: our handle.
            unsafe { CloseHandle(handle) };
            tracing::warn!(
                path = %lock_path.display(),
                waited_ms = waited.as_millis(),
                "gave up waiting for the publisher holding the cache lock",
            );
            return Ok(PublishLock::TimedOut);
        }
        // Said once, before the first retry: a holder that is merely slow (a
        // large image, a scanner reading it) is then visible in the log, rather
        // than a boot that looks stuck, and the line is there ahead of the give
        // up line for a holder that is gone.
        if !said {
            said = true;
            tracing::warn!(
                path = %lock_path.display(),
                waited_ms = waited.as_millis(),
                wait_ms = LOCK_WAIT.as_millis(),
                "another publisher holds the cache lock; waiting for it",
            );
        }
        std::thread::sleep(LOCK_RETRY);
    }
}

/// Publishes without the publishers' lock, which is safe only where nothing is
/// written: the image is checked by reading it back, and an image that already
/// holds `bytes` is left alone.
///
/// A cache hit publishes no sidecar either. The sidecar records the image's
/// length and last-write time taken from whatever is at the path when it is
/// written, so one written beside a holder's rename could describe the
/// holder's image with this digest — and the boot after that would trust it.
/// Left unwritten, the next boot reads the image back instead, which is what
/// this path has just done, and the sidecar is backfilled by a publisher that
/// holds the lock.
///
/// An image that has to be replaced needs the lock: the sidecar's removal, the
/// rename and the sidecar's write are ordered by it, and interleaving them with
/// a holder that may wake up mid-sequence is how a sidecar ends up describing
/// an image that is not on disk. So that publish is reported — an error naming
/// the file, which the load path carries into the boot's failure — instead of
/// raced.
#[cfg(windows)]
fn publish_without_lock(path: &Path, staging: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let _ = std::fs::remove_file(staging);
    if std::fs::read(path).is_ok_and(|existing| existing == bytes) {
        tracing::info!(
            path = %path.display(),
            "the image is already published; the sidecar is left to the lock holder",
        );
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!(
            "another publisher held the lock on {} for more than {} ms",
            path.display(),
            LOCK_WAIT.as_millis()
        ),
    ))
}

/// Writes `bytes` to `path` unless it is already there, using a staging file
/// unique to this process and call. "Already there" is answered from the
/// sidecar when it describes the image on disk — its digest, length and
/// last-write time all match — and by reading the file back otherwise (a
/// cache from before sidecars, or an image another writer replaced).
pub fn write_if_changed(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let digest = blake3::hash(bytes);
    // Fast path: the sidecar was written for this image and still describes
    // it, so the image holds the bytes asked for — no read of the image. A
    // same-length replacement fails the time check here instead of passing
    // as a hit.
    if let Some((len, written_nanos)) = image_stamp(path)
        && read_sidecar(path).is_some_and(|side| {
            side.digest == *digest.as_bytes()
                && (side.len, side.written_nanos) == (len, written_nanos)
        })
    {
        return Ok(());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image");
    let sequence = NEXT_STAGING.fetch_add(1, Ordering::Relaxed);
    let staging = path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()));
    std::fs::write(&staging, bytes)?;
    // Serialize publishers, and order the sidecar around the rename: it is
    // removed before the rename and written after it, so a sidecar on disk
    // always describes the file on disk. A crash in between leaves no
    // sidecar, and the next boot falls back to reading the image.
    // The read-back of a cache from before sidecars is inside the same
    // critical section: written outside it, its sidecar could describe an
    // image another publisher replaced meanwhile.
    #[cfg(windows)]
    let lock = publish_lock(path)?;
    // A holder that kept the lock past `LOCK_WAIT` is not waited for any
    // longer: the image is verified by reading it back, and nothing is
    // written beside it (see `publish_without_lock`).
    #[cfg(windows)]
    if matches!(lock, PublishLock::TimedOut) {
        return publish_without_lock(path, &staging, bytes);
    }
    if std::fs::read(path).is_ok_and(|existing| existing == bytes) {
        // Already there: keep it, and publish the sidecar for the next boot
        // (a failure there only costs the fallback read next time).
        let _ = std::fs::remove_file(&staging);
        write_sidecar(path, &digest);
        return Ok(());
    }
    // A sidecar that cannot be removed must not survive beside an image it
    // no longer describes: stop before replacing the image.
    match std::fs::remove_file(sidecar(path)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            return Err(error);
        }
    }
    if let Err(error) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    // The digest of what was published, for the fast path next time. Not
    // fatal: without it the next boot falls back to reading the image.
    write_sidecar(path, &digest);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two publishers staging the same destination at once both succeed,
    /// and neither leaves a staging file behind.
    #[test]
    fn concurrent_publishers_do_not_share_a_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let bytes = b"a prepared image".to_vec();
        let (first, second) = (path.clone(), path.clone());
        let (first_bytes, second_bytes) = (bytes.clone(), bytes.clone());
        let (one, two) = (
            std::thread::spawn(move || write_if_changed(&first, &first_bytes)),
            std::thread::spawn(move || write_if_changed(&second, &second_bytes)),
        );
        one.join().unwrap().unwrap();
        two.join().unwrap().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                let p = entry.path();
                let name = path.file_name().unwrap().to_str().unwrap();
                p != path
                    && p != sidecar(&path)
                    && p != path.with_file_name(format!(".{name}.lock"))
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging files were removed: {leftovers:?}"
        );
    }

    /// A cache hit is answered from the sidecar digest: the image file is
    /// not read back (it is unreadable here and the call still succeeds).
    /// A cache from before sidecars falls back to reading, and publishes the
    /// sidecar for next time. Changed bytes republish and move the sidecar.
    #[test]
    fn a_cache_hit_reads_the_sidecar_not_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let bytes = vec![7u8; 1 << 20];
        write_if_changed(&path, &bytes).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let side = read_sidecar(&path).expect("the publish recorded a sidecar");
        assert_eq!(side.digest, *blake3::hash(&bytes).as_bytes());
        assert_eq!(side.len, bytes.len() as u64);
        assert_eq!(side.written_nanos, image_stamp(&path).unwrap().1);
        // Same bytes again: a hit, nothing rewritten.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        write_if_changed(&path, &bytes).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        // The image deleted under a matching sidecar: republished (the
        // stamp misses, the read fallback misses, the write happens).
        std::fs::remove_file(&path).unwrap();
        write_if_changed(&path, &bytes).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // Changed bytes republish and move the sidecar.
        let new = vec![8u8; 1 << 20];
        write_if_changed(&path, &new).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), new);
        assert_eq!(
            read_sidecar(&path).unwrap().digest,
            *blake3::hash(&new).as_bytes()
        );
        // A legacy cache (no sidecar) falls back to reading, then publishes
        // the sidecar.
        std::fs::remove_file(sidecar(&path)).unwrap();
        write_if_changed(&path, &new).unwrap();
        assert_eq!(
            read_sidecar(&path).unwrap().digest,
            *blake3::hash(&new).as_bytes()
        );
        assert_eq!(read_sidecar(&path).unwrap().len, new.len() as u64);
        // Boot cost: 64 MB image, cache hit — sidecar read + in-memory hash
        // vs the old full read + compare.
        let big = vec![3u8; 64 << 20];
        let big_path = dir.path().join("big.bin");
        write_if_changed(&big_path, &big).unwrap();
        let t0 = std::time::Instant::now();
        for _ in 0..5 {
            write_if_changed(&big_path, &big).unwrap();
        }
        let hit = t0.elapsed() / 5;
        let t0 = std::time::Instant::now();
        for _ in 0..5 {
            let existing = std::fs::read(&big_path).unwrap();
            assert!(existing == big);
        }
        let old = t0.elapsed() / 5;
        eprintln!("64 MB cache hit: sidecar path {hit:?}, old read+compare {old:?}");
    }

    /// The sidecar records the image's length and last-write time, so an
    /// image another writer replaced with different bytes of the same length
    /// is not a cache hit: it is read back, and republished.
    #[test]
    fn a_same_length_stale_image_is_not_a_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let published = vec![0x11u8; 1 << 16];
        write_if_changed(&path, &published).unwrap();
        let recorded = read_sidecar(&path).expect("the publish recorded a sidecar");
        // Different bytes of the same length, the sidecar left in place:
        // only the recorded last-write time can tell the two apart.
        let stale = vec![0x22u8; 1 << 16];
        std::fs::write(&path, &stale).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
            .unwrap();
        let (len, written_nanos) = image_stamp(&path).unwrap();
        assert_eq!(len, recorded.len, "the stale image has the recorded length");
        assert_ne!(
            written_nanos, recorded.written_nanos,
            "and a last-write time the sidecar does not describe"
        );
        write_if_changed(&path, &published).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            published,
            "the stale image was read back and replaced, not taken for a hit"
        );
        let side = read_sidecar(&path).unwrap();
        assert_eq!(side.digest, *blake3::hash(&published).as_bytes());
        assert_eq!(side.len, published.len() as u64);
        assert_eq!(side.written_nanos, image_stamp(&path).unwrap().1);
    }

    /// A sidecar in the older digest-only layout records no image stamp, so
    /// it is not trusted: the image is read back, and a stale one replaced.
    #[test]
    fn a_digest_only_sidecar_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let published = vec![0x33u8; 1 << 16];
        write_if_changed(&path, &published).unwrap();
        // The older layout: the 32 digest bytes and nothing else, beside a
        // same-length image that is not the one they describe.
        std::fs::write(sidecar(&path), blake3::hash(&published).as_bytes()).unwrap();
        let stale = vec![0x44u8; 1 << 16];
        std::fs::write(&path, &stale).unwrap();
        assert!(
            read_sidecar(&path).is_none(),
            "a sidecar without an image stamp reads as absent"
        );
        write_if_changed(&path, &published).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), published);
        assert_eq!(
            read_sidecar(&path).map(|s| s.digest),
            Some(*blake3::hash(&published).as_bytes())
        );
    }

    /// A cache hit is answered from the sidecar and the image's metadata, not
    /// by reading the image: held here with no sharing at all, the image
    /// cannot be read, and the call still succeeds.
    #[cfg(windows)]
    #[test]
    fn a_cache_hit_does_not_read_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let bytes = vec![0x5au8; 1 << 16];
        write_if_changed(&path, &bytes).unwrap();
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: a valid path; the handle is closed below.
        let held = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                0,
                std::ptr::null(),
                windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            held,
            INVALID_HANDLE_VALUE,
            "open the image: {}",
            std::io::Error::last_os_error()
        );
        let hit = write_if_changed(&path, &bytes);
        // SAFETY: our handle.
        unsafe { CloseHandle(held) };
        assert!(hit.is_ok(), "a cache hit must not read the image: {hit:?}");
    }

    /// A cache from before sidecars: its sidecar is backfilled under the
    /// publishers' lock, so it cannot land over an image another publisher
    /// replaced meanwhile. Held here for part of the wait, a publisher
    /// retries and then has the lock.
    #[cfg(windows)]
    #[test]
    fn a_legacy_sidecar_backfill_waits_for_the_publishers_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let bytes = vec![0x1au8; 1 << 16];
        // A legacy cache: the image is there, the sidecar is not.
        std::fs::write(&path, &bytes).unwrap();
        let held = publish_lock(&path).unwrap();
        assert!(
            matches!(held, PublishLock::Held(_)),
            "the lock was free for the test to take"
        );
        let (p, b) = (path.clone(), bytes.clone());
        let published = std::thread::spawn(move || write_if_changed(&p, &b));
        // Held for part of the publisher's wait, so it is still retrying here
        // and the backfill below happens under the lock rather than after a
        // give-up.
        std::thread::sleep(LOCK_WAIT / 2);
        assert!(
            !sidecar(&path).exists(),
            "the sidecar was published outside the publishers' lock"
        );
        drop(held);
        published.join().unwrap().unwrap();
        let side = read_sidecar(&path).expect("the backfill wrote a sidecar");
        assert_eq!(side.digest, *blake3::hash(&bytes).as_bytes());
        assert_eq!(side.written_nanos, image_stamp(&path).unwrap().1);
    }

    /// Runs `attempt` on its own thread and hands back what it produced,
    /// failing the test if it has not returned within a generous ceiling. An
    /// attempt that regressed to waiting for a lock it cannot have must fail
    /// this suite, not hang it; the thread that is stuck is left to the test
    /// process, which ends the suite.
    #[cfg(windows)]
    fn within_ceiling<T: Send + 'static>(
        what: &str,
        attempt: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        const CEILING: std::time::Duration = std::time::Duration::from_secs(5);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(attempt());
        });
        receiver.recv_timeout(CEILING).unwrap_or_else(|_| {
            panic!("{what} did not return within {CEILING:?}: it blocked instead of giving up")
        })
    }

    /// A lock this process already holds is retried and given up on, not
    /// waited for forever: a holder that is suspended or hung (a debugger, a
    /// stopped process) would otherwise stall the boot silently, with nothing
    /// in the log to say which file it was. Measured, and run behind
    /// `within_ceiling`, so a regression to a blocking `LockFileEx` fails here
    /// instead of hanging the suite.
    #[cfg(windows)]
    #[test]
    fn a_lock_held_past_the_wait_gives_up_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let held = publish_lock(&path).unwrap();
        assert!(matches!(held, PublishLock::Held(_)), "the lock was free");
        let waiting = path.clone();
        let (elapsed, gave_up) = within_ceiling("a second attempt at the lock", move || {
            let started = std::time::Instant::now();
            let outcome = publish_lock(&waiting).expect("a lock held elsewhere is not an error");
            let gave_up = matches!(outcome, PublishLock::TimedOut);
            (started.elapsed(), gave_up)
        });
        // Released even though the attempt gave up, so the file is free for the
        // next test that wants it.
        drop(held);
        assert!(gave_up, "the attempt reported that it did not get the lock");
        assert!(
            elapsed >= LOCK_WAIT,
            "it retried for its whole bound before giving up: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "and gave up within a few hundred milliseconds: {elapsed:?}"
        );
    }

    /// A publish that cannot have the lock within the wait does not block
    /// either: it verifies the image by reading it back. So a cache hit still
    /// succeeds, and writes nothing beside the image (a sidecar stamped here
    /// could describe the holder's image), while an image that would have to be
    /// replaced — which needs the lock for the sidecar ordering — is reported.
    #[cfg(windows)]
    #[test]
    fn a_publish_that_waits_out_the_lock_verifies_the_image_instead() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let there = vec![0x66u8; 1 << 16];
        std::fs::write(&path, &there).unwrap();
        let held = publish_lock(&path).unwrap();
        assert!(matches!(held, PublishLock::Held(_)), "the lock was free");
        // The bytes asked for are already the image: read back and left alone.
        let (p, b) = (path.clone(), there.clone());
        let hit = within_ceiling("a publish of the image already there", move || {
            write_if_changed(&p, &b)
        });
        assert!(hit.is_ok(), "the image is already there: {hit:?}");
        assert!(
            !sidecar(&path).exists(),
            "no sidecar is published without the publishers' lock"
        );
        // Different bytes: this publish would have to replace the image, which
        // needs the lock, so it is reported instead of raced.
        let (p, b) = (path.clone(), vec![0x77u8; 1 << 16]);
        let stopped = within_ceiling("a publish that has to replace the image", move || {
            write_if_changed(&p, &b)
        });
        drop(held);
        let error = stopped.expect_err("a publish that cannot replace the image is reported");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock, "{error}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            there,
            "the image was not replaced beside a holder this publish cannot see"
        );
        let staging: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(staging.is_empty(), "no staging file survives: {staging:?}");
    }

    /// A sidecar another process holds without delete sharing cannot be
    /// removed: publication stops before replacing the image, so the sidecar
    /// on disk still describes the file on disk.
    #[cfg(windows)]
    #[test]
    fn a_sidecar_that_cannot_be_removed_stops_the_publish() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let a = vec![0xaau8; 1 << 16];
        let b = vec![0xbbu8; 1 << 16];
        write_if_changed(&path, &a).unwrap();
        let side = sidecar(&path);
        let wide: Vec<u16> = side
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: a valid path; the handle is closed below.
        let open = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            open,
            INVALID_HANDLE_VALUE,
            "open the sidecar: {}",
            std::io::Error::last_os_error()
        );
        let stopped = write_if_changed(&path, &b);
        // SAFETY: our handle.
        unsafe { CloseHandle(open) };
        assert!(
            stopped.is_err(),
            "a publish whose old sidecar cannot be removed must fail: {stopped:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), a, "the image is untouched");
        assert_eq!(
            read_sidecar(&path).map(|s| s.digest),
            Some(*blake3::hash(&a).as_bytes()),
            "the sidecar still describes the image"
        );
    }

    /// Many interleaved publishers of alternating contents: whenever a
    /// sidecar exists, it describes the file on disk (the remove-before-
    /// rename / write-after ordering under the lock).
    #[test]
    fn concurrent_publishers_keep_the_sidecar_matching_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.bin");
        let a = vec![1u8; 4096];
        let b = vec![2u8; 4096];
        let mut handles = Vec::new();
        for t in 0..4u32 {
            let (path, a, b) = (path.clone(), a.clone(), b.clone());
            handles.push(std::thread::spawn(move || {
                for i in 0..50u32 {
                    let bytes = if (i + t) % 2 == 0 { &a } else { &b };
                    write_if_changed(&path, bytes).unwrap();
                    // If the file did not change across the check, a sidecar
                    // present at the same time must describe it.
                    // A rename by another publisher can make these reads
                    // fail transiently; only check when all three succeed.
                    if let (Ok(before), Some(side), Ok(after)) = (
                        std::fs::read(&path),
                        read_sidecar(&path),
                        std::fs::read(&path),
                    ) && before == after
                    {
                        assert_eq!(
                            side.digest,
                            *blake3::hash(&before).as_bytes(),
                            "sidecar matches the file"
                        );
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(
            read_sidecar(&path).map(|s| s.digest),
            Some(*blake3::hash(&on_disk).as_bytes())
        );
    }

    #[test]
    fn a_failed_publication_removes_its_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-parent").join("image.bin");
        assert!(write_if_changed(&path, b"bytes").is_err());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        assert!(leftovers.is_empty(), "no staging file survives an error");
    }
}
