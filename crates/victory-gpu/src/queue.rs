//! Command queues. Submitted lists run on the host GPU and are waited for,
//! so fence signals complete at once.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use crate::list::{CpuOp, ListStep, MemoryWait};
use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUEUE, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const QUEUE_IIDS: [Iid; 4] = [
    guid(
        0x0ec870a6,
        0x5d7e,
        0x4c22,
        [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDesc {
    kind: u32,
    priority: i32,
    flags: u32,
    nodes: u32,
}

#[cfg(windows)]
type HostSegment = (Arc<crate::host::HostList>, Arc<Vec<Vec<u8>>>);

/// One queued item on a guest command queue waiting behind an unsatisfied
/// 64-bit command-list memory wait.
enum PendingAction {
    #[cfg(windows)]
    Host(HostSegment),
    Cpu(Vec<CpuOp>),
    Wait(MemoryWait),
    /// Retained guest fence pointer (`AddRef`'d on enqueue, `Release`'d after signal).
    Signal(usize, u64),
}

impl From<ListStep> for PendingAction {
    fn from(step: ListStep) -> Self {
        match step {
            #[cfg(windows)]
            ListStep::Host { list, uploads } => PendingAction::Host((list, uploads)),
            ListStep::Cpu(ops) => PendingAction::Cpu(ops),
            ListStep::Wait(w) => PendingAction::Wait(w),
        }
    }
}

#[derive(Default)]
struct QueuePending {
    items: VecDeque<PendingAction>,
    running: bool,
}

impl QueuePending {
    fn is_busy(&self) -> bool {
        self.running || !self.items.is_empty()
    }
}

/// Per-queue pending submission state, kept alive by `Arc` until drained.
#[derive(Default)]
struct QueueState {
    busy: AtomicBool,
    inner: Mutex<QueuePending>,
}

/// Number of queues currently registered with the wait scheduler.
static PENDING_QUEUES: AtomicUsize = AtomicUsize::new(0);

/// Background scheduler that polls/wakes pending command-list memory waits
/// without blocking the submitting CPU thread.
struct WaitScheduler {
    queues: Mutex<Vec<Arc<QueueState>>>,
    wake: (Mutex<u64>, Condvar),
}

static SCHEDULER: OnceLock<WaitScheduler> = OnceLock::new();

fn scheduler() -> &'static WaitScheduler {
    SCHEDULER.get_or_init(|| {
        let s = WaitScheduler {
            queues: Mutex::new(Vec::new()),
            wake: (Mutex::new(0), Condvar::new()),
        };
        let _ = std::thread::Builder::new()
            .name("victory-gpu-wait".into())
            .spawn(wait_worker);
        s
    })
}

fn register_pending(state: Arc<QueueState>) {
    let s = scheduler();
    let mut queues = s.queues.lock().unwrap_or_else(|e| e.into_inner());
    if !queues.iter().any(|q| Arc::ptr_eq(q, &state)) {
        queues.push(state);
        PENDING_QUEUES.store(queues.len(), Ordering::Release);
    }
    drop(queues);
    wake_scheduler(s);
}

fn wake_scheduler(s: &WaitScheduler) {
    let mut epoch = s.wake.0.lock().unwrap_or_else(|e| e.into_inner());
    *epoch = epoch.wrapping_add(1);
    s.wake.1.notify_all();
}

/// Wakes any command-list memory waits that may have been satisfied by a
/// GPU submission, copy-back, or bottom-of-pipe write.
pub(crate) fn notify_memory_waiters() {
    if PENDING_QUEUES.load(Ordering::Acquire) == 0 {
        return;
    }
    if let Some(s) = SCHEDULER.get() {
        wake_scheduler(s);
    }
}

fn release_fence(fence: This) {
    crate::fence::release(fence);
}

#[cfg(windows)]
fn run_host_batch(batch: &[HostSegment]) {
    if batch.is_empty() {
        return;
    }
    if let Some(host) = crate::host::host() {
        let refs: Vec<&crate::host::HostList> = batch.iter().map(|(l, _)| l.as_ref()).collect();
        host.execute(&refs);
    }
}

fn run_cpu_ops(ops: &[CpuOp]) {
    for &op in ops {
        op.run();
    }
    notify_memory_waiters();
}

/// Advances `q` as far as ready actions allow without holding `q.inner` while
/// executing host GPU submissions or fence signals. Returns `true` if any
/// item made progress.
fn drain_ready(q: &QueueState) -> bool {
    let mut progressed = false;
    loop {
        #[cfg(windows)]
        let mut host_batch: Vec<HostSegment> = Vec::new();
        let mut next_other = None;
        {
            let mut inner = q.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.running {
                return progressed;
            }
            loop {
                match inner.items.front() {
                    Some(PendingAction::Wait(w)) => {
                        #[cfg(windows)]
                        if !host_batch.is_empty() {
                            break;
                        }
                        if w.satisfied() {
                            inner.items.pop_front();
                            progressed = true;
                            continue;
                        }
                        q.busy.store(inner.is_busy(), Ordering::Release);
                        return progressed;
                    }
                    #[cfg(windows)]
                    Some(PendingAction::Host(_)) => {
                        if let Some(PendingAction::Host(seg)) = inner.items.pop_front() {
                            host_batch.push(seg);
                        }
                    }
                    Some(PendingAction::Cpu(_)) | Some(PendingAction::Signal(..)) => {
                        #[cfg(windows)]
                        if !host_batch.is_empty() {
                            break;
                        }
                        next_other = inner.items.pop_front();
                        break;
                    }
                    None => {
                        #[cfg(windows)]
                        if !host_batch.is_empty() {
                            break;
                        }
                        q.busy.store(false, Ordering::Release);
                        return progressed;
                    }
                }
            }
            inner.running = true;
            q.busy.store(true, Ordering::Release);
        }

        progressed = true;
        #[cfg(windows)]
        if !host_batch.is_empty() {
            run_host_batch(&host_batch);
        }
        match next_other {
            Some(PendingAction::Cpu(ops)) => run_cpu_ops(&ops),
            Some(PendingAction::Signal(fence, value)) => {
                crate::fence::signal(fence as This, value);
                release_fence(fence as This);
            }
            _ => {}
        }

        let mut inner = q.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.running = false;
        q.busy.store(inner.is_busy(), Ordering::Release);
    }
}

fn wait_worker() {
    let s = scheduler();
    loop {
        let epoch = *s.wake.0.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let snapshot: Vec<Arc<QueueState>> = {
                let queues = s.queues.lock().unwrap_or_else(|e| e.into_inner());
                queues.clone()
            };
            let mut progressed = false;
            for q in &snapshot {
                progressed |= drain_ready(q);
            }
            if !progressed {
                break;
            }
        }
        let still_waiting = {
            let mut queues = s.queues.lock().unwrap_or_else(|e| e.into_inner());
            queues.retain(|q| q.busy.load(Ordering::Acquire));
            let len = queues.len();
            PENDING_QUEUES.store(len, Ordering::Release);
            len > 0
        };
        let lock = s.wake.0.lock().unwrap_or_else(|e| e.into_inner());
        if *lock != epoch {
            continue;
        }
        if still_waiting {
            // Bounded poll fallback so direct CPU memory writes also wake promptly.
            drop(
                s.wake
                    .1
                    .wait_timeout(lock, Duration::from_millis(1))
                    .unwrap_or_else(|e| e.into_inner()),
            );
        } else {
            drop(s.wake.1.wait(lock).unwrap_or_else(|e| e.into_inner()));
        }
    }
}

/// The title reads the list type right after the header.
#[repr(C)]
struct Queue {
    object: Object,
    kind: u32,
    desc: QueueDesc,
    state: Arc<QueueState>,
}
const _: () = assert!(std::mem::offset_of!(Queue, kind) == 0x10);

fn queue<'a>(this: This) -> &'a Queue {
    // SAFETY: called from queue methods only.
    unsafe { &*this.cast::<Queue>() }
}

pub(crate) extern "system" fn create(
    _: This,
    desc: *const QueueDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "command queue");
    let queue = Box::new(Queue {
        object: if crate::device::newer() {
            Object::new(&NEWER_FUNCTIONS)
        } else {
            Object::new(&FUNCTIONS)
        },
        kind: desc.kind,
        desc,
        state: Arc::new(QueueState::default()),
    });
    object::hand_out(QUEUE, queue, iid, &QUEUE_IIDS, out)
}

/// The console's queue description: an engine or pipe and a queue on it
/// in place of PC's priority and node mask.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct QueueDescX {
    kind: u32,
    flags: u32,
    engine_or_pipe: u32,
    queue: u32,
}

/// Device method `CreateCommandQueueX`. The host picks engines itself, so
/// the queue is created as on PC.
pub(crate) extern "system" fn create_x(
    this: This,
    desc: *const QueueDescX,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&x) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(desc = ?x, "console command queue");
    let desc = QueueDesc {
        kind: x.kind,
        priority: 0,
        flags: x.flags,
        nodes: 1,
    };
    create(this, &desc, iid, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUEUE,
        iid,
        object::is(iid, &QUEUE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Queue>(this)
}

/// Runs the lists' host lists and waits for them (decision 0007). Lists
/// containing an unsatisfied 64-bit memory wait execute their pre-wait work
/// immediately and defer only the post-wait tail onto this queue's pending
/// stream so the submitting CPU thread never deadlocks.
extern "system" fn execute(this: This, count: u32, lists: *const This) {
    if lists.is_null() || count == 0 {
        return;
    }
    // SAFETY: the title passes `count` lists.
    let lists = unsafe { std::slice::from_raw_parts(lists, count as usize) };
    let mut steps = Vec::new();
    for &l in lists {
        steps.extend(crate::list::snapshot(l));
    }
    if steps.is_empty() {
        #[cfg(windows)]
        if crate::host::host().is_none() {
            static ONCE: AtomicBool = AtomicBool::new(false);
            if !ONCE.swap(true, Ordering::Relaxed) {
                tracing::warn!(count, "command lists submitted; no host GPU runs them");
            }
        }
        return;
    }

    let state = &queue(this).state;
    let has_wait = steps.iter().any(|s| matches!(s, ListStep::Wait(_)));

    // Fast path: queue is idle and no list contains a memory wait.
    if !has_wait && !state.busy.load(Ordering::Acquire) {
        #[cfg(windows)]
        {
            let mut batch: Vec<HostSegment> = Vec::new();
            for step in steps {
                match step {
                    ListStep::Host { list, uploads } => batch.push((list, uploads)),
                    ListStep::Cpu(ops) => {
                        run_host_batch(&batch);
                        batch.clear();
                        run_cpu_ops(&ops);
                    }
                    ListStep::Wait(_) => unreachable!(),
                }
            }
            run_host_batch(&batch);
        }
        #[cfg(not(windows))]
        for step in steps {
            if let ListStep::Cpu(ops) = step {
                run_cpu_ops(&ops);
            }
        }
        return;
    }

    // Slow path: queue is already waiting behind an earlier wait, or this
    // submission contains a 64-bit memory wait.
    {
        let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.is_busy() {
            for step in steps {
                inner.items.push_back(step.into());
            }
            state.busy.store(true, Ordering::Release);
            drop(inner);
            register_pending(state.clone());
            return;
        }
        inner.running = true;
        state.busy.store(true, Ordering::Release);
    }

    // Execute ready prefix synchronously so pre-wait work runs right away;
    // stop and enqueue without blocking at the first unsatisfied wait.
    let mut iter = steps.into_iter().peekable();
    let mut deferred_tail: Vec<PendingAction> = Vec::new();
    while let Some(step) = iter.next() {
        match step {
            #[cfg(windows)]
            ListStep::Host { list, uploads } => {
                let mut batch = vec![(list, uploads)];
                while matches!(iter.peek(), Some(ListStep::Host { .. })) {
                    if let Some(ListStep::Host { list, uploads }) = iter.next() {
                        batch.push((list, uploads));
                    }
                }
                run_host_batch(&batch);
            }
            ListStep::Cpu(ops) => run_cpu_ops(&ops),
            ListStep::Wait(w) => {
                if !w.satisfied() {
                    deferred_tail.push(PendingAction::Wait(w));
                    deferred_tail.extend(iter.map(PendingAction::from));
                    break;
                }
            }
        }
    }

    let need_register = {
        let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Prepend our deferred tail ahead of any items that arrived concurrently
        // on this queue while our pre-wait prefix was running.
        for action in deferred_tail.into_iter().rev() {
            inner.items.push_front(action);
        }
        inner.running = false;
        let busy = inner.is_busy();
        state.busy.store(busy, Ordering::Release);
        busy
    };
    if need_register {
        register_pending(state.clone());
    }
}

/// The start of a present plane: frame token, then the plane's resources.
#[repr(C)]
pub(crate) struct Plane {
    token: u64,
    count: u32,
    resources: *const This,
}
/// Submission tagged with the frame it belongs to `(count, lists, frame
/// token, flags)`; frames are paced elsewhere, so it runs as a plain one.
extern "system" fn execute_x(this: This, count: u32, lists: *const This, _token: u64, _flags: u32) {
    execute(this, count, lists);
}
/// Signal with sync settings for the top and bottom of the pipe; our
/// submissions are complete when they return, so it signals at once.
extern "system" fn signal2_x(
    this: This,
    fence: This,
    value: u64,
    _bottom: *const u8,
    _top: *const u8,
) -> HRESULT {
    signal(this, fence, value)
}
/// Markers and events only matter to profilers.
extern "system" fn marker(_: This) {}
extern "system" fn signal(this: This, fence: This, value: u64) -> HRESULT {
    if fence.is_null() {
        return E_INVALIDARG;
    }
    let state = &queue(this).state;
    if state.busy.load(Ordering::Acquire) {
        let mut inner = state.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.is_busy() {
            crate::object::add_ref(fence);
            inner
                .items
                .push_back(PendingAction::Signal(fence as usize, value));
            drop(inner);
            register_pending(state.clone());
            return S_OK;
        }
    }
    crate::fence::signal(fence, value)
}
/// Nothing runs on the queue, so waiting for a fence never blocks it.
extern "system" fn wait(_: This, fence: This, _value: u64) -> HRESULT {
    if fence.is_null() { E_INVALIDARG } else { S_OK }
}
/// Ticks per second of GPU timestamps (see [`Written::GpuTimestamp`]).
extern "system" fn timestamp_frequency(_: This, out: *mut u64) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(TIMESTAMP_FREQUENCY) };
    S_OK
}
const TIMESTAMP_FREQUENCY: u64 = 100_000_000;

/// What a list's bottom-of-pipe write stores when the list has run.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Written {
    Value(u64),
    /// The GPU clock: [`TIMESTAMP_FREQUENCY`] ticks per second since the
    /// process started (our GPU work is done on the CPU's timeline).
    GpuTimestamp,
    /// The CPU's performance counter.
    SystemTimestamp,
}

impl Written {
    /// Stores the low `bytes` (4 or 8) of the value at `at`.
    pub(crate) fn write(self, at: *mut u8, bytes: usize) {
        let value = match self {
            Written::Value(v) => v,
            Written::GpuTimestamp => {
                static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
                (elapsed.as_nanos() / (1_000_000_000 / u128::from(TIMESTAMP_FREQUENCY))) as u64
            }
            Written::SystemTimestamp => system_timestamp(),
        };
        // SAFETY: the title names writable GPU memory (unified with ours).
        unsafe {
            if bytes == 4 {
                if (at as usize).is_multiple_of(4) {
                    AtomicU32::from_ptr(at.cast::<u32>()).store(value as u32, Ordering::Release);
                } else {
                    std::sync::atomic::fence(Ordering::Release);
                    at.cast::<u32>().write_unaligned(value as u32);
                }
            } else if (at as usize).is_multiple_of(8) {
                AtomicU64::from_ptr(at.cast::<u64>()).store(value, Ordering::Release);
            } else {
                std::sync::atomic::fence(Ordering::Release);
                at.cast::<u64>().write_unaligned(value);
            }
        }
        notify_memory_waiters();
    }
}

fn system_timestamp() -> u64 {
    #[cfg(windows)]
    {
        let mut count = 0;
        // SAFETY: a valid out pointer.
        unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut count) };
        count as u64
    }
    #[cfg(not(windows))]
    0
}

/// Frames presented so far.
static FRAMES: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
pub(crate) fn presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

/// Shows the first plane's first resource in our window.
///
/// shortcut: one plane, no scaling or placement, the source expected in
/// the common state.
extern "system" fn present(
    _: This,
    planes: u32,
    plane_parameters: *const Plane,
    _parameters: *const u8,
) -> HRESULT {
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if frame.is_power_of_two() {
        let (draws, dispatches) = crate::list::take_counts();
        #[cfg(windows)]
        let (drawn, dispatched) = {
            static DRAWN: AtomicU64 = AtomicU64::new(0);
            static DISPATCHED: AtomicU64 = AtomicU64::new(0);
            let since = |last: &AtomicU64, now: u64| now - last.swap(now, Ordering::Relaxed);
            (
                since(&DRAWN, crate::draw::drawn()),
                since(&DISPATCHED, crate::draw::dispatched()),
            )
        };
        #[cfg(not(windows))]
        let (drawn, dispatched) = (0, 0);
        tracing::info!(
            frame,
            draws,
            drawn,
            dispatches,
            dispatched,
            "frame presented (since the last line: draws and dispatches, and those run on the host)"
        );
        #[cfg(windows)]
        if frame >= 256 {
            let (hits, misses) = crate::shader_cache::counts();
            tracing::info!(hits, misses, "shaders from the cache and translated so far");
        }
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    if crate::capture::listed(frame) {
        crate::capture::dump_targets(frame);
    }
    #[cfg(windows)]
    if planes > 0
        // SAFETY: the title passes `planes` plane descriptions.
        && let Some(plane) = unsafe { plane_parameters.as_ref() }
        && plane.count > 0
        && !plane.resources.is_null()
        // SAFETY: the plane lists `count` resources.
        && let resource = unsafe { *plane.resources }
        && let Some(texture) = crate::resource::host_resource(resource)
        && let Some(host) = crate::host::host()
    {
        const COPY_SOURCE: u32 = 0x800;
        let state = crate::resource::host_transition(resource, COPY_SOURCE)
            .map_or(COPY_SOURCE, |(_, before)| before);
        let shown = host.present(texture, state, frame);
        crate::resource::host_transition(resource, state);
        #[cfg(feature = "diagnostics")]
        blinks(frame, resource, state, shown);
        #[cfg(not(feature = "diagnostics"))]
        let _ = shown;
    }
    #[cfg(not(windows))]
    let _ = (planes, plane_parameters);
    S_OK
}
/// Diagnostics: logs a presented frame that is black after one that was
/// not, with what was presented and how many draws ran since.
#[cfg(all(windows, feature = "diagnostics"))]
fn blinks(frame: u64, resource: This, state: u32, shown: Option<bool>) {
    static LAST: std::sync::Mutex<(bool, u64, u64)> = std::sync::Mutex::new((false, 0, 0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let drawn = crate::draw::drawn();
    let black = shown.unwrap_or(false);
    let draws = crate::draw::take_frame();
    if black && !last.0 {
        last.2 += 1;
        tracing::warn!(
            frame,
            resource = ?resource,
            state = format_args!("{state:#x}"),
            draws = drawn - last.1,
            copied = shown.is_some(),
            black_frames = last.2,
            levels = ?crate::capture::recent_levels(),
            "a black frame after a frame that was not"
        );
        for d in draws {
            tracing::warn!("black frame {frame}: {d}");
        }
        AFTER.store(frame + 6, Ordering::Relaxed);
    }
    // Six frames on: the levels around it (a fade or a single drop).
    static AFTER: AtomicU64 = AtomicU64::new(0);
    if AFTER.load(Ordering::Relaxed) == frame {
        tracing::warn!(frame, levels = ?crate::capture::recent_levels(), "levels after the black frame");
    }
    *last = (black, drawn, last.2);
}

/// A run of destination pages: how they are mapped (0 pool pages from
/// `start` on, 1 pool page `start` each, 2 unmapped, 3 left as they are)
/// and how many.
#[repr(C)]
pub(crate) struct PageRange {
    kind: u32,
    start: u32,
    count: u32,
}

/// The ranges mapped from a destination address on.
#[repr(C)]
pub(crate) struct PageBatch {
    destination: u64,
    count: u32,
    ranges: *const PageRange,
}

/// Bytes per mapped page.
const PAGE: u64 = 64 << 10;

/// `CopyPageMappingsBatchX(count, batches, pool, pool pages, flags)`: maps
/// pages of a pool (memory the title mapped physical pages into) at the
/// batches' addresses. Grand Theft Auto VI streams textures into pool pages
/// and maps them into its tiled heap this way (title+0xf2c470, ranges of
/// one constant page or of unmapped pages). Done at once: every earlier
/// submission has run.
extern "system" fn copy_page_mappings(
    _: This,
    count: u32,
    batches: *const PageBatch,
    pool: u64,
    pool_pages: u32,
    _flags: u32,
) -> HRESULT {
    let Some(map) = crate::resource::page_mapper() else {
        return E_INVALIDARG;
    };
    if batches.is_null() && count > 0 {
        return E_INVALIDARG;
    }
    let batches = (count > 0).then(|| {
        // SAFETY: the title passes `count` batches.
        unsafe { std::slice::from_raw_parts(batches, count as usize) }
    });
    let mut ok = true;
    for b in batches.into_iter().flatten() {
        if b.ranges.is_null() {
            continue;
        }
        // SAFETY: each batch holds `count` ranges.
        let ranges = unsafe { std::slice::from_raw_parts(b.ranges, b.count as usize) };
        let mut at = b.destination;
        for r in ranges {
            let len = u64::from(r.count) * PAGE;
            let inside = |pages: u32| r.start.checked_add(pages).is_some_and(|e| e <= pool_pages);
            let source = pool + u64::from(r.start) * PAGE;
            ok &= match r.kind {
                0 if inside(r.count) => map(at, source, len),
                1 if inside(1) => (0..u64::from(r.count)).all(|k| map(at + k * PAGE, source, PAGE)),
                2 => map(at, 0, len),
                3 => true,
                _ => false,
            };
            at += len;
        }
    }
    if !ok {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(pool = format_args!("{pool:#x}"), "a page mapping failed")
        });
    }
    S_OK
}

extern "system" fn desc(this: This, out: *mut QueueDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(queue(this).desc) };
    }
}

/// The queue methods by their slot in older titles' queues (the 2020-era
/// layout): IUnknown 3, object 4, device child 1, queue 26 (12 as on PC,
/// then console ones).
const METHODS: [(usize, *const ()); 15] = [
    (0, query as *const ()),
    (2, release as *const ()),
    (7, object::parent_device as *const ()), // 0x38
    (10, execute as *const ()),              // 0x50
    (11, marker as *const ()),               // 0x58 set marker
    (12, marker as *const ()),               // 0x60 begin event
    (13, marker as *const ()),               // 0x68 end event
    (14, signal as *const ()),               // 0x70
    (15, wait as *const ()),                 // 0x78
    (16, timestamp_frequency as *const ()),  // 0x80
    (26, desc as *const ()),                 // 0xd0
    (29, copy_page_mappings as *const ()),   // 0xe8 page mapping batch
    (30, present as *const ()),              // 0xf0
    (33, execute_x as *const ()),            // 0x108
    (32, signal2_x as *const ()),            // 0x100
];

/// Newer titles' queues have two more methods before Signal, which moved
/// from 0x70 to 0x80: every slot from 0x70 on is two later (taken here as
/// starting exactly at 0x70, the methods before it kept in place).
/// Inferred from Grand Theft Auto VI's calls: Signal 0x80 `(fence,
/// value)`, timestamp frequency 0x90 (title+0x24841a6), suspend and resume
/// 0xb8/0xc0, PresentX 0x100, the frame-token submission 0x118 `(count,
/// lists, token, flags)` (title+0x24821dd).
const fn newer_slot(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

const fn queue_table(newer: bool) -> [*const (); 36] {
    let mut f = object::d3d12_object(object::missing_table::<QUEUE, 36>());
    let mut i = 0;
    while i < METHODS.len() {
        let (slot, method) = METHODS[i];
        f[if newer { newer_slot(slot) } else { slot }] = method;
        i += 1;
    }
    f
}

static FUNCTIONS: Table<36> = Table(queue_table(false));
static NEWER_FUNCTIONS: Table<36> = Table(queue_table(true));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    /// Where a queue's signal is: newer layouts have it two slots later.
    /// Another test may switch layouts while this one runs, so the queue's
    /// own table decides.
    fn signal_slot(q: This) -> usize {
        // SAFETY: one of our queues; its first field is the table.
        let table = unsafe { *q.cast::<*const *const ()>() };
        if table == NEWER_FUNCTIONS.0.as_ptr() {
            0x80
        } else {
            0x70
        }
    }

    #[test]
    fn queue_signals_complete_at_once() {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        // SAFETY: the title reads the type at +0x10.
        assert_eq!(unsafe { q.cast::<u8>().add(0x10).cast::<u32>().read() }, 0);
        let mut fence = null;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(
            crate::fence::create(null, 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let signal = slot::<extern "system" fn(This, This, u64) -> HRESULT>(q, signal_slot(q));
        assert_eq!(signal(q, fence, 7), S_OK);
        assert_eq!(
            slot::<extern "system" fn(This) -> u64>(fence, 0x40)(fence),
            7
        );
        assert_eq!(slot::<extern "system" fn(This) -> u32>(q, 0x10)(q), 0);
    }

    /// Grand Theft Auto VI's compute queue: `{COMPUTE, 0, pipe 1, queue 0}`.
    #[test]
    fn console_queues_are_created_as_on_pc() {
        let null = std::ptr::null();
        let desc = QueueDescX {
            kind: 2,
            flags: 0,
            engine_or_pipe: 1,
            queue: 0,
        };
        let mut q = null;
        assert_eq!(create_x(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        assert_eq!(queue(q).desc.kind, 2);
        let none = std::ptr::null();
        assert_eq!(create_x(null, none, &QUEUE_IIDS[0], &mut q), E_INVALIDARG);
    }

    #[test]
    fn newer_queues_have_their_methods_two_slots_later() {
        let (old, new) = (queue_table(false), queue_table(true));
        assert_eq!(old[10], new[10]); // submission stays at 0x50
        assert_eq!(old[14], new[16]); // signal 0x70 → 0x80
        assert_eq!(old[30], new[32]); // present 0xf0 → 0x100
        assert_eq!(new[35], execute_x as *const ()); // 0x118
    }

    #[test]
    fn timestamps_and_values_are_written() {
        let mut at = [0u64; 2];
        Written::Value(0x1_0000_0002).write(at.as_mut_ptr().cast(), 4);
        assert_eq!(at[0], 2);
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        let first = at[1];
        std::thread::sleep(std::time::Duration::from_millis(2));
        Written::GpuTimestamp.write(at[1..].as_mut_ptr().cast(), 8);
        assert!(at[1] > first);
    }

    #[test]
    fn memory_wait_comparison_ops_cover_full_64_bit_words() {
        let hi = 0x0000_0001_0000_0000u64;
        let lo = 0x0000_0000_ffff_ffffu64;
        assert!(!MemoryWait::compare(hi, 1, lo)); // NEVER
        assert!(MemoryWait::compare(lo, 2, hi)); // LESS
        assert!(!MemoryWait::compare(hi, 2, lo));
        assert!(MemoryWait::compare(hi, 3, hi)); // EQUAL
        assert!(!MemoryWait::compare(hi, 3, 0));
        assert!(MemoryWait::compare(lo, 4, hi)); // LESS_EQUAL
        assert!(MemoryWait::compare(hi, 4, hi));
        assert!(MemoryWait::compare(hi, 5, lo)); // GREATER
        assert!(!MemoryWait::compare(hi, 5, hi));
        assert!(MemoryWait::compare(hi, 6, lo)); // NOT_EQUAL
        assert!(!MemoryWait::compare(hi, 6, hi));
        assert!(MemoryWait::compare(hi, 7, lo)); // GREATER_EQUAL
        assert!(MemoryWait::compare(hi, 7, hi));
        assert!(!MemoryWait::compare(lo, 7, hi));
        assert!(MemoryWait::compare(0, 8, u64::MAX)); // ALWAYS
    }

    fn make_queue_and_list() -> (This, This, This) {
        let null = std::ptr::null();
        let desc = QueueDesc {
            kind: 0,
            priority: 0,
            flags: 0,
            nodes: 0,
        };
        let mut q = null;
        assert_eq!(create(null, &desc, &QUEUE_IIDS[0], &mut q), S_OK);
        let mut alloc = null;
        let alloc_iid = guid(
            0x6102dee4,
            0xaf59,
            0x4b09,
            [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
        );
        assert_eq!(
            crate::list::create_allocator(null, 0, &alloc_iid, &mut alloc),
            S_OK
        );
        let mut list = null;
        let list_iid = guid(
            0x5b160d0f,
            0xac1b,
            0x4185,
            [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
        );
        assert_eq!(
            crate::list::create_list(null, 0, 0, alloc, null, &list_iid, &mut list),
            S_OK
        );
        (q, alloc, list)
    }

    fn list_slot_offset(list: This, old_offset: usize) -> usize {
        if crate::list::is_newer_table(list) && old_offset >= 59 * 8 {
            old_offset + 16
        } else {
            old_offset
        }
    }

    fn wait_fence_value(fence: This, want: u64) {
        let completed = slot::<extern "system" fn(This) -> u64>(fence, 0x40);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while completed(fence) < want {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for fence value {want} (got {})",
                completed(fence)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn command_list_memory_wait_orders_producer_and_consumer_both_ways() {
        for consumer_first in [false, true] {
            let (prod_q, prod_alloc, prod_list) = make_queue_and_list();
            let (cons_q, cons_alloc, cons_list) = make_queue_and_list();
            let mut fence = std::ptr::null();
            assert_eq!(
                crate::fence::create(
                    std::ptr::null(),
                    0,
                    0,
                    &crate::fence::FENCE_IIDS[0],
                    &mut fence
                ),
                S_OK
            );

            let payload = Box::new(AtomicU64::new(0));
            let counter = Box::new(AtomicU64::new(0));
            let result = Box::new(AtomicU64::new(0));

            let write64 = slot::<extern "system" fn(This, u64, u64, u32, u32)>(
                prod_list,
                list_slot_offset(prod_list, 0x230),
            );
            let close_p = slot::<extern "system" fn(This) -> HRESULT>(prod_list, 0x48);
            write64(prod_list, (&raw const *payload) as u64, 0x5a55a11c, 0, 0);
            write64(prod_list, (&raw const *counter) as u64, 2, 0, 0);
            assert_eq!(close_p(prod_list), S_OK);

            let wait64 = slot::<extern "system" fn(This, u64, u32, u64, u32)>(
                cons_list,
                list_slot_offset(cons_list, 0x240),
            );
            let copy_mem = slot::<extern "system" fn(This, u64, u64, u64, u32)>(
                cons_list,
                list_slot_offset(cons_list, 0x1e0),
            );
            let close_c = slot::<extern "system" fn(This) -> HRESULT>(cons_list, 0x48);
            wait64(cons_list, (&raw const *counter) as u64, 7, 1, 0);
            copy_mem(
                cons_list,
                (&raw const *result) as u64,
                (&raw const *payload) as u64,
                8,
                0,
            );
            assert_eq!(close_c(cons_list), S_OK);

            let exec_p = slot::<extern "system" fn(This, u32, *const This)>(prod_q, 0x50);
            let exec_c = slot::<extern "system" fn(This, u32, *const This)>(cons_q, 0x50);
            let sig_c =
                slot::<extern "system" fn(This, This, u64) -> HRESULT>(cons_q, signal_slot(cons_q));
            let completed = slot::<extern "system" fn(This) -> u64>(fence, 0x40);

            if consumer_first {
                exec_c(cons_q, 1, &cons_list);
                assert_eq!(sig_c(cons_q, fence, 1), S_OK);
                assert_eq!(completed(fence), 0);
                assert_eq!(result.load(Ordering::Acquire), 0);
                exec_p(prod_q, 1, &prod_list);
                wait_fence_value(fence, 1);
            } else {
                exec_p(prod_q, 1, &prod_list);
                exec_c(cons_q, 1, &cons_list);
                assert_eq!(sig_c(cons_q, fence, 1), S_OK);
                assert_eq!(completed(fence), 1);
            }

            assert_eq!(counter.load(Ordering::Acquire), 2);
            assert_eq!(payload.load(Ordering::Acquire), 0x5a55a11c);
            assert_eq!(
                result.load(Ordering::Acquire),
                0x5a55a11c,
                "consumer_first={consumer_first}"
            );

            for o in [
                prod_list, prod_alloc, prod_q, cons_list, cons_alloc, cons_q, fence,
            ] {
                slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
            }
        }
    }

    #[test]
    fn mid_list_memory_wait_preserves_pre_wait_work_and_releases_post_wait_tail() {
        let (prod_q, prod_alloc, prod_list) = make_queue_and_list();
        let (cons_q, cons_alloc, cons_list) = make_queue_and_list();
        let mut fence = std::ptr::null();
        assert_eq!(
            crate::fence::create(
                std::ptr::null(),
                0,
                0,
                &crate::fence::FENCE_IIDS[0],
                &mut fence
            ),
            S_OK
        );

        let pre_src = Box::new(AtomicU64::new(0xfeed_face_cafe_babe));
        let pre_dst = Box::new(AtomicU64::new(0));
        let pre_marker = Box::new(AtomicU64::new(0));
        let gate = Box::new(AtomicU64::new(0));
        let post_src = Box::new(AtomicU64::new(0));
        let post_dst = Box::new(AtomicU64::new(0));
        let post_marker = Box::new(AtomicU64::new(0));

        let write64_c = slot::<extern "system" fn(This, u64, u64, u32, u32)>(
            cons_list,
            list_slot_offset(cons_list, 0x230),
        );
        let wait64_c = slot::<extern "system" fn(This, u64, u32, u64, u32)>(
            cons_list,
            list_slot_offset(cons_list, 0x240),
        );
        let copy_c = slot::<extern "system" fn(This, u64, u64, u64, u32)>(
            cons_list,
            list_slot_offset(cons_list, 0x1e0),
        );
        let close_c = slot::<extern "system" fn(This) -> HRESULT>(cons_list, 0x48);

        // Pre-wait work.
        copy_c(
            cons_list,
            (&raw const *pre_dst) as u64,
            (&raw const *pre_src) as u64,
            8,
            0,
        );
        write64_c(
            cons_list,
            (&raw const *pre_marker) as u64,
            0x1111_2222_3333_4444,
            0,
            0,
        );
        // Full 64-bit threshold: low 32 bits alone must not satisfy it.
        wait64_c(
            cons_list,
            (&raw const *gate) as u64,
            7,
            0x0000_0001_0000_0001,
            1,
        );
        // Post-wait work.
        copy_c(
            cons_list,
            (&raw const *post_dst) as u64,
            (&raw const *post_src) as u64,
            8,
            0,
        );
        write64_c(
            cons_list,
            (&raw const *post_marker) as u64,
            0xaaaa_bbbb_cccc_dddd,
            0,
            0,
        );
        assert_eq!(close_c(cons_list), S_OK);

        let write64_p = slot::<extern "system" fn(This, u64, u64, u32, u32)>(
            prod_list,
            list_slot_offset(prod_list, 0x230),
        );
        let close_p = slot::<extern "system" fn(This) -> HRESULT>(prod_list, 0x48);
        write64_p(
            prod_list,
            (&raw const *post_src) as u64,
            0x5a55_a11c_dead_beef,
            0,
            0,
        );
        write64_p(
            prod_list,
            (&raw const *gate) as u64,
            0x0000_0001_0000_0001,
            0,
            0,
        );
        assert_eq!(close_p(prod_list), S_OK);

        let exec_c = slot::<extern "system" fn(This, u32, *const This)>(cons_q, 0x50);
        let exec_p = slot::<extern "system" fn(This, u32, *const This)>(prod_q, 0x50);
        let sig_c =
            slot::<extern "system" fn(This, This, u64) -> HRESULT>(cons_q, signal_slot(cons_q));
        let completed = slot::<extern "system" fn(This) -> u64>(fence, 0x40);

        // Submit consumer first: pre-wait work completes immediately; post-wait stays pending.
        exec_c(cons_q, 1, &cons_list);
        assert_eq!(sig_c(cons_q, fence, 1), S_OK);
        assert_eq!(pre_dst.load(Ordering::Acquire), 0xfeed_face_cafe_babe);
        assert_eq!(pre_marker.load(Ordering::Acquire), 0x1111_2222_3333_4444);
        assert_eq!(post_dst.load(Ordering::Acquire), 0);
        assert_eq!(post_marker.load(Ordering::Acquire), 0);
        assert_eq!(completed(fence), 0);

        // Low-32-bit write only must not release a 64-bit wait.
        gate.store(1, Ordering::Release);
        notify_memory_waiters();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(post_dst.load(Ordering::Acquire), 0);
        assert_eq!(completed(fence), 0);

        // Producer writes full 64-bit payload and gate, releasing post-wait work and fence.
        exec_p(prod_q, 1, &prod_list);
        wait_fence_value(fence, 1);
        assert_eq!(post_dst.load(Ordering::Acquire), 0x5a55_a11c_dead_beef);
        assert_eq!(post_marker.load(Ordering::Acquire), 0xaaaa_bbbb_cccc_dddd);

        for o in [
            prod_list, prod_alloc, prod_q, cons_list, cons_alloc, cons_q, fence,
        ] {
            slot::<extern "system" fn(This) -> u32>(o, 0x10)(o);
        }
    }
}
