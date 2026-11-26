//! Task queues and async calls.
//!
//! Public reference: reference/system/xtaskqueue, reference/system/xasync,
//! reference/system/xasyncprovider (XAsyncBlock, XAsyncOp, the dispatch
//! modes and ports, XTaskQueueDispatch, XTaskQueueTerminate, XAsyncCancel).
//! Microsoft's open-source implementation of the same API is libHttpClient's
//! `Source/Task` (MIT), a second oracle where the reference is brief.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::rt::{Guid, Hresult, Object, S_OK, area};
use crate::tap::Tap;

/// The task-queue area (VIctory's `xasync::PROVIDER`, also the interface id).
const AREA: Guid = Guid(
    0x073b_7dcb,
    0x1fcf,
    0x4030,
    [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28],
);

const E_PENDING: Hresult = 0x8000_000A_u32 as i32;
const E_ABORT: Hresult = 0x8000_4004_u32 as i32;

// XTaskQueueDispatchMode and XTaskQueuePort.
const MANUAL: u32 = 0;
const THREAD_POOL: u32 = 1;
const SERIALIZED_THREAD_POOL: u32 = 2;
const IMMEDIATE: u32 = 3;
/// E_NO_TASK_QUEUE (reference/errorcodes).
const E_NO_TASK_QUEUE: Hresult = 0x8007_01AB_u32 as i32;
const E_INVALIDARG: Hresult = 0x8007_0057_u32 as i32;
const WORK: u32 = 0;
const COMPLETION: u32 = 1;

// XAsyncOp.
const BEGIN: u32 = 0;
const DO_WORK: u32 = 1;
const GET_RESULT: u32 = 2;
const CANCEL: u32 = 3;
const CLEANUP: u32 = 4;

type Queue = *mut c_void;
type Callback = extern "system" fn(*mut c_void, bool);
type Done = extern "system" fn(*mut Block);

/// XAsyncBlock.
#[repr(C)]
pub struct Block {
    queue: Queue,
    context: *mut c_void,
    callback: Option<Done>,
    internal: [usize; 4],
}

impl Block {
    /// A block on the process task queue, with no completion callback.
    pub fn on_process_queue() -> Block {
        Block::new(std::ptr::null_mut(), std::ptr::null_mut(), None)
    }

    fn new(queue: Queue, context: *mut c_void, callback: Option<Done>) -> Block {
        Block {
            queue,
            context,
            callback,
            internal: [0; 4],
        }
    }
}

/// XAsyncProviderData.
#[repr(C)]
struct ProviderData {
    block: *mut Block,
    buffer_size: usize,
    buffer: *mut c_void,
    context: *mut c_void,
}

type Provider = extern "system" fn(u32, *const ProviderData) -> Hresult;

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn Sleep(ms: u32);
}

/// The area's methods, by their slots in VIctory's table.
struct Api(Object);

impl Api {
    fn port(&self, q: Queue, port: u32) -> *mut c_void {
        let mut out = std::ptr::null_mut();
        // SAFETY: (these methods) the slot's signature in VIctory's table.
        let f: extern "system" fn(*mut c_void, Queue, u32, *mut *mut c_void) -> Hresult =
            unsafe { self.0.method(0x70) };
        f(self.0.this(), q, port, &mut out);
        out
    }
    fn composite(&self, work: *mut c_void, completion: *mut c_void) -> Result<Queue, Hresult> {
        let mut q = std::ptr::null_mut();
        let f: extern "system" fn(*mut c_void, *mut c_void, *mut c_void, *mut Queue) -> Hresult =
            unsafe { self.0.method(0x68) };
        match f(self.0.this(), work, completion, &mut q) {
            S_OK => Ok(q),
            hr => Err(hr),
        }
    }
    fn set_process_queue(&self, q: Queue) {
        let f: extern "system" fn(*mut c_void, Queue) = unsafe { self.0.method(0xd0) };
        f(self.0.this(), q)
    }
    fn create(&self, work: u32, completion: u32) -> Result<Queue, Hresult> {
        let mut q = std::ptr::null_mut();
        // SAFETY: (all methods) the slot's signature in VIctory's table.
        let f: extern "system" fn(*mut c_void, u32, u32, *mut Queue) -> Hresult =
            unsafe { self.0.method(0x60) };
        match f(self.0.this(), work, completion, &mut q) {
            S_OK => Ok(q),
            hr => Err(hr),
        }
    }
    fn dispatch(&self, q: Queue, port: u32, timeout: u32) -> bool {
        let f: extern "system" fn(*mut c_void, Queue, u32, u32) -> bool =
            unsafe { self.0.method(0x80) };
        f(self.0.this(), q, port, timeout)
    }
    fn close(&self, q: Queue) {
        let f: extern "system" fn(*mut c_void, Queue) = unsafe { self.0.method(0x88) };
        f(self.0.this(), q)
    }
    fn submit(&self, q: Queue, port: u32, context: *mut c_void, cb: Callback) -> Hresult {
        let f: extern "system" fn(
            *mut c_void,
            Queue,
            u32,
            *mut c_void,
            Option<Callback>,
        ) -> Hresult = unsafe { self.0.method(0x90) };
        f(self.0.this(), q, port, context, Some(cb))
    }
    fn submit_delayed(
        &self,
        q: Queue,
        port: u32,
        delay: u32,
        context: *mut c_void,
        cb: Callback,
    ) -> Hresult {
        let f: extern "system" fn(
            *mut c_void,
            Queue,
            u32,
            u32,
            *mut c_void,
            Option<Callback>,
        ) -> Hresult = unsafe { self.0.method(0x98) };
        f(self.0.this(), q, port, delay, context, Some(cb))
    }
    fn terminate(
        &self,
        q: Queue,
        wait: bool,
        context: *mut c_void,
        cb: extern "system" fn(*mut c_void),
    ) -> Hresult {
        let f: extern "system" fn(
            *mut c_void,
            Queue,
            u8,
            *mut c_void,
            Option<extern "system" fn(*mut c_void)>,
        ) -> Hresult = unsafe { self.0.method(0xb0) };
        f(self.0.this(), q, u8::from(wait), context, Some(cb))
    }
    fn process_queue(&self) -> (bool, Queue) {
        let mut q = std::ptr::null_mut();
        let f: extern "system" fn(*mut c_void, *mut Queue) -> bool = unsafe { self.0.method(0xc8) };
        (f(self.0.this(), &mut q), q)
    }
    fn status(&self, b: *mut Block, wait: bool) -> Hresult {
        let f: extern "system" fn(*mut c_void, *mut Block, u8) -> Hresult =
            unsafe { self.0.method(0x18) };
        f(self.0.this(), b, u8::from(wait))
    }
    fn result_size(&self, b: *mut Block) -> (Hresult, usize) {
        let mut n = 0;
        let f: extern "system" fn(*mut c_void, *mut Block, *mut usize) -> Hresult =
            unsafe { self.0.method(0x20) };
        (f(self.0.this(), b, &mut n), n)
    }
    fn cancel(&self, b: *mut Block) {
        let f: extern "system" fn(*mut c_void, *mut Block) = unsafe { self.0.method(0x28) };
        f(self.0.this(), b)
    }
    fn run(&self, b: *mut Block, work: extern "system" fn(*mut Block) -> Hresult) -> Hresult {
        let f: extern "system" fn(
            *mut c_void,
            *mut Block,
            Option<extern "system" fn(*mut Block) -> Hresult>,
        ) -> Hresult = unsafe { self.0.method(0x30) };
        f(self.0.this(), b, Some(work))
    }
    fn begin(
        &self,
        b: *mut Block,
        context: *mut c_void,
        identity: *const c_void,
        provider: Provider,
    ) -> Hresult {
        let f: extern "system" fn(
            *mut c_void,
            *mut Block,
            *mut c_void,
            *const c_void,
            *const u8,
            Option<Provider>,
        ) -> Hresult = unsafe { self.0.method(0x38) };
        f(
            self.0.this(),
            b,
            context,
            identity,
            c"conformance".as_ptr().cast(),
            Some(provider),
        )
    }
    fn schedule(&self, b: *mut Block, delay: u32) -> Hresult {
        let f: extern "system" fn(*mut c_void, *mut Block, u32) -> Hresult =
            unsafe { self.0.method(0x48) };
        f(self.0.this(), b, delay)
    }
    fn complete(&self, b: *mut Block, hr: Hresult, size: usize) {
        let f: extern "system" fn(*mut c_void, *mut Block, Hresult, usize) =
            unsafe { self.0.method(0x50) };
        f(self.0.this(), b, hr, size)
    }
    fn result(
        &self,
        b: *mut Block,
        identity: *const c_void,
        buffer: &mut [u8],
    ) -> (Hresult, usize) {
        let mut used = 0;
        let f: extern "system" fn(
            *mut c_void,
            *mut Block,
            *const c_void,
            usize,
            *mut c_void,
            *mut usize,
        ) -> Hresult = unsafe { self.0.method(0x58) };
        (
            f(
                self.0.this(),
                b,
                identity,
                buffer.len(),
                buffer.as_mut_ptr().cast(),
                &mut used,
            ),
            used,
        )
    }
}

// SAFETY: the object is the runtime's, callable from any thread.
unsafe impl Sync for Api {}
// SAFETY: as above.
unsafe impl Send for Api {}

static API: OnceLock<Api> = OnceLock::new();

/// Waits for an async call; its status (XAsyncGetStatus with wait set).
pub fn wait(b: &mut Block) -> Hresult {
    api().status(b, true)
}

fn api() -> &'static Api {
    API.get().expect("the task-queue area")
}

/// What a submitted callback saw: its thread and `canceled`.
#[derive(Default)]
struct Seen {
    calls: AtomicU32,
    thread: AtomicU32,
    canceled: AtomicBool,
}

extern "system" fn note(context: *mut c_void, canceled: bool) {
    // SAFETY: tests pass a live `Seen`.
    let seen = unsafe { &*context.cast::<Seen>() };
    // SAFETY: a plain call.
    seen.thread
        .store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    seen.canceled.store(canceled, Ordering::SeqCst);
    seen.calls.fetch_add(1, Ordering::SeqCst);
}

extern "system" fn terminated(context: *mut c_void) {
    // SAFETY: tests pass a live `AtomicU32`.
    unsafe { &*context.cast::<AtomicU32>() }.fetch_add(1, Ordering::SeqCst);
}

fn ctx<T>(value: &T) -> *mut c_void {
    std::ptr::from_ref(value).cast_mut().cast()
}

/// Waits up to `ms` for `done`.
fn wait_for(ms: u32, done: impl Fn() -> bool) -> bool {
    for _ in 0..ms / 5 {
        if done() {
            return true;
        }
        // SAFETY: a plain call.
        unsafe { Sleep(5) };
    }
    done()
}

pub fn run(t: &mut Tap) {
    match area(AREA, AREA) {
        Ok(object) => {
            let _ = API.set(Api(object));
        }
        Err(hr) => {
            t.check(false, "xasync: the task-queue area is served", || {
                format!("QueryApiImpl {hr:#010x}")
            });
            return;
        }
    }
    let a = api();
    // SAFETY: a plain call.
    let me = unsafe { GetCurrentThreadId() };

    // Manual dispatch: nothing runs until XTaskQueueDispatch, which runs one
    // callback on the calling thread and returns true; false when empty.
    let q = a.create(MANUAL, MANUAL).expect("manual queue");
    let seen = Seen::default();
    t.eq(
        a.submit(q, WORK, ctx(&seen), note),
        S_OK,
        "xasync: XTaskQueueSubmitCallback succeeds",
    );
    t.eq(
        seen.calls.load(Ordering::SeqCst),
        0,
        "xasync: a manual port runs nothing before dispatch",
    );
    t.eq(
        a.dispatch(q, WORK, 0),
        true,
        "xasync: XTaskQueueDispatch returns true when it ran a callback",
    );
    t.eq(
        seen.calls.load(Ordering::SeqCst),
        1,
        "xasync: the dispatched callback ran once",
    );
    t.eq(
        seen.thread.load(Ordering::SeqCst),
        me,
        "xasync: a manual dispatch runs on the calling thread",
    );
    t.eq(
        seen.canceled.load(Ordering::SeqCst),
        false,
        "xasync: a dispatched callback is not canceled",
    );
    t.eq(
        a.dispatch(q, WORK, 0),
        false,
        "xasync: XTaskQueueDispatch returns false on an empty port",
    );
    t.eq(
        a.dispatch(q, WORK, 20),
        false,
        "xasync: a dispatch with a timeout returns false when nothing comes",
    );

    // A delayed callback is not there before its delay, and is after.
    let delayed = Seen::default();
    t.eq(
        a.submit_delayed(q, WORK, 100, ctx(&delayed), note),
        S_OK,
        "xasync: XTaskQueueSubmitDelayedCallback succeeds",
    );
    t.eq(
        a.dispatch(q, WORK, 0),
        false,
        "xasync: a delayed callback is not dispatched before its delay",
    );
    t.eq(
        a.dispatch(q, WORK, 2000),
        true,
        "xasync: a delayed callback is dispatched after its delay",
    );
    t.eq(
        delayed.calls.load(Ordering::SeqCst),
        1,
        "xasync: the delayed callback ran once",
    );

    // Terminate: pending callbacks are called with canceled set, new ones
    // are refused, and the termination callback comes last.
    let pending = Seen::default();
    let ends = AtomicU32::new(0);
    a.submit(q, WORK, ctx(&pending), note);
    t.eq(
        a.terminate(q, false, ctx(&ends), terminated),
        S_OK,
        "xasync: XTaskQueueTerminate succeeds",
    );
    let late = Seen::default();
    t.check(
        a.submit(q, WORK, ctx(&late), note) != S_OK,
        "xasync: a terminated queue refuses new callbacks",
        || "accepted".into(),
    );
    while a.dispatch(q, WORK, 0) || a.dispatch(q, COMPLETION, 0) {}
    t.eq(
        pending.calls.load(Ordering::SeqCst),
        1,
        "xasync: a pending callback is still called on terminate",
    );
    t.eq(
        pending.canceled.load(Ordering::SeqCst),
        true,
        "xasync: ... with canceled set",
    );
    t.eq(
        late.calls.load(Ordering::SeqCst),
        0,
        "xasync: a refused callback is never called",
    );
    t.check(
        wait_for(2000, || ends.load(Ordering::SeqCst) == 1),
        "xasync: the termination callback is called once",
        || format!("called {} times", ends.load(Ordering::SeqCst)),
    );
    a.close(q);

    // Immediate runs on the submitting thread, during the submit; the
    // thread pool runs elsewhere.
    let q = a.create(IMMEDIATE, IMMEDIATE).expect("immediate queue");
    let seen = Seen::default();
    a.submit(q, WORK, ctx(&seen), note);
    t.eq(
        seen.calls.load(Ordering::SeqCst),
        1,
        "xasync: an immediate port runs the callback during the submit",
    );
    t.eq(
        seen.thread.load(Ordering::SeqCst),
        me,
        "xasync: ... on the submitting thread",
    );
    a.close(q);
    let q = a
        .create(THREAD_POOL, THREAD_POOL)
        .expect("thread-pool queue");
    let seen = Seen::default();
    a.submit(q, WORK, ctx(&seen), note);
    t.check(
        wait_for(2000, || seen.calls.load(Ordering::SeqCst) == 1),
        "xasync: a thread-pool port runs the callback",
        || "not run".into(),
    );
    t.check(
        seen.thread.load(Ordering::SeqCst) != me,
        "xasync: ... on another thread",
        || "ran on the submitting thread".into(),
    );
    a.close(q);

    // The process has a default queue (a thread pool).
    let (has, q) = a.process_queue();
    t.check(
        has && !q.is_null(),
        "xasync: there is a process task queue",
        || format!("{has} {q:?}"),
    );
    if has && !q.is_null() {
        a.close(q);
    }

    async_calls(t);
    edges(t);
}

static OPS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// A provider that schedules its work, completes with 8 bytes, and hands
/// them over on GetResult.
extern "system" fn eight_bytes(op: u32, data: *const ProviderData) -> Hresult {
    OPS.lock().unwrap().push(op);
    // SAFETY: the runtime passes valid provider data.
    let data = unsafe { &*data };
    let a = api();
    match op {
        BEGIN => a.schedule(data.block, 0),
        DO_WORK => {
            a.complete(data.block, S_OK, 8);
            S_OK
        }
        GET_RESULT => {
            // SAFETY: the runtime passes the caller's buffer of `buffer_size`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    0x1122_3344_5566_7788_u64.to_le_bytes().as_ptr(),
                    data.buffer.cast(),
                    8,
                )
            };
            S_OK
        }
        _ => S_OK,
    }
}

/// A provider that never finishes on its own; canceled, it completes with
/// E_ABORT.
extern "system" fn forever(op: u32, data: *const ProviderData) -> Hresult {
    OPS.lock().unwrap().push(op);
    // SAFETY: the runtime passes valid provider data.
    let data = unsafe { &*data };
    if op == CANCEL {
        api().complete(data.block, E_ABORT, 0);
    }
    S_OK
}

extern "system" fn work_ok(_: *mut Block) -> Hresult {
    S_OK
}

extern "system" fn done(b: *mut Block) {
    // SAFETY: tests put a live `AtomicU32` in the block's context.
    unsafe { &*(*b).context.cast::<AtomicU32>() }.fetch_add(1, Ordering::SeqCst);
}

fn async_calls(t: &mut Tap) {
    let a = api();
    let q = a.create(MANUAL, MANUAL).expect("manual queue");
    let dispatch_all = || while a.dispatch(q, WORK, 0) || a.dispatch(q, COMPLETION, 0) {};

    // XAsyncRun: E_PENDING until its work has run, then the work's result;
    // the completion callback runs on the completion port.
    let calls = AtomicU32::new(0);
    let mut block = Block::new(q, ctx(&calls), Some(done));
    let b: *mut Block = &mut block;
    t.eq(a.run(b, work_ok), S_OK, "xasync: XAsyncRun succeeds");
    t.eq(
        a.status(b, false),
        E_PENDING,
        "xasync: XAsyncGetStatus is E_PENDING before the work runs",
    );
    dispatch_all();
    t.eq(
        a.status(b, false),
        S_OK,
        "xasync: XAsyncGetStatus is the work's result once it ran",
    );
    t.eq(
        calls.load(Ordering::SeqCst),
        1,
        "xasync: the completion callback ran once",
    );

    // A provider: Begin, DoWork, then GetResult and Cleanup on XAsyncGetResult.
    OPS.lock().unwrap().clear();
    let calls = AtomicU32::new(0);
    let mut block = Block::new(q, ctx(&calls), Some(done));
    let b: *mut Block = &mut block;
    let identity = eight_bytes as *const c_void;
    t.eq(
        a.begin(b, std::ptr::null_mut(), identity, eight_bytes),
        S_OK,
        "xasync: XAsyncBegin succeeds",
    );
    dispatch_all();
    t.eq(
        a.status(b, false),
        S_OK,
        "xasync: a provider's call succeeds once it completes",
    );
    t.eq(
        a.result_size(b),
        (S_OK, 8),
        "xasync: XAsyncGetResultSize is the size the provider completed with",
    );
    let mut buffer = [0u8; 8];
    t.eq(
        a.result(b, identity, &mut buffer),
        (S_OK, 8),
        "xasync: XAsyncGetResult copies the result",
    );
    t.eq(
        u64::from_le_bytes(buffer),
        0x1122_3344_5566_7788,
        "xasync: ... the provider's bytes",
    );
    t.eq(
        OPS.lock().unwrap().clone(),
        vec![BEGIN, DO_WORK, GET_RESULT, CLEANUP],
        "xasync: provider operations come in order",
    );
    t.eq(
        calls.load(Ordering::SeqCst),
        1,
        "xasync: the provider call's completion callback ran once",
    );

    // Cancel: the provider is asked to cancel; the call ends with E_ABORT and
    // its completion callback still runs.
    OPS.lock().unwrap().clear();
    let calls = AtomicU32::new(0);
    let mut block = Block::new(q, ctx(&calls), Some(done));
    let b: *mut Block = &mut block;
    a.begin(b, std::ptr::null_mut(), forever as *const c_void, forever);
    t.eq(
        a.status(b, false),
        E_PENDING,
        "xasync: an unfinished call is E_PENDING",
    );
    a.cancel(b);
    dispatch_all();
    t.eq(
        a.status(b, false),
        E_ABORT,
        "xasync: a canceled call's status is E_ABORT",
    );
    t.eq(
        calls.load(Ordering::SeqCst),
        1,
        "xasync: a canceled call's completion callback still runs",
    );
    let ops = OPS.lock().unwrap().clone();
    t.check(
        ops.first() == Some(&BEGIN) && ops.contains(&CANCEL) && ops.last() == Some(&CLEANUP),
        "xasync: a canceled provider sees Begin, Cancel, then Cleanup",
        || format!("ops {ops:?}"),
    );
    a.close(q);
}

/// Counts callbacks running at once (and the most seen).
#[derive(Default)]
struct Overlap {
    running: AtomicU32,
    most: AtomicU32,
    done: AtomicU32,
}

extern "system" fn slow(context: *mut c_void, _: bool) {
    // SAFETY: tests pass a live `Overlap`.
    let o = unsafe { &*context.cast::<Overlap>() };
    let now = o.running.fetch_add(1, Ordering::SeqCst) + 1;
    o.most.fetch_max(now, Ordering::SeqCst);
    // SAFETY: a plain call.
    unsafe { Sleep(2) };
    o.running.fetch_sub(1, Ordering::SeqCst);
    o.done.fetch_add(1, Ordering::SeqCst);
}

/// reference/system/xtaskqueue: SerializedThreadPool "executes the calls in
/// serial, one at a time, on the thread pool"; XTaskQueueCreateComposite
/// "Creates a task queue composed of ports of other task queues"; with the
/// process task queue set to nullptr, "XTaskQueueGetCurrentProcessTaskQueue
/// will also return nullptr" and APIs needing a queue "error with
/// E_NO_TASK_QUEUE if null was provided". reference/system/xasyncprovider:
/// XAsyncGetResult with another identity than XAsyncBegin's is a call
/// mismatch, E_INVALIDARG.
fn edges(t: &mut Tap) {
    let a = api();

    let q = a
        .create(SERIALIZED_THREAD_POOL, SERIALIZED_THREAD_POOL)
        .expect("serialized queue");
    let overlap = Overlap::default();
    for _ in 0..16 {
        a.submit(q, WORK, ctx(&overlap), slow);
    }
    t.check(
        wait_for(5000, || overlap.done.load(Ordering::SeqCst) == 16),
        "xasync: a serialized thread-pool port runs every callback",
        || format!("{} of 16", overlap.done.load(Ordering::SeqCst)),
    );
    t.eq(
        overlap.most.load(Ordering::SeqCst),
        1,
        "xasync: ... one at a time",
    );
    a.close(q);
    let q = a
        .create(THREAD_POOL, THREAD_POOL)
        .expect("thread-pool queue");
    let parallel = Overlap::default();
    for _ in 0..16 {
        a.submit(q, WORK, ctx(&parallel), slow);
    }
    wait_for(5000, || parallel.done.load(Ordering::SeqCst) == 16);
    t.check(
        parallel.most.load(Ordering::SeqCst) > 1,
        "xasync: a thread-pool port runs callbacks in parallel",
        || "never more than one at a time".into(),
    );
    a.close(q);

    // A composite of a manual queue's work port: its callbacks are
    // dispatched from that queue.
    let base = a.create(MANUAL, MANUAL).expect("manual queue");
    let port = a.port(base, WORK);
    match a.composite(port, port) {
        Ok(c) => {
            let seen = Seen::default();
            a.submit(c, WORK, ctx(&seen), note);
            t.eq(
                a.dispatch(base, WORK, 0),
                true,
                "xasync: a composite queue's callback is dispatched from the queue owning its port",
            );
            t.eq(
                seen.calls.load(Ordering::SeqCst),
                1,
                "xasync: ... and runs once",
            );
            a.close(c);
        }
        Err(hr) => t.check(false, "xasync: XTaskQueueCreateComposite succeeds", || {
            format!("{hr:#010x}")
        }),
    }

    // A call mismatch: another identity than XAsyncBegin's.
    let mut block = Block::new(base, std::ptr::null_mut(), None);
    let b: *mut Block = &mut block;
    a.begin(
        b,
        std::ptr::null_mut(),
        eight_bytes as *const c_void,
        eight_bytes,
    );
    while a.dispatch(base, WORK, 0) || a.dispatch(base, COMPLETION, 0) {}
    let mut buffer = [0u8; 8];
    t.eq(
        a.result(b, forever as *const c_void, &mut buffer).0,
        E_INVALIDARG,
        "xasync: XAsyncGetResult with another identity is E_INVALIDARG",
    );
    a.result(b, eight_bytes as *const c_void, &mut buffer);
    a.close(base);

    // No process task queue: Get says none, and an async call with no queue
    // of its own fails with E_NO_TASK_QUEUE. The queue is put back after.
    let (had, previous) = a.process_queue();
    a.set_process_queue(std::ptr::null_mut());
    let (has, none) = a.process_queue();
    t.check(
        !has && none.is_null(),
        "xasync: with the process task queue set to nullptr, there is none",
        || format!("{has} {none:?}"),
    );
    let mut block = Block::on_process_queue();
    let hr = a.run(&mut block, work_ok);
    t.eq(
        hr,
        E_NO_TASK_QUEUE,
        "xasync: an async call with no queue and no process queue is E_NO_TASK_QUEUE",
    );
    if hr == S_OK {
        wait(&mut block);
    }
    if had {
        a.set_process_queue(previous);
        a.close(previous);
    }
    let (back, q) = a.process_queue();
    t.check(
        back == had,
        "xasync: the process task queue is put back",
        || format!("had {had}, now {back}"),
    );
    if back {
        a.close(q);
    }
}
