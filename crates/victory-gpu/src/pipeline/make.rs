//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and Grand
//! Theft Auto VI's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame (`VICTORY_PIPELINE_WAIT_MS`, 500 ms by default), so the cheap ones
//! (menus) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//! Pipelines a draw waits for go ahead of those started early (at the
//! title's creation, because an earlier run drew them). A one-off pass, such
//! as a colour-grading bake, has no later frame to try again in: its first
//! use may be given its own wait with `VICTORY_PIPELINE_WAIT_ONCE_MS` (off by
//! default).
//!
//! A pipeline first used while still pending is listed for the next run
//! either way, so that run starts making it at creation, during loading.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queued: AtomicBool,
    /// Whether it went into the queue of pipelines a draw waits for, and
    /// whether a worker took it (it may be in both queues).
    now: AtomicBool,
    taken: AtomicBool,
    made: OnceLock<Option<HostPipeline>>,
    #[cfg(feature = "diagnostics")]
    pub(super) skipped_shader: Option<String>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw has used it yet.
    pub(crate) key: String,
    used: AtomicBool,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(
        shaders: Vec<Arc<Shader>>,
        graphics: Option<Box<GraphicsState>>,
        #[cfg(feature = "diagnostics")] skip_hashes: &[String],
    ) -> Pending {
        let key = key(&shaders, graphics.as_deref());
        #[cfg(feature = "diagnostics")]
        log("created", &key, || {
            let state = graphics.as_ref().map(|g| g.describe()).unwrap_or_default();
            format!("{shaders:?}\t{state}")
        });
        #[cfg(feature = "diagnostics")]
        let skipped_shader = super::skipped_shader(&shaders, skip_hashes).map(str::to_owned);
        let made = OnceLock::new();
        // Publish the completed result before the pipeline can be queued.
        #[cfg(feature = "diagnostics")]
        if skipped_shader.is_some() {
            let _ = made.set(None);
        }
        Pending {
            key,
            shaders,
            graphics,
            queued: AtomicBool::new(false),
            now: AtomicBool::new(false),
            taken: AtomicBool::new(false),
            made,
            #[cfg(feature = "diagnostics")]
            skipped_shader,
            used: AtomicBool::new(false),
        }
    }

    fn make(&self) {
        self.made.get_or_init(|| {
            let (started, cpu) = (Instant::now(), super::thread_cpu());
            let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
            #[cfg(feature = "diagnostics")]
            log_built(&self.key);
            let (user, kernel) = super::thread_cpu_since(cpu);
            tracing::debug!(
                shaders = ?self.shaders,
                ms = started.elapsed().as_millis() as u64,
                user_ms = user,
                kernel_ms = kernel,
                "pipeline made"
            );
            made
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// What identifies a title pipeline across runs: its shader containers
/// and graphics state.
pub(crate) fn key(shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) -> String {
    let mut key = blake3::Hasher::new();
    for s in shaders {
        key.update(s.stage.as_bytes());
        key.update(s.hash.as_bytes());
    }
    key.update(graphics.map(|g| g.describe()).unwrap_or_default().as_bytes());
    key.finalize().to_hex()[..32].to_owned()
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Logs the library name of the host pipeline this thread just made or
/// loaded for `key` (diagnostics).
#[cfg(feature = "diagnostics")]
pub(crate) fn log_built(key: &str) {
    if let Some((name, loaded)) = super::library::LAST.take() {
        log("built", key, || format!("{name}\t{}", if loaded { "loaded" } else { "made" }));
    }
}

/// `VICTORY_PIPELINE_LOG=<file>` (diagnostics): a line per title pipeline
/// key created (`created`, its shaders, its state as described) and first
/// drawn or dispatched with (`used`), and its host pipeline's library name
/// once made or loaded (`built`, also from the pre-build), to compare what
/// a run creates with what the pre-build makes.
#[cfg(feature = "diagnostics")]
fn log(what: &str, key: &str, rest: impl FnOnce() -> String) {
    use std::collections::HashSet;
    use std::io::Write;
    static LOG: Mutex<Option<(std::fs::File, HashSet<String>)>> = Mutex::new(None);
    static PATH: OnceLock<Option<std::ffi::OsString>> = OnceLock::new();
    let Some(path) = PATH.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_LOG")) else {
        return;
    };
    let mut log = LOG.lock().unwrap_or_else(|e| e.into_inner());
    if log.is_none() {
        *log = std::fs::File::create(path).ok().map(|f| (f, HashSet::new()));
    }
    let Some((file, seen)) = log.as_mut() else { return };
    if seen.insert(format!("{what} {key}")) {
        let _ = writeln!(file, "{what}\t{key}\t{}", rest());
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads
/// (`VICTORY_PIPELINE_WAIT_MS`, 500 ms by default).
fn frame_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| millis(std::env::var("VICTORY_PIPELINE_WAIT_MS").ok().as_deref(), 500))
}

/// Waiting allowed for a pipeline's first use, outside the frame's budget
/// (`VICTORY_PIPELINE_WAIT_ONCE_MS`, off by default). A pass the title runs
/// once has no later frame in which to get its pipeline.
fn once_budget() -> Duration {
    static ONCE: OnceLock<Duration> = OnceLock::new();
    *ONCE.get_or_init(|| millis(std::env::var("VICTORY_PIPELINE_WAIT_ONCE_MS").ok().as_deref(), 0))
}

/// What `var` asks for in milliseconds, or `default` when it is unset or
/// not a number.
fn millis(value: Option<&str>, default: u64) -> Duration {
    Duration::from_millis(value.and_then(|v| v.trim().parse().ok()).unwrap_or(default))
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    // Listed for an early start in later runs from its first use, made or
    // not: one a run never got made is the one it should start early.
    let first = !p.used.swap(true, Ordering::Relaxed);
    if first {
        super::library::note_used(&p.key);
        #[cfg(feature = "diagnostics")]
        log("used", &p.key, String::new);
    }
    if p.made.get().is_none() {
        start(p, true);
        wait(p, crate::frame::current(), first);
    }
    p.made.get()?.as_ref()
}

/// Queues `p` for the workers: once, and once more ahead of the early
/// starts if a draw waits for it (`now`) while it is still queued.
fn start(p: &Arc<Pending>, now: bool) {
    if let Some(now) = enqueue(p, now) {
        queue(p.clone(), now);
    }
}

/// Which queue `p` goes into now (true: the one draws wait for), if any.
fn enqueue(p: &Pending, now: bool) -> Option<bool> {
    let first = !p.queued.swap(true, Ordering::Relaxed);
    if first {
        PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
    }
    let ahead = now && !p.now.swap(true, Ordering::Relaxed);
    (first || ahead && !p.taken.load(Ordering::Relaxed)).then_some(ahead)
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    super::library::note_created(&p.key);
    if p.made.get().is_none() && !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, false);
    }
}

/// Waits for `p` with its one-shot budget on first use, else while frame
/// `frame` has frame budget left.
fn wait(p: &Pending, frame: u64, first: bool) {
    if first {
        let once = once_budget();
        if !once.is_zero() {
            wait_for(p, once);
            return;
        }
    }
    static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static WAITED_US: AtomicU64 = AtomicU64::new(0);
    if FRAME.swap(frame, Ordering::Relaxed) != frame {
        WAITED_US.store(0, Ordering::Relaxed);
    }
    let left =
        frame_budget().saturating_sub(Duration::from_micros(WAITED_US.load(Ordering::Relaxed)));
    let started = Instant::now();
    wait_for(p, left);
    WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Waits up to `left` for `p` to be made.
fn wait_for(p: &Pending, left: Duration) {
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// Pipelines for the workers: those draws wait for, then those started
/// early.
static QUEUES: (Mutex<[VecDeque<Arc<Pending>>; 2]>, Condvar) =
    (Mutex::new([VecDeque::new(), VecDeque::new()]), Condvar::new());

/// Hands `p` to the workers (half the logical processors, 2 to 8), started
/// on first use.
fn queue(p: Arc<Pending>, now: bool) {
    static WORKERS: OnceLock<usize> = OnceLock::new();
    let workers = *WORKERS.get_or_init(|| {
        let n = std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
        (0..n)
            .filter(|i| {
                let spawned = std::thread::Builder::new()
                    .name(format!("victory-pipelines-{i}"))
                    .spawn(work);
                if let Err(e) = &spawned {
                    tracing::warn!(error = %e, "no pipeline worker");
                }
                spawned.is_ok()
            })
            .count()
    });
    if workers == 0 {
        // No worker: made here instead.
        if !p.taken.swap(true, Ordering::Relaxed) {
            p.make();
            WAITING.fetch_sub(1, Ordering::Relaxed);
        }
        return;
    }
    let (lock, work) = &QUEUES;
    lock.lock().unwrap_or_else(|e| e.into_inner())[usize::from(!now)].push_back(p);
    work.notify_one();
}

/// A worker: makes queued pipelines, those draws wait for first, and saves
/// the cache when idle (first uses of pipelines made before).
fn work() {
    // Below the title's own threads (loading, recording).
    // SAFETY: a plain call on our own thread.
    unsafe {
        use windows::Win32::System::Threading::*;
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    let (lock, work) = &QUEUES;
    loop {
        let p = {
            let mut queues = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(p) = queues.iter_mut().find_map(VecDeque::pop_front) {
                    break p;
                }
                let (held, waited) = work
                    .wait_timeout(queues, Duration::from_secs(20))
                    .unwrap_or_else(|e| e.into_inner());
                queues = held;
                if waited.timed_out() {
                    drop(queues);
                    super::library::save(false);
                    queues = lock.lock().unwrap_or_else(|e| e.into_inner());
                }
            }
        };
        if p.taken.swap(true, Ordering::Relaxed) {
            continue; // its other queue entry
        }
        p.make();
        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
            drained();
        } else {
            super::library::save(false);
        }
    }
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An early start is queued once; a draw waiting for it puts it ahead,
    /// once, unless a worker has taken it.
    #[test]
    fn draws_go_ahead_of_early_starts() {
        let p = Pending::new(
            Vec::new(),
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        );
        assert_eq!(enqueue(&p, false), Some(false));
        assert_eq!(enqueue(&p, false), None);
        assert_eq!(enqueue(&p, true), Some(true));
        assert_eq!(enqueue(&p, true), None);
        let q = Pending::new(
            Vec::new(),
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        );
        assert_eq!(enqueue(&q, false), Some(false));
        q.taken.store(true, Ordering::Relaxed);
        assert_eq!(enqueue(&q, true), None);
        let r = Pending::new(
            Vec::new(),
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        );
        assert_eq!(enqueue(&r, true), Some(true));
        assert_eq!(enqueue(&r, false), None);
        // Counted as waiting by `enqueue`, never handed to a worker.
        WAITING.fetch_sub(3, Ordering::Relaxed);
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(
            Vec::new(),
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));
        assert!(ready(&p).is_none());
        assert!(p.queued.load(Ordering::Relaxed));
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn wait_budgets_read_milliseconds_or_the_default() {
        assert_eq!(millis(Some(" 250 "), 500), Duration::from_millis(250));
        assert_eq!(millis(Some("0"), 500), Duration::ZERO);
        assert_eq!(millis(Some("not a number"), 500), Duration::from_millis(500));
        assert_eq!(millis(None, 500), Duration::from_millis(500));
    }

    /// A draw whose pipeline is still being made when its wait ends has
    /// still used it: the next launch must start it early.
    #[test]
    fn pending_pipeline_is_wanted_for_the_next_launch() {
        let p = Arc::new(Pending::new(
            Vec::new(),
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));
        // Queued and already taken: no worker can make it during the wait.
        p.queued.store(true, Ordering::Relaxed);
        p.taken.store(true, Ordering::Relaxed);
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(crate::pipeline::library::wanted_next_launch(&p.key));
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn skipped_pipeline_stays_off_the_queue() {
        let hashes = crate::pipeline::parse_skip_hashes(" 0123456789ABCDEF ");
        let shader = Arc::new(Shader {
            stage: "compute",
            hash: "0123456789abcdef".into(),
            container: Vec::new(),
            translated: OnceLock::new(),
        });
        let cold = Arc::new(Pending::new(vec![shader.clone()], None, &hashes));
        let warm = Arc::new(Pending::new(vec![shader.clone()], None, &hashes));
        assert!(ready(&cold).is_none());
        crate::pipeline::library::note_used(&warm.key);
        early(&warm);
        assert_eq!(
            (
                cold.queued.load(Ordering::Relaxed),
                warm.queued.load(Ordering::Relaxed)
            ),
            (false, false)
        );
        assert!(matches!(cold.made.get(), Some(None)));
        assert!(matches!(warm.made.get(), Some(None)));
        assert!(shader.translated.get().is_none());
    }
}
