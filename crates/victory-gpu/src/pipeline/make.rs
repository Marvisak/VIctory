//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and Grand
//! Theft Auto VI's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! Shader translations and ready pipeline builds are scheduled as separate
//! dependency-tracked tasks across two priority tiers (draw-requested demand
//! ahead of speculative early loads). A pipeline whose shaders are still
//! translating registers as a dependent instead of blocking a worker inside a
//! shared `OnceLock`, freeing the remaining workers to translate independent
//! shaders and compile already-unblocked pipelines.
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! shortcut: the budget (half a second of waiting per title frame) is a
//! guess; a pass the title runs only once and whose pipeline takes longer
//! is lost.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// Sentinel placed in `Pending::remaining_deps` before registration completes.
const UNREGISTERED: usize = usize::MAX;

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    deps: OnceLock<Vec<Arc<Shader>>>,
    queued: AtomicBool,
    demand: AtomicBool,
    pipeline_queued_early: AtomicBool,
    pipeline_queued_demand: AtomicBool,
    pipeline_claimed: AtomicBool,
    remaining_deps: AtomicUsize,
    resolved: AtomicBool,
    scheduler: OnceLock<Weak<Scheduler>>,
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
            shaders,
            graphics,
            deps: OnceLock::new(),
            queued: AtomicBool::new(false),
            demand: AtomicBool::new(false),
            pipeline_queued_early: AtomicBool::new(false),
            pipeline_queued_demand: AtomicBool::new(false),
            pipeline_claimed: AtomicBool::new(false),
            remaining_deps: AtomicUsize::new(UNREGISTERED),
            resolved: AtomicBool::new(false),
            scheduler: OnceLock::new(),
            made,
            #[cfg(feature = "diagnostics")]
            skipped_shader,
            key,
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
    }

    fn fail(&self) {
        self.made.get_or_init(|| None);
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
    key.update(
        graphics
            .map(|g| g.describe())
            .unwrap_or_default()
            .as_bytes(),
    );
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
        log("built", key, || {
            format!("{name}\t{}", if loaded { "loaded" } else { "made" })
        });
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
        *log = std::fs::File::create(path)
            .ok()
            .map(|f| (f, HashSet::new()));
    }
    let Some((file, seen)) = log.as_mut() else {
        return;
    };
    if seen.insert(format!("{what} {key}")) {
        let _ = writeln!(file, "{what}\t{key}\t{}", rest());
    }
}

/// Waiting allowed per title frame, over all threads.
const BUDGET: Duration = Duration::from_millis(500);

enum Task {
    Pipeline(Arc<Pending>),
    Shader(Arc<Shader>),
}

#[derive(Default)]
struct QueueState {
    demand_pipelines: VecDeque<Arc<Pending>>,
    demand_shaders: VecDeque<Arc<Shader>>,
    early_pipelines: VecDeque<Arc<Pending>>,
    early_shaders: VecDeque<Arc<Shader>>,
    workers: usize,
    closed: bool,
}

impl QueueState {
    fn pop(&mut self) -> Option<Task> {
        self.demand_pipelines
            .pop_front()
            .map(Task::Pipeline)
            .or_else(|| self.demand_shaders.pop_front().map(Task::Shader))
            .or_else(|| self.early_pipelines.pop_front().map(Task::Pipeline))
            .or_else(|| self.early_shaders.pop_front().map(Task::Shader))
    }
}

struct Scheduler {
    queue: Mutex<QueueState>,
    inflight: Mutex<HashMap<usize, Arc<Pending>>>,
    wake: Condvar,
    done_lock: Mutex<()>,
    done_cv: Condvar,
    waiting: AtomicU64,
    peak: AtomicU64,
    translate_shader: Box<dyn Fn(&Shader) + Send + Sync>,
    build_pipeline: Box<dyn Fn(&Pending) + Send + Sync>,
    on_drained: Box<dyn Fn(&Scheduler) + Send + Sync>,
    on_step: Box<dyn Fn() + Send + Sync>,
}

/// Owns worker threads backing a [`Scheduler`] and joins them when dropped.
struct Pool {
    scheduler: Arc<Scheduler>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl Pool {
    fn new(
        worker_count: usize,
        translate_shader: impl Fn(&Shader) + Send + Sync + 'static,
        build_pipeline: impl Fn(&Pending) + Send + Sync + 'static,
        on_drained: impl Fn(&Scheduler) + Send + Sync + 'static,
        on_step: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let scheduler = Arc::new(Scheduler {
            queue: Mutex::new(QueueState::default()),
            inflight: Mutex::new(HashMap::new()),
            wake: Condvar::new(),
            done_lock: Mutex::new(()),
            done_cv: Condvar::new(),
            waiting: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            translate_shader: Box::new(translate_shader),
            build_pipeline: Box::new(build_pipeline),
            on_drained: Box::new(on_drained),
            on_step: Box::new(on_step),
        });
        let mut handles = Vec::with_capacity(worker_count);
        for i in 0..worker_count {
            let sched = scheduler.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(move || {
                    // Below the title's own threads (loading, recording).
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    sched.worker_loop();
                });
            match spawned {
                Ok(h) => handles.push(h),
                Err(e) => tracing::warn!(error = %e, "no pipeline worker"),
            }
        }
        scheduler
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .workers = handles.len();
        Pool {
            scheduler,
            handles: Mutex::new(handles),
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let handles = {
            let mut q = self
                .scheduler
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            q.closed = true;
            self.scheduler.wake.notify_all();
            std::mem::take(&mut *self.handles.lock().unwrap_or_else(|e| e.into_inner()))
        };
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Scheduler {
    fn worker_loop(&self) {
        loop {
            let task = {
                let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(task) = q.pop() {
                        break task;
                    }
                    if q.closed {
                        return;
                    }
                    q = self.wake.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            match task {
                Task::Pipeline(p) => self.run_pipeline(&p),
                Task::Shader(s) => {
                    if s.claim_scheduled() {
                        (self.translate_shader)(&s);
                    }
                }
            }
        }
    }

    fn note_queued(&self) {
        let count = self.waiting.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(count, Ordering::Relaxed);
    }

    fn resolve_pending(&self, p: &Pending, set_made: impl FnOnce()) {
        set_made();
        if !p.resolved.swap(true, Ordering::AcqRel) {
            let remaining = self.waiting.fetch_sub(1, Ordering::AcqRel) - 1;
            {
                let _held = self.done_lock.lock().unwrap_or_else(|e| e.into_inner());
                self.done_cv.notify_all();
            }
            if remaining == 0 {
                (self.on_drained)(self);
            } else {
                (self.on_step)();
            }
            self.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(p as *const Pending as usize));
        }
    }

    fn run_pipeline(&self, p: &Pending) {
        if p.resolved.load(Ordering::Acquire) || p.pipeline_claimed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.resolve_pending(p, || (self.build_pipeline)(p));
    }

    fn push_shader(&self, s: Arc<Shader>, demand: bool) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.workers == 0 {
            drop(q);
            if s.claim_scheduled() {
                (self.translate_shader)(&s);
            }
            return;
        }
        if demand {
            q.demand_shaders.push_back(s);
        } else {
            q.early_shaders.push_back(s);
        }
        self.wake.notify_one();
    }

    fn enqueue_pipeline(&self, p: &Arc<Pending>, demand: bool) {
        if p.resolved.load(Ordering::Acquire) {
            return;
        }
        if demand {
            if p.pipeline_queued_demand.swap(true, Ordering::AcqRel) {
                return;
            }
        } else if p.pipeline_queued_early.swap(true, Ordering::AcqRel)
            || p.pipeline_queued_demand.load(Ordering::Acquire)
        {
            return;
        }
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.workers == 0 {
            drop(q);
            self.run_pipeline(p);
            return;
        }
        if demand {
            q.demand_pipelines.push_back(p.clone());
        } else {
            q.early_pipelines.push_back(p.clone());
        }
        self.wake.notify_one();
    }

    fn start(self: &Arc<Self>, p: &Arc<Pending>, demand: bool) {
        self.start_with_deps(p, demand, || super::host::required_shaders(&p.shaders));
    }

    fn start_with_deps(
        self: &Arc<Self>,
        p: &Arc<Pending>,
        demand: bool,
        resolve_deps: impl FnOnce() -> Vec<Arc<Shader>>,
    ) {
        let promoted = demand && !p.demand.swap(true, Ordering::AcqRel);
        if !p.queued.swap(true, Ordering::AcqRel) {
            let _ = p.scheduler.set(Arc::downgrade(self));
            self.note_queued();
            self.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(Arc::as_ptr(p) as usize, p.clone());
            let deps = p.deps.get_or_init(resolve_deps);
            if deps.is_empty() {
                p.remaining_deps.store(0, Ordering::Release);
                self.enqueue_pipeline(p, p.demand.load(Ordering::Acquire));
                return;
            }
            // Hold a +1 registration guard so concurrent shader completions
            // cannot enqueue `p` before every dependency has been checked.
            p.remaining_deps.store(deps.len() + 1, Ordering::Release);
            let initial_demand = p.demand.load(Ordering::Acquire);
            let mut done_ok = 0usize;
            let mut to_enqueue = Vec::new();
            for s in deps {
                let (done, enqueue) = s.register_waiter(p, initial_demand);
                if let Some(d) = enqueue {
                    to_enqueue.push((s.clone(), d));
                }
                match done {
                    Some(true) => done_ok += 1,
                    Some(false) => {
                        for (shader, d) in to_enqueue {
                            self.push_shader(shader, d);
                        }
                        self.resolve_pending(p, || p.fail());
                        return;
                    }
                    None => {}
                }
            }
            // If `ready()` promoted `p` while dependencies were still being
            // registered under `initial_demand == false`, ensure all shaders get
            // demand priority.
            if !initial_demand && p.demand.load(Ordering::Acquire) {
                for s in deps {
                    if s.promote_to_demand() {
                        if let Some((_, priority)) = to_enqueue
                            .iter_mut()
                            .find(|(queued, _)| Arc::ptr_eq(queued, s))
                        {
                            *priority = true;
                        } else {
                            to_enqueue.push((s.clone(), true));
                        }
                    }
                }
            }
            for (s, d) in to_enqueue {
                self.push_shader(s, d);
            }
            let release = done_ok + 1;
            let prev = p.remaining_deps.fetch_sub(release, Ordering::AcqRel);
            if prev == release && !p.resolved.load(Ordering::Acquire) {
                self.enqueue_pipeline(p, p.demand.load(Ordering::Acquire));
            }
        } else if promoted && !p.resolved.load(Ordering::Acquire) {
            if let Some(deps) = p.deps.get() {
                for s in deps {
                    if s.promote_to_demand() {
                        self.push_shader(s.clone(), true);
                    }
                }
            }
            if p.remaining_deps.load(Ordering::Acquire) == 0 && !p.resolved.load(Ordering::Acquire)
            {
                self.enqueue_pipeline(p, true);
            }
        }
    }

    fn on_shader_done(self: &Arc<Self>, p: &Arc<Pending>, ok: bool) {
        if p.resolved.load(Ordering::Acquire) {
            return;
        }
        if !ok {
            self.resolve_pending(p, || p.fail());
            return;
        }
        let prev = p.remaining_deps.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 && !p.resolved.load(Ordering::Acquire) {
            self.enqueue_pipeline(p, p.demand.load(Ordering::Acquire));
        }
    }

    fn wait(&self, p: &Pending, frame: u64) {
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        static WAITED_US: AtomicU64 = AtomicU64::new(0);
        if FRAME.swap(frame, Ordering::Relaxed) != frame {
            WAITED_US.store(0, Ordering::Relaxed);
        }
        let left = BUDGET.saturating_sub(Duration::from_micros(WAITED_US.load(Ordering::Relaxed)));
        let started = Instant::now();
        let mut held = self.done_lock.lock().unwrap_or_else(|e| e.into_inner());
        while p.made.get().is_none() {
            let rest = left.saturating_sub(started.elapsed());
            if rest.is_zero() {
                break;
            }
            held = self
                .done_cv
                .wait_timeout(held, rest)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        WAITED_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

fn global() -> &'static Arc<Scheduler> {
    static POOL: OnceLock<Pool> = OnceLock::new();
    &POOL
        .get_or_init(|| {
            let workers =
                std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).clamp(2, 8));
            Pool::new(
                workers,
                |s| {
                    let _ = s.translated();
                },
                |p| p.make(),
                drained,
                || super::library::save(false),
            )
        })
        .scheduler
}

/// Called by [`Shader::finish_translation`] when a shader finishes translating.
pub(crate) fn on_shader_done(p: &Arc<Pending>, ok: bool) {
    if let Some(sched) = p.scheduler.get().and_then(Weak::upgrade) {
        sched.on_shader_done(p, ok);
    }
}

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    global().waiting.load(Ordering::Relaxed)
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
        let sched = global();
        sched.start(p, true);
        sched.wait(p, crate::frame::current());
    }
    p.made.get()?.as_ref()
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    super::library::note_created(&p.key);
    if p.made.get().is_none() && !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        global().start(p, false);
    }
}

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained(sched: &Scheduler) {
    use super::library::{EARLY, LOADED, MADE};
    if sched.peak.swap(0, Ordering::Relaxed) >= 10 {
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
    use std::sync::{Barrier, mpsc};

    fn pending_with(deps: Vec<Arc<Shader>>) -> Arc<Pending> {
        let p = Arc::new(Pending::new(
            deps.clone(),
            None,
            #[cfg(feature = "diagnostics")]
            &[],
        ));
        let _ = p.deps.set(deps);
        p
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

    #[cfg(feature = "diagnostics")]
    #[test]
    fn skipped_pipeline_stays_off_the_queue() {
        let hashes = crate::pipeline::parse_skip_hashes(" 0123456789ABCDEF ");
        let shader = Shader::from_parts(
            "compute",
            "0123456789abcdef".into(),
            Vec::new(),
            None,
        );
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
    fn shared_slow_shader_does_not_block_independent_pipelines() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let built_count = Arc::new(AtomicUsize::new(0));
        let independent_done = Arc::new(AtomicUsize::new(0));
        let shared_vs = Shader::test_only("vertex", "slow_shared");
        let shared_hash_for_worker = shared_vs.hash.clone();
        let independent_done_clone = independent_done.clone();
        let built_count_clone = built_count.clone();

        let pool = Pool::new(
            4,
            move |s| {
                if s.hash == shared_hash_for_worker {
                    let _ = started_tx.send(());
                    let _ = release_rx.lock().unwrap().recv();
                }
                s.finish_translation(true);
            },
            move |p| {
                built_count_clone.fetch_add(1, Ordering::Relaxed);
                if p.shaders.len() == 1 && p.shaders[0].stage == "compute" {
                    independent_done_clone.fetch_add(1, Ordering::Relaxed);
                }
                p.fail();
            },
            |_| {},
            || {},
        );

        let mut waiting_ps = Vec::new();
        for i in 0..8 {
            let ps = Shader::test_only("pixel", &format!("ps_{i}"));
            let p = pending_with(vec![shared_vs.clone(), ps]);
            pool.scheduler
                .start_with_deps(&p, true, || p.shaders.clone());
            waiting_ps.push(p);
        }

        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let mut independent = Vec::new();
        for i in 0..12 {
            let cs = Shader::test_only("compute", &format!("ind_{i}"));
            let p = pending_with(vec![cs]);
            pool.scheduler
                .start_with_deps(&p, true, || p.shaders.clone());
            independent.push(p);
        }

        // All 12 independent pipelines must complete while the shared slow shader is still blocked.
        let deadline = Instant::now() + Duration::from_secs(5);
        while independent_done.load(Ordering::Relaxed) < 12 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(independent_done.load(Ordering::Relaxed), 12);
        assert!(
            waiting_ps
                .iter()
                .all(|p| !p.resolved.load(Ordering::Acquire))
        );

        // Release the slow shared shader and verify all 8 dependent pipelines finish cleanly.
        let _ = release_tx.send(());
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.scheduler.waiting.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(pool.scheduler.waiting.load(Ordering::Relaxed), 0);
        assert_eq!(built_count.load(Ordering::Relaxed), 20);
        assert!(waiting_ps.iter().all(|p| p.made.get().is_some()));
    }

    #[test]
    fn failed_shader_fast_fails_dependent_pipelines_without_building() {
        let built = Arc::new(AtomicUsize::new(0));
        let built_clone = built.clone();
        let bad_vs = Shader::test_only("vertex", "bad_vs");
        let bad_hash_for_worker = bad_vs.hash.clone();
        let ok_gate = Arc::new(Barrier::new(2));
        let ok_gate_worker = ok_gate.clone();

        let pool = Pool::new(
            2,
            move |s| {
                let ok = if s.hash == bad_hash_for_worker {
                    false
                } else {
                    ok_gate_worker.wait();
                    true
                };
                s.finish_translation(ok);
            },
            move |p| {
                built_clone.fetch_add(1, Ordering::Relaxed);
                p.fail();
            },
            |_| {},
            || {},
        );

        let slow_ps = Shader::test_only("pixel", "slow_ps");
        let p = pending_with(vec![bad_vs, slow_ps]);
        pool.scheduler
            .start_with_deps(&p, true, || p.shaders.clone());

        let deadline = Instant::now() + Duration::from_secs(5);
        while (!p.resolved.load(Ordering::Acquire)
            || pool.scheduler.waiting.load(Ordering::Relaxed) > 0)
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
        assert_eq!(pool.scheduler.waiting.load(Ordering::Relaxed), 0);
        assert_eq!(built.load(Ordering::Relaxed), 0);

        ok_gate.wait();
    }

    #[test]
    fn demand_pipelines_jump_ahead_of_queued_early_work() {
        let (hold_tx, hold_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let go_rx = Arc::new(Mutex::new(go_rx));
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_clone = order.clone();
        let gate_cs = Shader::test_only("compute", "gate");
        let gate_hash_worker = gate_cs.hash.clone();

        let pool = Pool::new(
            1,
            move |s| {
                if s.hash == gate_hash_worker {
                    let _ = hold_tx.send(());
                    let _ = go_rx.lock().unwrap().recv();
                }
                s.finish_translation(true);
            },
            move |p| {
                order_clone.lock().unwrap().push(p.shaders[0].hash.clone());
                p.fail();
            },
            |_| {},
            || {},
        );

        let early_cs = Shader::test_only("compute", "early");
        let promoted_cs = Shader::test_only("compute", "promoted");
        let demand_cs = Shader::test_only("compute", "demand");
        let p_gate = pending_with(vec![gate_cs.clone()]);
        let p_early = pending_with(vec![early_cs.clone()]);
        let p_promoted = pending_with(vec![promoted_cs.clone()]);
        let p_demand = pending_with(vec![demand_cs.clone()]);

        pool.scheduler
            .start_with_deps(&p_gate, false, || p_gate.shaders.clone());
        hold_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        pool.scheduler
            .start_with_deps(&p_early, false, || p_early.shaders.clone());
        pool.scheduler
            .start_with_deps(&p_promoted, false, || p_promoted.shaders.clone());
        pool.scheduler
            .start_with_deps(&p_demand, true, || p_demand.shaders.clone());
        // Promoting an already-queued early pipeline should move it ahead of remaining early work.
        pool.scheduler
            .start_with_deps(&p_promoted, true, || p_promoted.shaders.clone());

        let _ = go_tx.send(());
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.scheduler.waiting.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            *order.lock().unwrap(),
            vec![
                demand_cs.hash.clone(),
                promoted_cs.hash.clone(),
                gate_cs.hash.clone(),
                early_cs.hash.clone(),
            ]
        );
    }

    #[test]
    fn dropped_title_pipeline_does_not_leak_waiting_count() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let gate = Shader::test_only("compute", "lifetime_gate");
        let gate_hash = gate.hash.clone();
        let pool = Pool::new(
            1,
            move |s| {
                if s.hash == gate_hash {
                    let _ = entered_tx.send(());
                    let _ = release_rx.lock().unwrap().recv();
                }
                s.finish_translation(true);
            },
            |p| p.fail(),
            |_| {},
            || {},
        );
        let sched = pool.scheduler.clone();
        let blocker = pending_with(vec![gate]);
        sched.start_with_deps(&blocker, false, || blocker.shaders.clone());
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let target = pending_with(vec![Shader::test_only("compute", "lifetime_target")]);
        sched.start_with_deps(&target, false, || target.shaders.clone());
        let before = sched.waiting.load(Ordering::Acquire);
        drop(target);
        let _ = release_tx.send(());
        drop(pool);
        assert_eq!(before, 2);
        assert!(blocker.resolved.load(Ordering::Acquire));
        assert_eq!(sched.waiting.load(Ordering::Acquire), 0);
    }
}
