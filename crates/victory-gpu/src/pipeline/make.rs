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
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//! Pipelines a draw waits for go ahead of those started early (at the
//! title's creation, because an earlier run drew them).
//!
//! shortcut: the budget (half a second of waiting per title frame) is a
//! guess; a pass the title runs only once and whose pipeline takes longer
//! is lost.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};
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

/// Waiting allowed per title frame, over all threads.
const BUDGET: Duration = Duration::from_millis(500);

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
    if !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
        #[cfg(feature = "diagnostics")]
        log("used", &p.key, String::new);
    }
    if p.made.get().is_none() {
        start(p, true);
        wait(p, crate::frame::current());
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
    let ahead = now && !p.now.swap(true, Ordering::AcqRel);
    (first || ahead && !p.taken.load(Ordering::Acquire)).then_some(ahead)
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

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static WAITED_US: AtomicU64 = AtomicU64::new(0);
    if FRAME.swap(frame, Ordering::Relaxed) != frame {
        WAITED_US.store(0, Ordering::Relaxed);
    }
    let left = BUDGET.saturating_sub(Duration::from_micros(WAITED_US.load(Ordering::Relaxed)));
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
    WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

enum Work {
    Translate(Arc<Shader>),
    Build(Arc<Pending>),
}

struct Scheduler {
    queues: [VecDeque<Arc<Pending>>; 2],
    /// Pipelines parked on an in-flight shader, keyed by `Arc::as_ptr(shader) as usize`.
    /// Both the parked `Pending`s and the in-flight `Work::Translate` keep an `Arc<Shader>`
    /// alive until `unblock` removes the entry, so this address cannot be reused meanwhile.
    blocked: HashMap<usize, Vec<(Arc<Pending>, bool)>>,
}

impl Scheduler {
    fn new() -> Self {
        Self {
            queues: [VecDeque::new(), VecDeque::new()],
            blocked: HashMap::new(),
        }
    }

    fn has_queued(&self) -> bool {
        !self.queues[0].is_empty() || !self.queues[1].is_empty()
    }

    fn park(&mut self, shader_ptr: usize, p: Arc<Pending>, is_now: bool) {
        let entry = self.blocked.entry(shader_ptr).or_default();
        if let Some((_, was_now)) = entry.iter_mut().find(|(q, _)| Arc::ptr_eq(q, &p)) {
            *was_now |= is_now;
        } else {
            entry.push((p, is_now));
        }
    }

    fn pop(&mut self) -> Option<Work> {
        for (queue_idx, is_now) in [(0, true), (1, false)] {
            while let Some(p) = self.queues[queue_idx].pop_front() {
                if p.taken.load(Ordering::Acquire) {
                    continue;
                }
                let mut wait_on = None;
                for (i, s) in p.shaders.iter().enumerate() {
                    if s.is_done() {
                        continue;
                    }
                    if s.try_claim() {
                        let s = s.clone();
                        if p.shaders[i + 1..].iter().any(|other| other.is_unstarted()) {
                            self.queues[queue_idx].push_front(p);
                        } else {
                            self.park(Arc::as_ptr(&s) as usize, p, is_now);
                        }
                        return Some(Work::Translate(s));
                    }
                    if wait_on.is_none() && !s.is_done() {
                        wait_on = Some(Arc::as_ptr(s) as usize);
                    }
                }
                if let Some(s_ptr) = wait_on {
                    self.park(s_ptr, p, is_now);
                    continue;
                }
                if !p.taken.swap(true, Ordering::AcqRel) {
                    return Some(Work::Build(p));
                }
            }
        }
        None
    }

    fn unblock(&mut self, shader: &Arc<Shader>) {
        let Some(blocked) = self.blocked.remove(&(Arc::as_ptr(shader) as usize)) else {
            return;
        };
        for (p, was_now) in blocked {
            if p.taken.load(Ordering::Acquire) {
                continue;
            }
            let is_now = was_now || p.now.load(Ordering::Acquire);
            let queue = &mut self.queues[usize::from(!is_now)];
            if is_now {
                queue.push_front(p);
            } else {
                queue.push_back(p);
            }
        }
    }
}

/// Pipelines for the workers: those draws wait for, then those started
/// early, plus pipelines parked on an in-flight shader translation.
static SCHEDULER: LazyLock<(Mutex<Scheduler>, Condvar)> =
    LazyLock::new(|| (Mutex::new(Scheduler::new()), Condvar::new()));

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
        if !p.taken.swap(true, Ordering::AcqRel) {
            p.make();
            WAITING.fetch_sub(1, Ordering::Relaxed);
        }
        return;
    }
    let (lock, work) = &*SCHEDULER;
    lock.lock().unwrap_or_else(|e| e.into_inner()).queues[usize::from(!now)].push_back(p);
    work.notify_one();
}

/// A worker: translates shaders and builds queued pipelines, those draws
/// wait for first, and saves the cache when idle (first uses of pipelines
/// made before).
fn work() {
    // Below the title's own threads (loading, recording).
    // SAFETY: a plain call on our own thread.
    unsafe {
        use windows::Win32::System::Threading::*;
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    let (lock, work) = &*SCHEDULER;
    loop {
        let (task, more) = {
            let mut sched = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(task) = sched.pop() {
                    break (task, sched.has_queued());
                }
                let (held, waited) = work
                    .wait_timeout(sched, Duration::from_secs(20))
                    .unwrap_or_else(|e| e.into_inner());
                sched = held;
                if waited.timed_out() {
                    drop(sched);
                    super::library::save(false);
                    sched = lock.lock().unwrap_or_else(|e| e.into_inner());
                }
            }
        };
        if more {
            work.notify_one();
        }
        match task {
            Work::Translate(s) => {
                s.finish_translate();
                {
                    let mut sched = lock.lock().unwrap_or_else(|e| e.into_inner());
                    sched.unblock(&s);
                }
                work.notify_all();
            }
            Work::Build(p) => {
                p.make();
                if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                    drained();
                } else {
                    super::library::save(false);
                }
            }
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
        let p = pending(Vec::new());
        assert!(ready(&p).is_none());
        assert!(p.queued.load(Ordering::Relaxed));
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    fn pending(shaders: Vec<Arc<Shader>>) -> Arc<Pending> {
        Arc::new(Pending::new(
            shaders,
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ))
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
            claimed: AtomicBool::new(false),
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

    #[test]
    fn shared_in_flight_shader_does_not_block_independent_pipelines() {
        let a = Arc::new(Shader::new("vertex", b"shared-shader-a"));
        let b = Arc::new(Shader::new("pixel", b"independent-shader-b"));
        let p1 = pending(vec![a.clone()]);
        let p2 = pending(vec![a.clone()]);
        let p3 = pending(vec![b.clone()]);

        let mut sched = Scheduler::new();
        sched.queues[0].push_back(p1.clone());
        sched.queues[0].push_back(p2.clone());
        sched.queues[0].push_back(p3.clone());

        let Some(Work::Translate(s)) = sched.pop() else {
            panic!("expected Translate(A)");
        };
        assert!(Arc::ptr_eq(&s, &a));

        let Some(Work::Translate(s)) = sched.pop() else {
            panic!("expected Translate(B)");
        };
        assert!(Arc::ptr_eq(&s, &b));
        assert!(sched.pop().is_none());

        a.finish_translate();
        sched.unblock(&a);

        let mut built = Vec::new();
        for _ in 0..2 {
            let Some(Work::Build(p)) = sched.pop() else {
                panic!("expected Build after unblocking A");
            };
            built.push(p);
        }
        assert!(built.iter().any(|p| Arc::ptr_eq(p, &p1)));
        assert!(built.iter().any(|p| Arc::ptr_eq(p, &p2)));
        assert!(sched.pop().is_none());

        b.finish_translate();
        sched.unblock(&b);

        let Some(Work::Build(p)) = sched.pop() else {
            panic!("expected Build for independent pipeline");
        };
        assert!(Arc::ptr_eq(&p, &p3));
        assert!(sched.pop().is_none());
    }

    #[test]
    fn no_lost_wakeup_when_shader_finishes_around_pop() {
        let direct = Arc::new(Shader::new("compute", b"direct-done"));
        assert!(direct.translated().is_none());
        assert!(direct.is_done());
        assert!(!direct.try_claim());
        let p_direct = Arc::new(Pending::new(
            vec![direct],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));
        let mut sched = Scheduler::new();
        sched.queues[0].push_back(p_direct.clone());
        let Some(Work::Build(p)) = sched.pop() else {
            panic!("expected immediate Build when shader already translated");
        };
        assert!(Arc::ptr_eq(&p, &p_direct));
        assert!(sched.blocked.is_empty());

        for _ in 0..64 {
            let s = Arc::new(Shader::new("compute", b"race-shader"));
            let p1 = Arc::new(Pending::new(
                vec![s.clone()],
                None,
                #[cfg(feature = "diagnostics")]
                &[],
            ));
            let p2 = Arc::new(Pending::new(
                vec![s.clone()],
                None,
                #[cfg(feature = "diagnostics")]
                &[],
            ));
            let pair = Arc::new((Mutex::new(Scheduler::new()), Condvar::new()));
            {
                let mut sched = pair.0.lock().unwrap();
                sched.queues[0].push_back(p1.clone());
                let Some(Work::Translate(claimed)) = sched.pop() else {
                    panic!("expected Translate");
                };
                assert!(Arc::ptr_eq(&claimed, &s));
                sched.queues[0].push_back(p2.clone());
            }

            let pair_bg = pair.clone();
            let s_bg = s.clone();
            let finisher = std::thread::spawn(move || {
                s_bg.finish_translate();
                {
                    let mut sched = pair_bg.0.lock().unwrap();
                    sched.unblock(&s_bg);
                }
                pair_bg.1.notify_all();
            });

            let mut built = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut sched = pair.0.lock().unwrap();
            while built.len() < 2 {
                if let Some(work) = sched.pop() {
                    match work {
                        Work::Build(p) => built.push(p),
                        Work::Translate(_) => panic!("shader already claimed"),
                    }
                    continue;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "lost wakeup waiting for unblocked pipelines"
                );
                let (held, _) = pair.1.wait_timeout(sched, left).unwrap();
                sched = held;
            }
            drop(sched);
            finisher.join().unwrap();
            assert!(built.iter().any(|p| Arc::ptr_eq(p, &p1)));
            assert!(built.iter().any(|p| Arc::ptr_eq(p, &p2)));
        }
    }

    #[test]
    fn early_to_now_promotion_of_blocked_pipeline_builds_once() {
        let a = Arc::new(Shader::new("vertex", b"promo-a"));
        let done_a = Arc::new(Shader::new("vertex", b"promo-done-a"));
        done_a.translated();
        let p_promo = Arc::new(Pending::new(
            vec![a.clone()],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));
        let p_early = Arc::new(Pending::new(
            vec![done_a],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));

        assert!(a.try_claim());
        assert_eq!(enqueue(&p_promo, false), Some(false));
        let mut sched = Scheduler::new();
        sched.queues[1].push_back(p_promo.clone());
        assert!(sched.pop().is_none());

        assert_eq!(enqueue(&p_promo, true), Some(true));
        sched.queues[0].push_back(p_promo.clone());
        assert!(sched.pop().is_none());

        let a_ptr = Arc::as_ptr(&a) as usize;
        let blocked = sched.blocked.get(&a_ptr).expect("p_promo parked on A");
        assert_eq!(blocked.len(), 1);
        assert!(Arc::ptr_eq(&blocked[0].0, &p_promo));
        assert!(blocked[0].1);

        sched.queues[1].push_back(p_early.clone());
        a.finish_translate();
        sched.unblock(&a);
        assert_eq!(sched.queues[0].len(), 1);

        let Some(Work::Build(p)) = sched.pop() else {
            panic!("promoted pipeline should build before early pipeline");
        };
        assert!(Arc::ptr_eq(&p, &p_promo));

        let Some(Work::Build(p)) = sched.pop() else {
            panic!("early pipeline should build second");
        };
        assert!(Arc::ptr_eq(&p, &p_early));
        assert!(sched.pop().is_none());

        let b = Arc::new(Shader::new("vertex", b"promo-b"));
        let done_b = Arc::new(Shader::new("vertex", b"promo-done-b"));
        done_b.translated();
        let p_promo2 = Arc::new(Pending::new(
            vec![b.clone()],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));
        let p_early2 = Arc::new(Pending::new(
            vec![done_b],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));

        assert!(b.try_claim());
        assert_eq!(enqueue(&p_promo2, false), Some(false));
        sched.queues[1].push_back(p_promo2.clone());
        assert!(sched.pop().is_none());

        assert_eq!(enqueue(&p_promo2, true), Some(true));
        sched.queues[0].push_back(p_promo2.clone());
        sched.queues[1].push_back(p_early2.clone());

        b.finish_translate();
        sched.unblock(&b);

        let Some(Work::Build(p)) = sched.pop() else {
            panic!("promoted pipeline should build first");
        };
        assert!(Arc::ptr_eq(&p, &p_promo2));

        let Some(Work::Build(p)) = sched.pop() else {
            panic!("duplicate entry should be skipped and early pipeline build once");
        };
        assert!(Arc::ptr_eq(&p, &p_early2));
        assert!(sched.pop().is_none());

        WAITING.fetch_sub(2, Ordering::Relaxed);
    }

    #[test]
    fn multi_shader_pipeline_dispatches_shaders_concurrently() {
        // 1. Single [VS, PS] pipeline: two pop() calls dispatch VS and PS concurrently,
        //    and finishing PS before VS parks on VS until VS finishes.
        let vs = Arc::new(Shader::new("vertex", b"multi-vs-1"));
        let ps = Arc::new(Shader::new("pixel", b"multi-ps-1"));
        let p = Arc::new(Pending::new(
            vec![vs.clone(), ps.clone()],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));

        let mut sched = Scheduler::new();
        sched.queues[0].push_back(p.clone());

        let Some(Work::Translate(s)) = sched.pop() else {
            panic!("expected Translate(VS)");
        };
        assert!(Arc::ptr_eq(&s, &vs));
        assert!(sched.has_queued());

        let Some(Work::Translate(s)) = sched.pop() else {
            panic!("expected Translate(PS) concurrently while VS is in-flight");
        };
        assert!(Arc::ptr_eq(&s, &ps));
        assert!(sched.pop().is_none());

        ps.finish_translate();
        sched.unblock(&ps);
        assert!(sched.pop().is_none());

        vs.finish_translate();
        sched.unblock(&vs);
        let Some(Work::Build(built)) = sched.pop() else {
            panic!("expected Build once both VS and PS finish");
        };
        assert!(Arc::ptr_eq(&built, &p));
        assert!(sched.pop().is_none());

        // 2. Single [VS, PS] pipeline: reverse completion order (VS finishes before PS).
        let vs2 = Arc::new(Shader::new("vertex", b"multi-vs-2"));
        let ps2 = Arc::new(Shader::new("pixel", b"multi-ps-2"));
        let p2 = Arc::new(Pending::new(
            vec![vs2.clone(), ps2.clone()],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));

        let mut sched = Scheduler::new();
        sched.queues[0].push_back(p2.clone());

        let Some(Work::Translate(s)) = sched.pop() else {
            panic!("expected Translate(VS2)");
        };
        assert!(Arc::ptr_eq(&s, &vs2));

        let Some(Work::Translate(s)) = sched.pop() else {
            panic!("expected Translate(PS2)");
        };
        assert!(Arc::ptr_eq(&s, &ps2));
        assert!(sched.pop().is_none());

        vs2.finish_translate();
        sched.unblock(&vs2);
        assert!(sched.pop().is_none());

        ps2.finish_translate();
        sched.unblock(&ps2);
        let Some(Work::Build(built)) = sched.pop() else {
            panic!("expected Build once PS2 finishes after VS2");
        };
        assert!(Arc::ptr_eq(&built, &p2));
        assert!(sched.pop().is_none());

        // 3. Two pipelines sharing VS ([VS] and [VS, PS]): second pop() skips in-flight VS
        //    and dispatches PS rather than blocking PS behind VS.
        let vs3 = Arc::new(Shader::new("vertex", b"multi-vs-3"));
        let ps3 = Arc::new(Shader::new("pixel", b"multi-ps-3"));
        let p_vs = Arc::new(Pending::new(
            vec![vs3.clone()],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));
        let p_vs_ps = Arc::new(Pending::new(
            vec![vs3.clone(), ps3.clone()],
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));

        let mut sched = Scheduler::new();
        sched.queues[0].push_back(p_vs.clone());
        sched.queues[0].push_back(p_vs_ps.clone());

        let Some(Work::Translate(s)) = sched.pop() else {
            panic!("expected Translate(VS3)");
        };
        assert!(Arc::ptr_eq(&s, &vs3));

        let Some(Work::Translate(s)) = sched.pop() else {
            panic!("expected Translate(PS3) while VS3 is already in-flight");
        };
        assert!(Arc::ptr_eq(&s, &ps3));
        assert!(sched.pop().is_none());

        ps3.finish_translate();
        sched.unblock(&ps3);
        assert!(sched.pop().is_none());

        vs3.finish_translate();
        sched.unblock(&vs3);
        let mut built = Vec::new();
        for _ in 0..2 {
            let Some(Work::Build(item)) = sched.pop() else {
                panic!("expected Build for both pipelines after VS3 finishes");
            };
            built.push(item);
        }
        assert!(built.iter().any(|item| Arc::ptr_eq(item, &p_vs)));
        assert!(built.iter().any(|item| Arc::ptr_eq(item, &p_vs_ps)));
        assert!(sched.pop().is_none());
    }

    #[test]
    fn direct_translated_race_with_scheduler_pop_never_deadlocks() {
        for _ in 0..32 {
            let s = Arc::new(Shader::new("compute", b"direct-race-shader"));
            let p = Arc::new(Pending::new(
                vec![s.clone()],
                None,
                #[cfg(feature = "diagnostics")]
                &[],
            ));
            let s_bg = s.clone();
            let direct = std::thread::spawn(move || {
                let _ = s_bg.translated();
            });

            let mut sched = Scheduler::new();
            sched.queues[0].push_back(p.clone());
            match sched.pop() {
                Some(Work::Translate(claimed)) => {
                    assert!(Arc::ptr_eq(&claimed, &s));
                    claimed.finish_translate();
                    sched.unblock(&claimed);
                    let Some(Work::Build(built)) = sched.pop() else {
                        panic!("expected Build after finish_translate and unblock");
                    };
                    assert!(Arc::ptr_eq(&built, &p));
                }
                Some(Work::Build(built)) => {
                    assert!(Arc::ptr_eq(&built, &p));
                }
                None => panic!("expected either Translate or Build without lost wakeup"),
            }
            assert!(sched.pop().is_none());
            direct.join().unwrap();
        }
    }
}
