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
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let key = key(&shaders, graphics.as_deref());
        #[cfg(feature = "diagnostics")]
        log("created", &key, || {
            let state = graphics.as_ref().map(|g| g.describe()).unwrap_or_default();
            format!("{shaders:?}\t{state}")
        });
        Pending {
            key,
            shaders,
            graphics,
            queued: AtomicBool::new(false),
            now: AtomicBool::new(false),
            taken: AtomicBool::new(false),
            made: OnceLock::new(),
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
    let ahead = now && !p.now.swap(true, Ordering::Relaxed);
    (first || ahead && !p.taken.load(Ordering::Relaxed)).then_some(ahead)
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    super::library::note_created(&p.key);
    if !p.shaders.is_empty() && super::library::wanted_early(&p.key) {
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
        let _guard = serial();
        let p = Pending::new(Vec::new(), None);
        assert_eq!(enqueue(&p, false), Some(false));
        assert_eq!(enqueue(&p, false), None);
        assert_eq!(enqueue(&p, true), Some(true));
        assert_eq!(enqueue(&p, true), None);
        let q = Pending::new(Vec::new(), None);
        assert_eq!(enqueue(&q, false), Some(false));
        q.taken.store(true, Ordering::Relaxed);
        assert_eq!(enqueue(&q, true), None);
        let r = Pending::new(Vec::new(), None);
        assert_eq!(enqueue(&r, true), Some(true));
        assert_eq!(enqueue(&r, false), None);
        // Counted as waiting by `enqueue`, never handed to a worker.
        WAITING.fetch_sub(3, Ordering::Relaxed);
    }

    /// Runs the tests in this module that use the shared queue one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A title shader container (no code, so it never translates).
    fn shader(stage: &'static str, hash: &str) -> Arc<Shader> {
        Arc::new(Shader {
            stage,
            hash: hash.to_owned(),
            container: Vec::new(),
            translated: OnceLock::new(),
        })
    }

    /// Waits for the workers to finish everything queued.
    fn idle() {
        while waiting() > 0 {
            std::thread::yield_now();
        }
    }

    /// The key of a pipeline with the default graphics state, as `f` leaves
    /// it.
    fn state_key(f: impl FnOnce(&mut GraphicsState)) -> String {
        let mut g = GraphicsState::default();
        f(&mut g);
        Pending::new(Vec::new(), Some(Box::new(g))).key
    }

    /// Reads a synthetic pipeline-state subobject into `g`, as a stream
    /// hands one over: its type, then its description.
    fn sub<T>(g: &mut GraphicsState, kind: u32, value: T) {
        #[repr(C)]
        struct Sub<T> {
            kind: u32,
            value: T,
        }
        let sub = Sub { kind, value };
        // SAFETY: a whole subobject of type `kind`: its description follows
        // the type at +4, or at +8 for those holding pointers (where
        // `repr(C)` puts them), as `GraphicsState::read` expects.
        unsafe { g.read(kind, std::ptr::from_ref(&sub).cast()) };
    }

    /// A first use queues the pipeline for a worker (here: no host pipeline
    /// for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let _serial = serial();
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert!(p.queued.load(Ordering::Relaxed));
        assert!(p.made.wait().is_none());
        idle();
    }

    /// The key names the shaders, so the pipelines a run drew with are found
    /// again by the next run's early start.
    #[test]
    fn keys_identify_the_shaders_a_pipeline_is_made_from() {
        let vs = shader("vertex", "0123456789abcdef");
        let ps = shader("pixel", "fedcba9876543210");
        let key = |shaders: Vec<Arc<Shader>>| Pending::new(shaders, None).key;
        assert_eq!(
            key(vec![vs.clone(), ps.clone()]),
            key(vec![vs.clone(), ps.clone()])
        );
        assert_ne!(key(vec![vs.clone()]), key(vec![vs.clone(), ps.clone()]));
        assert_ne!(
            key(vec![vs.clone()]),
            key(vec![shader("pixel", "0123456789abcdef")])
        );
        assert_ne!(
            key(vec![vs.clone()]),
            key(vec![shader("vertex", "fedcba9876543210")])
        );
        // No state at all (a compute pipeline) is not the default state.
        assert_ne!(
            key(vec![vs.clone()]),
            Pending::new(vec![vs], Some(Box::new(GraphicsState::default()))).key
        );
        let empty = key(Vec::new());
        assert_eq!(
            empty.len(),
            32,
            "sixteen bytes, as the early list holds them"
        );
        assert!(empty.bytes().all(|b| b.is_ascii_hexdigit()), "{empty}");
    }

    /// Every part of the graphics state is in the key: a pipeline must never
    /// be started from the host pipeline of another state.
    #[test]
    fn keys_identify_every_part_of_the_graphics_state() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::core::PCSTR;

        // A subobject's type as the stream that holds it writes it.
        let kind = |t: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE| t.0 as u32;
        let base = state_key(|_| {});
        assert_eq!(base, state_key(|_| {}));
        let element = [D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(c"POSITION".as_ptr().cast()),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32B32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        }];
        let changes: [(&str, String); 10] = [
            (
                "blend",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND),
                        D3D12_BLEND_DESC {
                            IndependentBlendEnable: true.into(),
                            ..Default::default()
                        },
                    )
                }),
            ),
            (
                "sample mask",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK),
                        0xffu32,
                    )
                }),
            ),
            (
                "rasterizer",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER),
                        D3D12_RASTERIZER_DESC {
                            CullMode: D3D12_CULL_MODE_FRONT,
                            ..Default::default()
                        },
                    )
                }),
            ),
            (
                "depth-stencil",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL),
                        D3D12_DEPTH_STENCIL_DESC {
                            DepthFunc: D3D12_COMPARISON_FUNC_GREATER,
                            ..Default::default()
                        },
                    )
                }),
            ),
            (
                "input layout",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT),
                        D3D12_INPUT_LAYOUT_DESC {
                            pInputElementDescs: element.as_ptr(),
                            NumElements: element.len() as u32,
                        },
                    )
                }),
            ),
            (
                "strip cut",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE),
                        D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_0xFFFF,
                    )
                }),
            ),
            (
                "topology",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY),
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE_LINE,
                    )
                }),
            ),
            (
                "render targets",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS),
                        D3D12_RT_FORMAT_ARRAY {
                            NumRenderTargets: 1,
                            RTFormats: [DXGI_FORMAT_R8G8B8A8_UNORM; 8],
                        },
                    )
                }),
            ),
            (
                "depth format",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT),
                        DXGI_FORMAT_D32_FLOAT,
                    )
                }),
            ),
            (
                "samples",
                state_key(|g| {
                    sub(
                        g,
                        kind(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC),
                        DXGI_SAMPLE_DESC {
                            Count: 4,
                            Quality: 0,
                        },
                    )
                }),
            ),
        ];
        for (what, key) in changes {
            assert_ne!(key, base, "the key must change with the {what}");
        }
    }

    /// A second use of a refused pipeline is answered at once: it is neither
    /// queued nor waited for again.
    #[test]
    fn a_made_pipeline_is_not_queued_again() {
        let _serial = serial();
        idle();
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(p.made.set(None).is_ok());
        assert!(ready(&p).is_none());
        assert!(ready(&p).is_none());
        assert!(!p.queued.load(Ordering::Relaxed), "it was never queued");
        assert_eq!(waiting(), 0);
    }

    /// A first use queues the pipeline once, and its making leaves nothing
    /// waiting (the frame line's count comes back to zero).
    #[test]
    fn queueing_once_leaves_nothing_waiting() {
        let _serial = serial();
        idle();
        let p = Arc::new(Pending::new(Vec::new(), None));
        start(&p, false);
        start(&p, false);
        assert!(p.queued.load(Ordering::Relaxed));
        p.made.wait();
        idle();
    }

    /// A pipeline whose shaders do not translate is refused once, not queued
    /// for again on every draw.
    #[test]
    fn a_pipeline_whose_shaders_do_not_translate_is_refused_once() {
        let _serial = serial();
        idle();
        let p = Arc::new(Pending::new(
            vec![shader("vertex", "0123456789abcdef")],
            None,
        ));
        assert!(ready(&p).is_none());
        assert!(p.made.wait().is_none());
        assert!(ready(&p).is_none());
        idle();
    }

    /// A pipeline is marked used on its first draw, even if building it is
    /// refused, so later runs know to start it early.
    #[test]
    fn a_refused_pipeline_is_marked_used_on_first_use() {
        let _serial = serial();
        idle();
        let p = Arc::new(Pending::new(
            vec![shader("vertex", "aaaaaaaaaaaaaaaa")],
            None,
        ));
        assert!(ready(&p).is_none());
        assert!(p.made.wait().is_none());
        assert!(p.used.load(Ordering::Relaxed));
        idle();
    }

    /// At creation a pipeline that was never drawn with in an earlier run is
    /// not queued early.
    #[test]
    fn early_does_not_queue_an_unseen_pipeline() {
        let _serial = serial();
        idle();
        let unused = Arc::new(Pending::new(
            vec![shader("vertex", "2222222222222222")],
            None,
        ));
        early(&unused);
        assert!(
            !unused.queued.load(Ordering::Relaxed),
            "it was never drawn with"
        );
        idle();
    }

    /// A pipeline that is being made says so (diagnostics).
    #[cfg(feature = "diagnostics")]
    #[test]
    fn a_pipeline_being_made_says_so() {
        let p = Pending::new(Vec::new(), None);
        assert_eq!(missing(&p).as_deref(), Some("being made"));
    }

    /// A refused pipeline names the shaders that do not translate
    /// (diagnostics).
    #[cfg(feature = "diagnostics")]
    #[test]
    fn a_refused_pipeline_names_the_shaders_that_do_not_translate() {
        let vs = shader("vertex", "0123456789abcdef");
        let ps = shader("pixel", "fedcba9876543210");
        assert!(vs.translated.set(None).is_ok());
        let p = Pending::new(vec![vs, ps], None);
        assert!(p.made.set(None).is_ok());
        assert_eq!(missing(&p).as_deref(), Some("vertex 0123456789abcdef"));
    }

    /// A pipeline refused for its state, whose shaders all translate, names
    /// its shaders instead (diagnostics).
    #[cfg(feature = "diagnostics")]
    #[test]
    fn a_pipeline_refused_for_its_state_says_refused() {
        let p = Pending::new(vec![shader("vertex", "0123456789abcdef")], None);
        assert!(p.made.set(None).is_ok());
        let why = missing(&p).expect("a reason");
        assert!(why.starts_with("refused "), "{why}");
        assert!(why.contains("vertex 0123456789abcdef"), "{why}");
    }
}
