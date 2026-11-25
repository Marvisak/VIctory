//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap
//! (unless `VICTORY_LATE_WAIT=1`, see [`late`]);
//! overlap them with per-submission fences once more than clears run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    cpu_view: Mutex<ID3D12DescriptorHeap>,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Presentation>,
    /// The adapter the device is on, for its memory budget
    /// ([`Host::video_memory`]); `None` if the system does not name it.
    adapter: Option<IDXGIAdapter3>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Host objects the title let go of while a submission may still use them
/// (a texture its descriptors name, a descriptor slot's old resource), by
/// the submission count they wait for. The title's descriptor writes and
/// releases reach the host at once, from its own threads, while the host
/// runs a submission: freeing there is a GPU page fault on the freed
/// texture (a device hang).
static RETIRED: Mutex<Vec<(u64, Retired)>> = Mutex::new(Vec::new());
/// Submissions started and finished (waited for).
static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FINISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A retired object, dropped on whatever thread finishes the submission.
struct Retired(#[allow(dead_code)] Box<dyn std::any::Any>);
// SAFETY: what is retired is D3D12 objects (free-threaded) and the
// reference-counted wrappers holding them, dropped once.
unsafe impl Send for Retired {}

/// Milliseconds since the first call.
pub(crate) fn now_ms() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// This moment, for telling later how long ago it was ([`long_ago`]): the
/// frames begun so far (high half, `crate::frame::current`) and tenths of a
/// second since the start. Host copies of title memory, staging buffers and
/// host textures note when they were last used with it.
pub(crate) fn moment() -> u64 {
    (crate::frame::current() << 32) | ((now_ms() / 100) & 0xffff_ffff)
}

/// Whether `moment` lies at least `ms` milliseconds and `frames` frames
/// back. Both: at seconds per frame, what every frame uses is not touched
/// for seconds, and at many frames per second not for many frames. (A title
/// that never waits for frames has no frame count: nothing is then idle.)
pub(crate) fn long_ago(moment: u64, ms: u64, frames: u64) -> bool {
    let now = self::moment();
    let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
    (now >> 32).saturating_sub(moment >> 32) >= frames && tenths * 100 >= ms
}

/// How many references are held on a host object (its COM count): ours
/// all, since the host's views and commands hold none. A buffer or texture
/// with one is held by its owner alone: no descriptor slot keeps it
/// (`HostHeap::kept`) and no list (`HostList::keep`).
pub(crate) fn references(object: &impl Interface) -> u32 {
    type Method = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;
    let raw = object.as_raw();
    // SAFETY: a live COM object, whose first three methods are those of
    // `IUnknown` (query, add a reference, release one); the pair leaves the
    // count as it was, and the release returns it. The first is not called.
    unsafe {
        let methods = *raw.cast::<*const [Method; 3]>();
        ((*methods)[1])(raw);
        ((*methods)[2])(raw)
    }
}

/// Once a second, before a submission starts: lets go of what nothing
/// names any more (copies of windows of title memory, staging buffers,
/// host copies of textures the title fills).
/// `VICTORY_SWEEP=0` keeps everything, as before.
fn sweep() {
    // The in-place probe and census, if asked for (`VICTORY_INPLACE_PROBE`).
    crate::inplace::tick();
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 1000
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    crate::pipeline::trim_library();
    crate::shadow::let_go_idle();
    crate::texture::let_go_idle();
    crate::resource::let_go_idle();
}

/// Drops `object` once every submission running now has finished: at once
/// if none is.
pub(crate) fn retire<T: 'static>(object: T) {
    let started = STARTED.load(Ordering::Acquire);
    if started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
        drop(object);
        return;
    }
    RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((started, Retired(Box::new(object))));
}

/// Drops what waited for submissions up to `finished`.
fn bury(finished: u64) {
    let done: Vec<(u64, Retired)> = {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        if retired.is_empty() {
            return;
        }
        let (done, keep) = std::mem::take(&mut *retired)
            .into_iter()
            .partition(|(at, _)| *at <= finished);
        *retired = keep;
        done
    };
    drop(done);
}

/// Objects [`retire`] dropped at once (no submission was running) and
/// kept until a running submission finished, since the start.
static RETIREMENTS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// What the layer did to the host's objects lately, newest last (sweeps
/// that let host copies go, pipeline library calls, saves and trims): said
/// once when the host device is removed ([`log_removal_context`]), with
/// what the submitting thread and the pipeline workers were doing at that
/// moment. A device the driver removes "internally" names no command, and
/// the pass that happens to notice first names only the view it was at:
/// the three such losses of runs 72, 73 and 82 each came within 5 ms of a
/// pipeline library call that no line of the log tied to them.
/// `VICTORY_TRAIL=0`: nothing is kept and nothing is said.
static TRAIL: Mutex<std::collections::VecDeque<(u64, u32, String)>> = Mutex::new(std::collections::VecDeque::new());
/// The entries kept.
const TRAIL_MOST: usize = 96;

fn trail_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TRAIL").is_none_or(|v| v != "0"))
}

/// Microseconds since the first call.
pub(crate) fn now_us() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

/// The calling thread, as the log's other lines name threads.
pub(crate) fn thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
}

/// Notes something done to the host's objects ([`TRAIL`]); `what` is only
/// run when the trail is kept.
pub(crate) fn trail(what: impl FnOnce() -> String) {
    if !trail_on() {
        return;
    }
    let entry = (now_us(), thread_id(), what());
    let mut trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    if trail.len() >= TRAIL_MOST {
        trail.pop_front();
    }
    trail.push_back(entry);
}

/// What the submitting thread is doing (0 nothing: between submissions,
/// 1 preparing one, 2 waiting for the card to run it, 3 copying back what
/// it wrote), the lists of that submission, and since when ([`now_us`]).
static SUBMITTING: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

fn submitting(phase: u64, lists: usize) {
    SUBMITTING[1].store(lists as u64, Ordering::Relaxed);
    SUBMITTING[2].store(now_us(), Ordering::Relaxed);
    SUBMITTING[0].store(phase, Ordering::Relaxed);
}

/// What was going on when the host device went: whether a submission was
/// on the card, the pipeline library's state and the calls running in it,
/// and the trail.
pub(crate) fn removal_context() -> String {
    use std::fmt::Write;
    let now = now_us();
    let ms = |at: u64| now.saturating_sub(at) as f64 / 1000.0;
    let phase = match SUBMITTING[0].load(Ordering::Relaxed) {
        0 => "between submissions (nothing of ours was running on the card)",
        1 => "preparing a submission (nothing of ours was running on the card)",
        2 => "waiting for the card to run a submission",
        _ => "copying back what a submission wrote (nothing of ours was running on the card)",
    };
    let mut text = format!(
        "the submitting thread was {phase}, of {} lists, for {:.1} ms; {}; objects let go at once {} and after a running submission {}; submissions started {} finished {}",
        SUBMITTING[1].load(Ordering::Relaxed),
        ms(SUBMITTING[2].load(Ordering::Relaxed)),
        crate::pipeline::library_removal_note(),
        RETIREMENTS[0].load(Ordering::Relaxed),
        RETIREMENTS[1].load(Ordering::Relaxed),
        STARTED.load(Ordering::Relaxed),
        FINISHED.load(Ordering::Relaxed),
    );
    let trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    let _ = write!(text, "; the last {} things done, newest last:", trail.len());
    for (at, thread, what) in trail.iter() {
        let _ = write!(text, "\n  {:.1} ms before, thread {thread}: {what}", ms(*at));
    }
    text
}

/// Says [`removal_context`] in the log, once: called by whoever finds the
/// host device removed.
pub(crate) fn log_removal_context() {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !trail_on() || SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::error!("when the host device went: {}", removal_context());
}

/// Since the last frame line, microseconds of the submitting thread's
/// preparing ([`SUBMISSION_US`] 0) by phase: the submission's one
/// write-watch asking, the title's bytes put in under flights, the lists'
/// deferred steps, their state preludes, the last refreshes, the snapshots
/// of shader-written ranges, and the copies in the card's memory (what goes
/// up, the compare). The frame line's copies fall in the deferred steps and
/// the last refreshes. Settles inside a phase are not its time.
static PREPARE_SPLIT: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];
const PREPARE_ASK: usize = 0;
const PREPARE_MERGE: usize = 1;
const PREPARE_DEFERRED: usize = 2;
const PREPARE_PRELUDES: usize = 3;
const PREPARE_FLUSH: usize = 4;
const PREPARE_SNAPSHOTS: usize = 5;
const PREPARE_CARD: usize = 6;

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, bytes that differed), since the last
/// frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// The card's clock, for the frame line of every run, in microseconds since
/// the last one: how long the card had nothing of ours queued; how much of
/// that while the thread that holds the submission lock was working (the
/// card idle behind us: preparing and copying back are done with the card
/// standing); and how long that thread stood waiting for the card.
///
/// Every send of lists to the queue is followed by a signal on a fence of
/// the clock's own ([`Host::card_sent`]). A thread of the clock waits for
/// the newest signal and notes the moment the card reached it with nothing
/// newer sent; the next send ends that idle stretch.
static CARD_CLOCK_US: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Of the thread that holds the submission lock: the microseconds it has
/// worked so far (preparing, copying back; not its waits for the card), and
/// since when it works now ([`now_us`] plus one; 0: it does not).
static WORKING: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The thread that holds the submission lock starts (`on`) or stops
/// working; whether it worked until now.
fn working(on: bool) -> bool {
    let now = now_us() + 1;
    let since = WORKING[1].swap(if on { now } else { 0 }, Ordering::Relaxed);
    if since != 0 {
        WORKING[0].fetch_add(now.saturating_sub(since), Ordering::Relaxed);
    }
    since != 0
}

/// [`WORKING`] as of now: microseconds worked so far.
fn worked() -> u64 {
    let since = WORKING[1].load(Ordering::Relaxed);
    let now = if since != 0 { (now_us() + 1).saturating_sub(since) } else { 0 };
    WORKING[0].load(Ordering::Relaxed) + now
}

/// The thread that holds the submission lock stood waiting for the card
/// from `since` until now ([`CARD_CLOCK_US`]).
fn stood(since: std::time::Instant) {
    CARD_CLOCK_US[2].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// What [`CARD_CLOCK_US`] is measured with.
struct CardClock {
    fence: ID3D12Fence,
    /// Signals sent so far; and, while the card has reached the last of
    /// them, since when and what [`worked`] said then.
    state: Mutex<(u64, Option<(std::time::Instant, u64)>)>,
    sent: std::sync::Condvar,
}

// SAFETY: a fence (free-threaded) behind a lock.
unsafe impl Send for CardClock {}
// SAFETY: as above.
unsafe impl Sync for CardClock {}

/// The clock's thread: waits for the newest signal sent and notes when the
/// card got there with nothing newer behind it.
fn card_clock_watch(clock: &'static CardClock) {
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        return;
    };
    let mut seen = 0;
    loop {
        let target = {
            let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.0 == seen {
                state = clock.sent.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            state.0
        };
        // SAFETY: the clock's fence and this thread's event.
        unsafe {
            if clock.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        seen = target;
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 == target && state.1.is_none() {
            state.1 = Some((std::time::Instant::now(), worked()));
        }
    }
}

/// For the frame-rate line: [`CARD_CLOCK_US`] per frame, `ms` being the
/// frame's time.
fn card_clock_line(frames: f64, ms: f64) {
    let [idle, behind, stood] = [0, 1, 2].map(|i| CARD_CLOCK_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
    tracing::info!(
        "the card's clock per frame: nothing of ours queued {idle:.1} ms (the card about {:.0}% busy by this count), \
         {behind:.1} ms of that while the submitting thread was working (the card idle behind us), the rest while the title \
         had not submitted; the submitting thread stood waiting for the card {stood:.1} ms",
        if ms > 0.0 { (100.0 * (1.0 - idle / ms)).clamp(0.0, 100.0) } else { 0.0 }
    );
}

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Host buffers made since the start, as (count, bytes): in the upload heap
/// (staging, list scratch), shared with the processor (host copies of title
/// memory) and the graphics card's own (indirect arguments, twins' copies).
/// For the memory line: against what is alive, they show what was let go.
pub(crate) static MADE: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// What of that is alive now, for the memory line (`MADE` counts what was
/// ever made: 42,607 upload buffers, 77.9 GB, in five minutes of run 60,
/// and nothing said how many were alive): host lists; the lists' scratch
/// chunks and their bytes; of those, the chunks no list holds (kept for the
/// next list, [`Spare`]) and their bytes; chunks taken from those kept
/// instead of made, since the start; and the lists' other upload buffers
/// (texture data copied from buffers, scratch too big for a chunk) and
/// their bytes.
pub(crate) static ALIVE: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];

/// About host lists, for the memory line: lists made by the host since the
/// start; preludes made (a list's second allocator and list, made when it
/// first needs one, [`lazy_prelude`]); and, in diagnostics builds, the
/// bytes the process's private memory grew by while lists were being made
/// (what a list costs: host objects with the driver's command memory; 631
/// lists were alive in run 65, each with a prelude it may never use).
/// Then the lists alive that were never reset since they were made: the
/// host sides of title lists the title made and has not used (a host list
/// cost 0.68 MB of private memory in run 69: 431 MB for the 632 the title
/// makes at its start).
pub(crate) static LISTS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The process's private bytes (diagnostics: around the making of lists).
#[cfg(feature = "diagnostics")]
fn private_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    c.PagefileUsage as u64
}

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// Ends the run on a host GPU failure: what failed, the removal reason and
/// the debug layer's first error, if any (`VICTORY_D3D12_DEBUG=1` in a
/// diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
            log_removal_context();
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => text.push_str(
                "; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command",
            ),
            None => {}
        }
    }
    tracing::error!("{text}");
    // Tests have no log: the reason goes to their output.
    #[cfg(test)]
    eprintln!("host GPU failure: {text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
pub(crate) fn host() -> Option<&'static Host> {
    static HOST: OnceLock<Option<Host>> = OnceLock::new();
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

fn create() -> windows::core::Result<Host> {
    crate::tdr::log();
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // The debug layer only in builds with diagnostics.
        let debug =
            cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_DEBUG");
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            // GPU-based validation (`VICTORY_D3D12_GBV`): out-of-range
            // descriptor heap indices and the like become messages naming
            // the command instead of page faults. Without state tracking
            // (directly indexed textures are not transitioned).
            if std::env::var_os("VICTORY_D3D12_GBV").is_some()
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                }
                if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let mut device: Option<ID3D12Device> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        if cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_WARP") {
            let adapter = warp_adapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        // `VICTORY_D3D12_BACKGROUND=0`: the runtime's background work (the
        // driver compiling shaders again on the "D3D Background Thread"s,
        // for minutes after pipelines were made or loaded) is switched off.
        // An experiment: while those threads ran, the same shot took
        // 1,000-1,200 ms a frame, and 830-950 ms from the moment they
        // stopped (run 54, 08:32:01 to 08:32:06). Left as it is by default:
        // whether the card is slower with the shaders it then keeps is not
        // known.
        if std::env::var_os("VICTORY_D3D12_BACKGROUND").is_some_and(|v| v == "0")
            && let Ok(device6) = device.cast::<ID3D12Device6>()
        {
            let set = device6.SetBackgroundProcessingMode(
                D3D12_BACKGROUND_PROCESSING_MODE_DISABLE_BACKGROUND_WORK,
                D3D12_MEASUREMENTS_ACTION_KEEP_ALL,
                None,
                None,
            );
            tracing::info!(ok = set.is_ok(), "the host's background shader work is switched off (VICTORY_D3D12_BACKGROUND=0)");
        }
        // the widest wave the GPU runs: 32 (NVIDIA) gets console wave64 compute programs whose
        // cross-lane code stays in 32-lane halves translated for waves of 32
        let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        if device
            .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS1, (&raw mut o1).cast(), size_of_val(&o1) as u32)
            .is_ok()
        {
            tracing::info!(min = o1.WaveLaneCountMin, max = o1.WaveLaneCountMax, "host wave lanes");
            victory_shader::emit::set_host_max_wave(o1.WaveLaneCountMax);
        }
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        let cpu_view = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: 1,
            ..Default::default()
        })?;
        let adapter = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid()))
            .ok();
        tracing::info!("host D3D12 device ready");
        Ok(Host {
            device,
            queue,
            fence,
            submitted: Mutex::new(0),
            event,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            cpu_view: Mutex::new(cpu_view),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(Presentation::default()),
            adapter,
        })
    }
}

/// DXGI may return hardware for `EnumWarpAdapter` (notably under Proton).
/// Reject it before creating a device in diagnostics' software-only mode.
fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

/// Tests: the turn to use the software adapter. D3D12 makes one device per
/// adapter, so every test's WARP device is the same one, and WARP compiles
/// a pipeline's shader on its own threads when a queue first runs it: two
/// queues doing that at once can end the process (an access violation in
/// d3d10warp.dll, on one of its threads, its compiler reading entry -1 of a
/// table). Two threads that each ran new pipelines in a submission of their
/// own at the same moment crashed in 10 runs of 10; the same with only one
/// of them running (the other making pipelines), or with both running
/// pipelines that had run before, in none. `indirect::`, its WARP tests in
/// parallel, ended so in 6 runs of 510. A test that uses WARP holds the
/// turn until everything it made there is released.
#[cfg(test)]
pub(crate) fn warp_turn() -> std::sync::MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    // Poisoned only by a test that failed holding it, which released what
    // it made before giving it back.
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    #[test]
    fn texture_uploads_share_a_chunk_at_aligned_offsets() {
        use windows::core::Interface;

        let Some(host) = super::host() else {
            return; // no host GPU
        };
        let list = host.new_list().unwrap();
        let (_, start) = list.scratch(32).unwrap();
        let (first, first_offset, first_data) = list.upload(32).unwrap();
        let (second, second_offset, second_data) = list.upload(32).unwrap();

        assert_eq!(first.as_raw(), second.as_raw());
        assert_eq!((first_offset, second_offset), (512, 1024));
        assert_eq!(first_data as usize - start as usize, first_offset as usize);
        assert_eq!(second_data as usize - start as usize, second_offset as usize);
        list.close();
    }

    #[test]
    fn sparse_copies_preserve_islands_and_observe_remaps() {
        use windows_sys::Win32::System::Memory::*;
        const N: usize = 5 << 16;
        // SAFETY: we reserve a private region, commit selected pages inside it,
        // and only directly access the pages while they are writable.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            let mut expected = vec![0; N];
            for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
                let at = base.add(offset);
                assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                at.write_bytes(value, 4096);
                expected[offset..offset + 4096].fill(value);
            }
            for remap in [false, true] {
                if remap {
                    let mut old = 0;
                    assert_ne!(VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old), 0);
                    expected[..4096].fill(0);
                    let at = base.add(1 << 16);
                    assert_eq!(VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE), at.cast());
                    at.write_bytes(0xc7, 4096);
                    expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
                }
                let mut out = vec![0xee; N];
                super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
                assert_eq!(out[27..N - 27], expected[27..N - 27]);
                assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
            }
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn write_mapped_skips_unmapped_and_readonly_pages_and_marks_write_watch() {
        use windows_sys::Win32::System::Memory::*;
        const PAGE: usize = 4096;
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        const N: usize = 4 * PAGE;
        // SAFETY: private test allocations freed before returning.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            // Page 0: writable, Page 1: PAGE_NOACCESS (reserved), Page 2: READONLY, Page 3: writable.
            assert_eq!(VirtualAlloc(base.cast(), PAGE, MEM_COMMIT, PAGE_READWRITE), base.cast());
            let p2 = base.add(2 * PAGE);
            assert_eq!(VirtualAlloc(p2.cast(), PAGE, MEM_COMMIT, PAGE_READWRITE), p2.cast());
            p2.write_bytes(0x5a, PAGE);
            let mut old = 0;
            assert_ne!(VirtualProtect(p2.cast(), PAGE, PAGE_READONLY, &mut old), 0);
            let p3 = base.add(3 * PAGE);
            assert_eq!(VirtualAlloc(p3.cast(), PAGE, MEM_COMMIT, PAGE_READWRITE), p3.cast());

            let src = vec![0xa3u8; N];
            super::write_mapped(base, src.as_ptr(), N);
            assert!(std::slice::from_raw_parts(base, PAGE).iter().all(|&b| b == 0xa3));
            assert!(std::slice::from_raw_parts(p2, PAGE).iter().all(|&b| b == 0x5a));
            assert!(std::slice::from_raw_parts(p3, PAGE).iter().all(|&b| b == 0xa3));
            assert!(!super::read_into(base, &mut vec![0u8; N]));
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);

            // Verify `direct_copy::try_copy` triggers write-watch tracking on committed pages.
            let ww = VirtualAlloc(
                std::ptr::null(),
                2 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!ww.is_null());
            assert_eq!(ResetWriteWatch(ww.cast(), 2 * PAGE), 0);
            let payload = vec![0x42u8; PAGE];
            super::write_mapped(ww.add(PAGE), payload.as_ptr(), PAGE);
            let mut dirty = [std::ptr::null_mut(); 4];
            let mut count = dirty.len();
            let mut granularity = 0;
            assert_eq!(
                GetWriteWatch(0, ww.cast(), 2 * PAGE, dirty.as_mut_ptr(), &mut count, &mut granularity),
                0
            );
            assert_eq!(count, 1);
            assert_eq!(dirty[0] as usize, ww.add(PAGE) as usize);
            assert_ne!(VirtualFree(ww.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// The direct copy ends where a page is not there, cannot be written or
    /// is a guard page, with what lay before it copied, and the thread goes
    /// on; eight threads faulting at once all come back.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn direct_copy_ends_at_missing_unwritable_and_guard_pages() {
        use windows_sys::Win32::System::Memory::*;
        const PAGE: usize = 4096;
        // SAFETY: private test allocations freed before returning; every
        // copy names their pages, some missing or protected on purpose.
        unsafe {
            // Page 0 and 2 committed, 1 missing.
            let base = VirtualAlloc(std::ptr::null(), 3 * PAGE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), PAGE, MEM_COMMIT, PAGE_READWRITE), base.cast());
            let p2 = base.add(2 * PAGE);
            assert_eq!(VirtualAlloc(p2.cast(), PAGE, MEM_COMMIT, PAGE_READWRITE), p2.cast());
            base.write_bytes(0x11, PAGE);
            p2.write_bytes(0x33, PAGE);
            // A missing source page.
            let mut dst = vec![0u8; 3 * PAGE];
            assert!(!super::direct_copy::try_copy(dst.as_mut_ptr(), base, 3 * PAGE));
            assert!(dst[..PAGE].iter().all(|&b| b == 0x11), "what lay before the hole was copied");
            let mut again = vec![0u8; PAGE];
            assert!(super::direct_copy::try_copy(again.as_mut_ptr(), p2, PAGE));
            assert!(again.iter().all(|&b| b == 0x33));
            // A destination that cannot be written (read-only, no guard of ours).
            let mut old = 0;
            assert_ne!(VirtualProtect(base.cast(), PAGE, PAGE_READONLY, &mut old), 0);
            let src = [0x5au8; 16];
            assert!(!super::direct_copy::try_copy(base, src.as_ptr(), 16));
            assert_eq!(*base, 0x11, "the read-only page kept what it had");
            assert_ne!(VirtualProtect(base.cast(), PAGE, PAGE_READWRITE, &mut old), 0);
            // A guard page as the source: the copy ends, the process goes on.
            assert_ne!(VirtualProtect(p2.cast(), PAGE, PAGE_READWRITE | PAGE_GUARD, &mut old), 0);
            assert!(!super::direct_copy::try_copy(again.as_mut_ptr(), p2, PAGE));
            // Eight threads faulting at once.
            let at = base as usize;
            std::thread::scope(|s| {
                for _ in 0..8 {
                    s.spawn(move || {
                        let mut dst = vec![0u8; 3 * PAGE];
                        for _ in 0..200 {
                            assert!(!super::direct_copy::try_copy(dst.as_mut_ptr(), at as *const u8, 3 * PAGE));
                        }
                    });
                }
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    #[ignore = "benchmark"]
    fn benchmark_direct_copy_vs_kernel_calls() {
        use std::time::Instant;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        const SIZE: usize = 64 << 20;
        const CHUNK: usize = 64 << 10;
        const ITERS: usize = 4;
        let src = vec![0x3cu8; SIZE];
        let mut dst = vec![0u8; SIZE];

        let t0 = Instant::now();
        for _ in 0..ITERS {
            assert!(super::read_kernel(src.as_ptr(), dst.as_mut_ptr(), SIZE));
        }
        let read_kernel_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        for _ in 0..ITERS {
            // SAFETY: valid test vectors.
            assert!(unsafe { super::direct_copy::try_copy(dst.as_mut_ptr(), src.as_ptr(), SIZE) });
        }
        let read_direct_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let chunks = 1000;
        let t2 = Instant::now();
        for i in 0..chunks {
            let off = (i * 4096) % (SIZE - CHUNK);
            let mut done = 0;
            // SAFETY: valid test vectors.
            let ok = unsafe {
                super::NtWriteVirtualMemory(
                    GetCurrentProcess(),
                    dst.as_mut_ptr().add(off).cast(),
                    src.as_ptr().add(off).cast(),
                    CHUNK,
                    &mut done,
                ) >= 0
            };
            assert!(ok);
        }
        let write_nt_ms = t2.elapsed().as_secs_f64() * 1000.0;

        let t3 = Instant::now();
        for i in 0..chunks {
            let off = (i * 4096) % (SIZE - CHUNK);
            // SAFETY: valid test vectors.
            let ok = unsafe {
                super::direct_copy::try_copy(dst.as_mut_ptr().add(off), src.as_ptr().add(off), CHUNK)
            };
            assert!(ok);
        }
        let write_direct_ms = t3.elapsed().as_secs_f64() * 1000.0;

        eprintln!(
            "256MB bulk read: kernel={read_kernel_ms:.2}ms ({:.0} MB/s) direct={read_direct_ms:.2}ms ({:.0} MB/s) | 1000x64K write: NtWrite={write_nt_ms:.2}ms ({:.0} MB/s) direct={write_direct_ms:.2}ms ({:.0} MB/s)",
            256.0 / (read_kernel_ms / 1000.0),
            256.0 / (read_direct_ms / 1000.0),
            (chunks as f64 * 64.0 / 1024.0) / (write_nt_ms / 1000.0),
            (chunks as f64 * 64.0 / 1024.0) / (write_direct_ms / 1000.0),
        );
    }

    use super::*;

    #[test]
    fn warp_adapter_accepts_only_software() {
        // This probes DXGI without creating a D3D12 device or running a title.
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
        // IDXGIAdapter1; `raw` owns that interface.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }

    /// A WARP test that fails while it holds the turn does not fail the
    /// ones after it.
    #[test]
    fn warp_turn_is_taken_again_after_a_test_failed_holding_it() {
        let failed = std::thread::spawn(|| {
            let _turn = warp_turn();
            panic!("a WARP test failing while it holds the turn");
        })
        .join();
        assert!(failed.is_err());
        drop(warp_turn());
    }
}

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    states: Mutex<States>,
    /// What the list does with depth textures that have a colour twin, by
    /// twin ([`TwinUse`]).
    twins: Mutex<std::collections::HashMap<usize, TwinUse>>,
    /// The list that runs those transitions just before this one, made
    /// when the list first has such transitions ([`lazy_prelude`]).
    prelude: OnceLock<Option<Prelude>>,
    /// Further preludes, for a submission that names the list more than
    /// once: each time needs its own, since the ones before it are queued
    /// and have not run when it is recorded.
    repeats: Mutex<Vec<Prelude>>,
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    /// The work its indirect executions may still do (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted.
    budget: OnceLock<Option<(ID3D12Resource, usize)>>,
    /// Device-local scratch buffer for clamped indirect arguments (`indirect`),
    /// grown to the largest indirect execution recorded on the list.
    indirect_args: Mutex<Option<(ID3D12Resource, u64)>>,
    /// Counts the list in [`ALIVE`] while it lives.
    _alive: Counted,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    /// With `VICTORY_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

/// A list's prelude: an allocator and a closed list.
type Prelude = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

/// Whether a host list's prelude is made when the list first needs one. A
/// prelude runs the texture transitions and twin copies a list needs before
/// it; lists that name no texture never have one, and every host list was
/// made with one all the same: twice the host objects per list, for the
/// title's 630 lists and for every list made anew.
/// `VICTORY_LAZY_PRELUDE=0`: made with the list, as before.
fn lazy_prelude() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAZY_PRELUDE").is_none_or(|v| v != "0"))
}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

type States = std::collections::HashMap<
    usize,
    (u32, u32, std::sync::Arc<crate::resource::HostTexture>),
>;

/// What a list does with a depth texture that has a colour twin
/// ([`crate::resource::Twin`]). Lists are recorded on many threads and run
/// in submission order, so a list cannot know which side an earlier list
/// leaves behind: it notes which sides it uses as it finds them, and its
/// prelude copies one side into the other at submission if that side is
/// behind then. From its first use of a side on, the list knows.
struct TwinUse {
    /// The depth texture (the title resource's host copy) and its twin.
    depth: std::sync::Arc<crate::resource::HostTexture>,
    twin: std::sync::Arc<crate::resource::Twin>,
    /// The sides (depth texture, twin) the list uses as it finds them.
    needs: [bool; 2],
    /// Whether each side lacks what the other was written last, where the
    /// list is now; `None`: as the list found it.
    behind: [Option<bool>; 2],
    /// The draw or dispatch being prepared writes the twin.
    writing: bool,
    /// The depth plane's twin holds what the plane lacks although the list
    /// copied the twins since: the plane's pass waits for the list's close.
    late: bool,
}

/// The pass that draws the twin of a depth plane into the plane: one
/// triangle over each mip of each slice, its pixel shader writing the
/// twin's texel as the depth, with the depth test "always" and no stencil
/// (the stencil plane is copied through a buffer and not touched here).
///
/// A 16-bit twin holds whole steps of 1/65535, and the depth written is
/// the step times the float nearest 1/65535 (0x37800080). That is how
/// The title's tiled lighting computes the depth of the
/// rectangles it then draws with the test "equal", one class of materials
/// a draw (all six of its vertex shaders multiply the class number by that
/// constant), so the pass and the draw give the host the same float, and
/// the host turns the same float into the same 24 bits. The step divided
/// by 65535 is another float for 512 of the 65,536 steps (none under 257),
/// and lands in another 24-bit step for 127 of them (none under 385).
const PLANE_CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
const PLANE_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<uint> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return float(twin.Load(int4(int2(p.xy), 0, 0))) * asfloat(0x37800080);
}
"#;
const PLANE_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<float> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return twin.Load(int4(int2(p.xy), 0, 0));
}
"#;

/// Views of twins the pass reads, in a heap of ours used as a ring.
/// shortcut: a view must last until its list has run; with one or two
/// passes a frame the ring is far from coming round.
const PLANE_VIEWS: u32 = 256;

/// What the pass draws with: the heap of its views of twins, and per kind
/// of twin (16-bit steps or floats) and depth format the root signature
/// and pipeline, made when first needed (`None`: the host refused).
struct PlanePass {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    made: Mutex<Vec<(bool, i32, Option<PlanePipeline>)>>,
}

/// The pass's root signature and pipeline for one kind of twin and one
/// depth format.
type PlanePipeline = (ID3D12RootSignature, ID3D12PipelineState);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for PlanePass {}
// SAFETY: as above.
unsafe impl Sync for PlanePass {}

fn plane_pass() -> Option<&'static PlanePass> {
    static PASS: OnceLock<Option<PlanePass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation and queries.
        unsafe {
            let views: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: PLANE_VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            Some(PlanePass {
                cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                step: host.device.GetDescriptorHandleIncrementSize(kind),
                views,
                next: AtomicU32::new(0),
                made: Mutex::default(),
            })
        }
    })
    .as_ref()
}

/// Compiles function `entry` of `source` for `target` with the system's
/// compiler, as the indirect-argument clamp is (`crate::indirect`).
pub(crate) fn compile(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("victory"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("a shader of ours: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

impl PlanePass {
    /// The root signature and pipeline for a twin of 16-bit steps (`steps`)
    /// or of floats and the depth format `format`.
    fn pipeline(&self, steps: bool, format: DXGI_FORMAT) -> Option<PlanePipeline> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, pipeline)) = made.iter().find(|m| (m.0, m.1) == (steps, format.0)) {
            return pipeline.clone();
        }
        let pipeline = match Self::make(steps, format) {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    steps,
                    format = format.0,
                    "no pipeline to draw a depth plane from its twin: such planes stay as they are"
                );
                None
            }
        };
        made.push((steps, format.0, pipeline.clone()));
        pipeline
    }

    fn make(steps: bool, format: DXGI_FORMAT) -> windows::core::Result<PlanePipeline> {
        use windows::core::s;
        let device = &host().ok_or_else(windows::core::Error::empty)?.device;
        let vs = compile(PLANE_CORNERS, s!("corner"), s!("vs_5_1"))?;
        let source = if steps { PLANE_STEPS } else { PLANE_FLOATS };
        let ps = compile(source, s!("plane"), s!("ps_5_1"))?;
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        // SAFETY: the root signature is inside the pixel shader; the
        // description points at live data and never releases its pointer.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                // Every texel is written; stencil stays off.
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                DSVFormat: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
            Ok((root, pipeline))
        }
    }
}

/// Records into `list` the pass that draws `plane`, the twin of the depth
/// plane of `depth`, into that plane: `depth` in the depth-write state,
/// `plane` readable by pixel shaders. The pass sets the list's descriptor
/// heap, graphics root signature, pipeline, depth target, viewport,
/// scissor and topology and puts none of them back: the caller sets the
/// title's again, or the list records nothing of the title's after it.
fn record_plane_pass(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    plane: &ID3D12Resource,
) {
    let (Some(host), Some(pass)) = (host(), plane_pass()) else {
        return;
    };
    // SAFETY: live resources.
    let (of, twin) = unsafe { (depth.GetDesc(), plane.GetDesc()) };
    // An R16 typeless twin (53) holds steps, read as R16_UINT (57); an R32
    // typeless one floats, read as R32_FLOAT (41). The depth view names
    // the depth-stencil format of the texture's family (44: 45, 19: 20).
    let steps = twin.Format.0 == 53;
    let format = DXGI_FORMAT(match of.Format.0 {
        44 => 45,
        19 => 20,
        typed => typed,
    });
    let Some((root, pipeline)) = pass.pipeline(steps, format) else {
        return;
    };
    for slice in 0..u32::from(of.DepthOrArraySize) {
        for mip in 0..u32::from(of.MipLevels) {
            let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PLANE_VIEWS;
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(if steps { 57 } else { 41 }),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                        MostDetailedMip: mip,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        ..Default::default()
                    },
                },
            };
            let target = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                        MipSlice: mip,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    },
                },
                ..Default::default()
            };
            let target = host.dsv(depth, Some(&target));
            let (width, height) = ((of.Width >> mip).max(1) as u32, (of.Height >> mip).max(1));
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: pass.cpu + (slot * pass.step) as usize,
            };
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: pass.gpu + u64::from(slot * pass.step),
            };
            // SAFETY: an open list; a slot of our heap; live resources and
            // objects (the pass's own live as long as the process).
            unsafe {
                host.device
                    .CreateShaderResourceView(plane, Some(&view), cpu);
                list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
                list.SetGraphicsRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetGraphicsRootDescriptorTable(0, gpu);
                list.OMSetRenderTargets(0, None, false, Some(&target));
                list.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: width as f32,
                    Height: height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }]);
                list.IASetPrimitiveTopology(
                    windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4),
                );
                list.DrawInstanced(3, 1, 0, 0);
            }
            crate::resource::PLANES[0].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Records into `list` the copy of a depth texture into its colour twin
/// (`colour`) or of the twin into the depth texture, both already in their
/// copy states: whole textures of one size in one format family, which the
/// host copies as they are; or, for the twin of a stencil plane, that plane
/// alone, through the twin's buffer.
fn record_twin_copy(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    twin: &crate::resource::Twin,
    colour: bool,
) {
    let (from, to) = if colour {
        (depth, &twin.colour.texture)
    } else {
        (&twin.colour.texture, depth)
    };
    crate::resource::TWINS[2 + usize::from(colour)].fetch_add(1, Ordering::Relaxed);
    // shortcut: the depth plane's twin is not given the plane (that is a
    // pass the other way: the plane read by a shader, the twin as its
    // target). The twin then lacks what draws and clears wrote to the
    // plane since shaders last wrote the twin. In the title
    // nothing writes that plane but the twin's pass (its 192 draws against
    // it only test, run 29), so the twin lacks nothing. Counted.
    if colour && twin.plane.is_some() {
        crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
    }
    let Some(through) = &twin.through else {
        // SAFETY: an open list and two live textures.
        unsafe { list.CopyResource(to, from) };
        return;
    };
    // A stencil plane and its twin: each mip and slice of the plane goes
    // into the buffer, laid out as the host lays the plane out for copies,
    // and from the buffer into the other texture, whole subresources both
    // times. In the depth texture the stencil plane's subresources follow
    // the depth plane's; the twin has the one plane.
    let Some(host) = host() else { return };
    // SAFETY: a live texture.
    let of = unsafe { depth.GetDesc() };
    let planes = u32::from(of.MipLevels) * u32::from(of.DepthOrArraySize);
    let mut rows = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); planes as usize];
    // SAFETY: a plain query writing `planes` footprints.
    unsafe {
        host.device.GetCopyableFootprints(
            &of,
            planes,
            planes,
            0,
            Some(rows.as_mut_ptr()),
            None,
            None,
            None,
        );
    }
    let (first_from, first_to) = if colour { (planes, 0) } else { (0, planes) };
    let subresource = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index,
        },
    };
    let placed = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(through) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, source, dest) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an open list and live resources; the buffer was made as big
    // as these footprints, and rests in the common state between copies.
    unsafe {
        list.ResourceBarrier(&[transition(through, common, dest)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_from + i as u32;
            list.CopyTextureRegion(&placed(row), 0, 0, 0, &subresource(from, index), None);
        }
        list.ResourceBarrier(&[transition(through, dest, source)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_to + i as u32;
            list.CopyTextureRegion(&subresource(to, index), 0, 0, 0, &placed(row), None);
        }
        list.ResourceBarrier(&[transition(through, source, common)]);
    }
}

#[cfg(feature = "diagnostics")]
fn traced_shaders() -> &'static [String] {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    HASHES.get_or_init(|| {
        std::env::var("VICTORY_TRACE_SHADERS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|hash| !hash.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

#[cfg(feature = "diagnostics")]
fn trace_shader_command(what: &str, shaders: &[String]) {
    static LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if shaders.iter().any(|hash| what.contains(hash.as_str()))
        && LOGGED.fetch_add(1, Ordering::Relaxed) < 500
    {
        tracing::info!("traced command: {what}");
    }
}

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_DRED")
    })
}

/// Indirect mesh dispatches since the last frame line: the largest group
/// count a record asked for, and the groups asked in all (from the lists'
/// budget buffers, a run late).
static MESH_GROUPS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`VICTORY_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `VICTORY_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_TIMING").is_some())
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// One in [`ALIVE`]`[0]` while it lives (a host list), and one in
/// [`LISTS`]`[3]` until it is first reset.
struct Counted {
    /// Not reset since it was made.
    fresh: std::sync::atomic::AtomicBool,
}

impl Counted {
    fn new() -> Counted {
        ALIVE[0].fetch_add(1, Ordering::Relaxed);
        LISTS[3].fetch_add(1, Ordering::Relaxed);
        Counted {
            fresh: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// The list is reset: it is in use.
    fn used(&self) {
        if self.fresh.swap(false, Ordering::Relaxed) {
            LISTS[3].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        ALIVE[0].fetch_sub(1, Ordering::Relaxed);
        self.used();
    }
}

/// A chunk of upload memory for lists' scratch data: the buffer, its
/// mapping and its host GPU address.
struct Chunk {
    buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

// SAFETY: a D3D12 resource (free-threaded) and its persistent mapping,
// written only by the list that holds the chunk.
unsafe impl Send for Chunk {}

/// Upload chunks handed out front to back: the chunks (each with its
/// bytes), the one in use and the bytes used in it.
#[derive(Default)]
struct Scratch {
    chunks: Vec<(Chunk, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

impl Drop for Scratch {
    fn drop(&mut self) {
        give_chunks(std::mem::take(&mut self.chunks));
    }
}

const SCRATCH_CHUNK: u64 = 4 << 20;
/// A list's first chunk when the chunks are shared ([`scratch_pool`]): most
/// lists put a few constants or a rectangle's corners there, and every
/// list that did took 4 MiB of the PC's memory for it.
const SCRATCH_FIRST: u64 = 256 << 10;
/// The most bytes of chunks kept while no list holds them.
const SPARE_MOST: u64 = 128 << 20;

/// Whether lists share their scratch chunks: a list takes a chunk when it
/// first needs one, a small one first, and gives all of them back when it
/// is reset or dropped, for the next list. Before, a list kept every chunk
/// it ever took (4 MiB each) for as long as it lived, used or not, and the
/// host lists of held submissions, which live for one submission, each
/// made theirs anew: some 57 upload buffers of 4 MiB made, zeroed and
/// freed per frame of gameplay (run 60).
/// `VICTORY_SCRATCH_POOL=0`: chunks of 4 MiB kept by their list, as before.
fn scratch_pool() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SCRATCH_POOL").is_none_or(|v| v != "0"))
}

/// Things of some sizes that nothing holds now, kept for whoever next asks
/// for one of the same size.
struct Spare<T> {
    kept: Vec<(T, u64)>,
    bytes: u64,
}

impl<T> Spare<T> {
    const fn new() -> Self {
        Spare { kept: Vec::new(), bytes: 0 }
    }

    /// A kept thing of `size` bytes, the one given last.
    fn take(&mut self, size: u64) -> Option<T> {
        let at = self.kept.iter().rposition(|(_, s)| *s == size)?;
        self.bytes -= size;
        Some(self.kept.remove(at).0)
    }

    /// Keeps `thing` (`size` bytes) unless that takes the kept bytes past
    /// `most`: then it comes back.
    fn give(&mut self, thing: T, size: u64, most: u64) -> Option<T> {
        if self.bytes + size > most {
            return Some(thing);
        }
        self.bytes += size;
        self.kept.push((thing, size));
        None
    }
}

/// Scratch chunks no list holds.
static SPARE: Mutex<Spare<Chunk>> = Mutex::new(Spare::new());

/// A scratch chunk of `size` bytes: one kept ([`SPARE`]) or a new one.
fn take_chunk(host: &Host, size: u64) -> Option<Chunk> {
    if scratch_pool()
        && let Some(chunk) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)
    {
        ALIVE[3].fetch_sub(1, Ordering::Relaxed);
        ALIVE[4].fetch_sub(size, Ordering::Relaxed);
        ALIVE[5].fetch_add(1, Ordering::Relaxed);
        return Some(chunk);
    }
    let (buffer, data) = host.upload_buffer(size)?;
    name(&buffer, || "list scratch".into());
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    ALIVE[1].fetch_add(1, Ordering::Relaxed);
    ALIVE[2].fetch_add(size, Ordering::Relaxed);
    Some(Chunk {
        buffer,
        data: data as usize,
        gpu,
    })
}

/// A list is done with `chunks` (it was reset after its submissions ran,
/// or dropped): kept for the next list, or freed past [`SPARE_MOST`] and
/// without the pool.
fn give_chunks(chunks: Vec<(Chunk, u64)>) {
    if chunks.is_empty() {
        return;
    }
    let mut freed = Vec::new();
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        for (chunk, size) in chunks {
            let back = if scratch_pool() {
                spare.give(chunk, size, SPARE_MOST)
            } else {
                Some(chunk)
            };
            match back {
                None => {
                    ALIVE[3].fetch_add(1, Ordering::Relaxed);
                    ALIVE[4].fetch_add(size, Ordering::Relaxed);
                }
                Some(chunk) => {
                    ALIVE[1].fetch_sub(1, Ordering::Relaxed);
                    ALIVE[2].fetch_sub(size, Ordering::Relaxed);
                    freed.push(chunk);
                }
            }
        }
    }
    // Freed outside the lock.
    drop(freed);
}

/// What the host keeps for copies in the card's own memory
/// ([`crate::shadow::cards`]): three lists of ours and the compare.
struct CardWork {
    /// Before a submission's lists: what changed on the copies' CPU side
    /// goes to their card side. Those not in use: a plan takes one and
    /// gives it back when its submission has run (`Host::card_done`), so
    /// that submissions sent ahead ([`late`]) each have their own.
    sync: Vec<Prelude>,
    /// After them, in the same submission: the compare of the declared
    /// ranges with their bytes from before. As `sync`.
    check: Vec<Prelude>,
    /// After it ran: the pieces that differ come down.
    fetch: Prelude,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// One dword per piece compared, set where the piece differs: PC
    /// memory, mapped (buffer, mapping, bytes).
    differs: (ID3D12Resource, usize, u64),
    /// Where the pieces that differ are fetched to: PC memory, mapped.
    fetched: Option<(ID3D12Resource, usize, u64)>,
    /// With [`relay_snapshot`]: where the compare copies the pieces it
    /// finds differing (PC memory, mapped: buffer, mapping, slots). Dword 0
    /// is the compares' count; the limit `areas` keeps is at
    /// [`CARD_RING_LIMIT`].
    ring: Option<(ID3D12Resource, usize, u32)>,
    /// What plans hold of `differs` and of the ring's slots.
    areas: CardAreas,
    /// Whether a plan looks never done with.
    overdue: CardOverdue,
    /// [`CARD_PATCH`], if it compiled: root signature and pipeline. (The
    /// buffers of a plan's patches are in [`PATCH_POOL`] while no plan
    /// holds them.)
    patch: Option<(ID3D12RootSignature, ID3D12PipelineState)>,
}

// SAFETY: D3D12 objects (free-threaded) and persistent mappings, used only
// under the submission lock.
unsafe impl Send for CardWork {}

static CARD_WORK: Mutex<Option<CardWork>> = Mutex::new(None);

impl CardWork {
    /// [`Host::card_done`]: the plan's lists are free for the next plan,
    /// and its run of the results and the ring's slots its compare took go
    /// back ([`card_run_done`]).
    fn plan_done(&mut self, plan: CardPlan) {
        // `CardPlan::take_run` gives a plan of the ring its run with the
        // count's dword; without it the count would be refused, and the
        // ring's slots would stop.
        debug_assert!(
            !plan.in_ring || plan.run.is_none() || plan.count_at.is_some(),
            "a plan of the card compare's ring holds a run of the results without the count's dword"
        );
        for (check, list) in plan.lists {
            if check { self.check.push(list) } else { self.sync.push(list) }
        }
        // SAFETY: the mappings of the results and of the ring; the plan's
        // compare has run, and its run is given to no other plan before
        // this one gives it back.
        unsafe {
            card_run_done(
                &mut self.areas,
                self.differs.1 as *const u32,
                self.ring.as_ref().map(|(_, data, _)| *data as *mut u8),
                plan.run,
                plan.count_at,
            )
        };
    }
}

/// Whether an eighth or less of the compare's results of `work`
/// ([`CARD_WORK`]) is free (`Host::card_results_low`).
fn card_results_low_of(work: &Mutex<Option<CardWork>>) -> bool {
    work.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|work| work.areas.low())
}

/// With [`relay_exact`], whether the wait of a submission that found the
/// compare's results of `work` ([`CARD_WORK`]) low is over, as `line`
/// stands ([`results_wait_over`] with them as they are now). Called under
/// the line's lock: it takes `work`'s, and nothing takes the line's under
/// that.
fn room_wait_over(work: &Mutex<Option<CardWork>>, line: &Line) -> bool {
    results_wait_over(card_results_low_of(work), line)
}

/// A plan's run of the compare's results ([`CardAreas`]), as it gives it
/// back: where it starts and ends, in dwords counted since the area was
/// made. A run that would pass the area's end starts again at its first
/// dword and holds the end it skips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResultRun {
    from: u64,
    to: u64,
}

/// A run taken ([`CardAreas`]): its plan's compared pieces, whether its
/// compare takes counts of the ring, whether the plan is done with, and
/// what its compare left.
#[derive(Debug)]
struct Taken {
    run: ResultRun,
    pieces: u64,
    ring: bool,
    done: bool,
    left: Option<Left>,
}

/// What a plan's compare left in its run with the ring ([`card_run_done`]):
/// the ring's count after its compares, and how many of its pieces it
/// flagged. Each piece it flags takes one count, with a slot or past the
/// limit, and no other piece does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Left {
    pub(crate) count: u32,
    pub(crate) flagged: u32,
}

/// The host's side of the compare's two areas between plans ([`CardWork`]):
/// the dwords of its results, a run per plan (its compared ranges, and with
/// the ring one dword more for the ring's count after its compare), and,
/// with the ring of pieces ([`relay_snapshot`]), how far the ring's count
/// was merged.
///
/// Both go round, so neither waits for every plan to be done with (with
/// the relay plans are made while earlier ones are in the air). A plan
/// takes its run where the last one taken ends, and the run is free again
/// once the plan and every plan before it are done with (`Host::card_done`
/// does them in the queue's order). The ring's count (dword 0, which the
/// compares raise once for each piece they find differing) is never set
/// back: a piece gets slot `count % slots` only below the limit the host
/// keeps at [`CARD_RING_LIMIT`], `slots` past the count left by the compare
/// of the last plan done with, so no slot is taken again before the landing
/// that merges it is over. Short of 2^32 the limit stops at the last
/// multiple of `slots` until the count has gone round, so that
/// `count % slots` goes on from where it was.
///
/// What that rests on, beside the queue's order: the count a plan's
/// compare leaves after its compares ([`record_card_compare`]) holds no
/// count a later plan's compare took. A plan's compare goes in its own
/// submission's `ExecuteCommandLists` call, and the boundaries between
/// such calls on one queue are full barriers: a later plan's compares
/// start only after the count was left. Were they to overlap, the count
/// could hold slots they took, and the limit would pass those slots before
/// their landing merged them: other pieces' bytes in title memory, not
/// only a count off. And no compare takes a count 2^31 or more past the
/// limit it reads, which is at least the one written before its plan was
/// made: past the count merged then, the count rises at most once per piece
/// of the plans whose runs were out then (its own too), each piece with a
/// dword of the results (2^18 in all). An older limit, read while the host
/// writes a newer one, only gives fewer slots.
///
/// So the count a plan of the ring left is taken only for what it must be
/// ([`CardAreas::done`], plans in the queue's order): the count taken
/// before it (0 before the first) and one more for each piece its compare
/// flagged, which is between 0 and its pieces. Anything else, a dword read
/// in another place (a flag, another plan's count) or a count that holds
/// counts a later plan's compares took, which is how a boundary that is no
/// full barrier would show on a real card, is refused: the limit stays,
/// the ring's slots run out, and the landings fetch the pieces (the
/// correct way, and the slower); [`card_run_done`] says so at the 1st,
/// 2nd, 4th... refusal. Once a plan that flagged pieces was refused the
/// counts after it are refused too: the slots stay stopped.
#[derive(Debug)]
pub(crate) struct CardAreas {
    /// Dwords of the results.
    size: u64,
    /// Where the next run starts (`% size`: its dword).
    head: u64,
    /// The runs taken and not free yet, in the order taken.
    runs: std::collections::VecDeque<Taken>,
    /// With the ring: its slots, and the count every slot below which was
    /// merged.
    ring: Option<(u32, u32)>,
    /// Counts of the ring refused.
    refused: u64,
}

impl CardAreas {
    pub(crate) fn new(size: u64, slots: Option<u32>) -> Self {
        CardAreas {
            size: size.max(1),
            head: 0,
            runs: std::collections::VecDeque::new(),
            ring: slots.map(|slots| (slots.max(1), 0)),
            refused: 0,
        }
    }

    /// A run for a plan's `pieces` compared pieces, with one dword more for
    /// the ring's count after its compare if `ring`: its first dword in the
    /// area, and the run. `None` without room beside the runs taken: the
    /// plan compares nothing.
    pub(crate) fn take(&mut self, pieces: u64, ring: bool) -> Option<(u32, ResultRun)> {
        let n = pieces + u64::from(ring);
        if self.runs.is_empty() {
            // Nothing is out: from the first dword again.
            self.head = self.head.next_multiple_of(self.size);
        }
        // A run never passes the end: it starts again at the first dword.
        let start = if self.head % self.size + n > self.size {
            self.head.next_multiple_of(self.size)
        } else {
            self.head
        };
        let oldest = self.runs.front().map_or(self.head, |t| t.run.from);
        if n == 0 || start + n - oldest > self.size {
            return None;
        }
        let run = ResultRun { from: self.head, to: start + n };
        self.runs.push_back(Taken {
            run,
            pieces,
            ring,
            done: false,
            left: None,
        });
        self.head = run.to;
        Some(((start % self.size) as u32, run))
    }

    /// [`CardAreas::take`] for a plan whose compared ranges hold `pieces`
    /// pieces each: their results one after the other from the run's first
    /// dword, in the order given, and with the ring the count after them,
    /// in the run's last dword.
    pub(crate) fn lay_out(&mut self, pieces: &[u32], ring: bool) -> Option<RunLayout> {
        let (first, run) = self.take(pieces.iter().map(|&p| u64::from(p)).sum(), ring)?;
        let mut next = first;
        let ats = pieces
            .iter()
            .map(|&p| {
                let at = next;
                next += p;
                at
            })
            .collect();
        Some(RunLayout {
            ats,
            count_at: ring.then_some(next),
            run,
        })
    }

    /// The plan holding `run` is done with; `left` is what its compare left
    /// in the run with the ring ([`card_run_done`]). Its run is free once
    /// the runs before it are; the ring's limit for the compares, if that
    /// moved it. Plans of the ring are passed in the order taken, each once
    /// every plan before it is: its count is the one merged from then on
    /// only if it is the count merged so far and one for each piece its
    /// compare flagged; else, or with nothing left (no dword for the
    /// count), it is refused and the count merged stays.
    pub(crate) fn done(&mut self, run: ResultRun, left: Option<Left>) -> Option<u32> {
        if let Some(t) = self.runs.iter_mut().find(|t| t.run == run) {
            t.done = true;
            t.left = left;
        }
        let mut moved = false;
        while self.runs.front().is_some_and(|t| t.done) {
            let Some(t) = self.runs.pop_front() else {
                break;
            };
            let Some((slots, merged)) = self.ring else {
                continue;
            };
            if !t.ring {
                continue;
            }
            match t.left {
                // A count per piece flagged, no fewer and no more: no other
                // value is the count the plan's compare left after the
                // plan before it.
                Some(left) if u64::from(left.flagged) <= t.pieces && left.count.wrapping_sub(merged) == left.flagged => {
                    moved |= left.count != merged;
                    self.ring = Some((slots, left.count));
                }
                _ => self.refused += 1,
            }
        }
        if moved { self.limit() } else { None }
    }

    /// With the ring, where the compare of the plan holding `run` leaves
    /// the count: the run's last dword ([`CardAreas::lay_out`]), and the
    /// dwords before it, one per piece, that hold its pieces' flags. `None`
    /// for a run not taken, or not for the ring.
    pub(crate) fn count_place(&self, run: ResultRun) -> Option<(u32, std::ops::Range<u32>)> {
        let t = self.runs.iter().find(|t| t.run == run && t.ring)?;
        // A run never passes the area's end ([`CardAreas::take`]).
        let at = (t.run.to - 1) % self.size;
        Some((at as u32, (at - t.pieces) as u32..at as u32))
    }

    /// Counts of the ring refused so far ([`CardAreas::done`]).
    pub(crate) fn refused(&self) -> u64 {
        self.refused
    }

    /// With the ring: the count every slot below which was merged.
    pub(crate) fn merged(&self) -> Option<u32> {
        self.ring.map(|(_, merged)| merged)
    }

    /// Tests: as if plans of the ring had taken counts up to `count`, all
    /// merged (nothing out).
    #[cfg(test)]
    pub(crate) fn jump(&mut self, count: u32) {
        assert!(self.runs.is_empty(), "a jump with runs out");
        if let Some(ring) = self.ring.as_mut() {
            ring.1 = count;
        }
    }

    /// The ring's limit ([`CARD_RING_LIMIT`]).
    pub(crate) fn limit(&self) -> Option<u32> {
        let (slots, merged) = self.ring?;
        let round = u32::MAX / slots * slots;
        Some(if merged < round {
            (u64::from(merged) + u64::from(slots)).min(u64::from(round)) as u32
        } else {
            round
        })
    }

    /// Dwords of the results the next run may take, the end it skips
    /// counted.
    pub(crate) fn free(&self) -> u64 {
        self.size - self.runs.front().map_or(0, |t| self.head - t.run.from)
    }

    /// Whether an eighth or less of the results is free (`Host::card_results_low`).
    pub(crate) fn low(&self) -> bool {
        self.free() * 8 <= self.size
    }

    /// Runs taken and not free yet ([`CardOverdue`]).
    pub(crate) fn out(&self) -> usize {
        self.runs.len()
    }
}

/// Where a plan's compares leave their results in its run
/// ([`CardAreas::lay_out`]).
#[derive(Debug)]
pub(crate) struct RunLayout {
    /// Each compared range's first dword, in the order given.
    pub(crate) ats: Vec<u32>,
    /// With the ring: the run's last dword, where the count is left after
    /// the compares ([`record_card_compare`]) and read by the host
    /// ([`card_run_done`]).
    pub(crate) count_at: Option<u32>,
    pub(crate) run: ResultRun,
}

/// What [`Host::card_done`] does with a plan's run of the compare's
/// results ([`CardAreas`]) and with the ring of pieces: with the ring, the
/// count the plan's compare left in the run's last dword (`count_at`) is
/// read from `results`, and the pieces it flagged in the dwords before it;
/// the run is given back, and the ring's limit is written where the
/// compares read it ([`set_card_limit`]). At every plan done with, moved or
/// not, so that a write lost on the way is put right by the next. A
/// `count_at` that is not the run's last dword is read nowhere: that plan's
/// count is refused, as is one off what its flags say
/// ([`CardAreas::done`]); refusals are said at the 1st, 2nd, 4th... time.
///
/// # Safety
/// `results` maps the compare's results and `ring` the ring's first slot's
/// worth of bytes (`None`: no ring). The plan's compare has run, and its
/// run's dwords were given to no other plan.
pub(crate) unsafe fn card_run_done(
    areas: &mut CardAreas,
    results: *const u32,
    ring: Option<*mut u8>,
    run: Option<ResultRun>,
    count_at: Option<u32>,
) {
    if let Some(run) = run {
        let left = count_at
            .zip(areas.count_place(run))
            .filter(|(at, (place, _))| at == place)
            .map(|(at, (_, flags))| {
                // SAFETY: the caller's: the run's dwords, which its compare
                // wrote.
                let read = |d: u32| unsafe { results.add(d as usize).read_volatile() };
                Left {
                    count: read(at),
                    flagged: flags.filter(|&d| read(d) != 0).count() as u32,
                }
            });
        let (before, merged) = (areas.refused(), areas.merged());
        areas.done(run, left);
        let refused = areas.refused();
        if (before + 1..=refused).any(u64::is_power_of_two) {
            tracing::warn!(
                refused,
                ?left,
                ?merged,
                "the card compare's ring: the count a plan's compare left is not the count before it and one for each \
                 piece it flagged (read in another place, or holding counts a later submission's compares took): \
                 the ring's limit stays, its slots run out, and the landings fetch; said at the 1st, 2nd, 4th... time"
            );
        }
    }
    if let (Some(ring), Some(limit)) = (ring, areas.limit()) {
        // SAFETY: the caller's.
        unsafe { set_card_limit(ring, limit) };
    }
}

/// Writes the ring's limit for the compares ([`CARD_RING_LIMIT`]). They
/// may be reading it meanwhile: one that finds the older limit gets fewer
/// slots. Released after the merges that read the slots it gives back.
///
/// # Safety
/// `ring` maps the ring's first slot's worth of bytes.
pub(crate) unsafe fn set_card_limit(ring: *mut u8, limit: u32) {
    // SAFETY: the caller's; aligned, as the mapping is and the place is a
    // multiple of 4.
    unsafe { (*ring.add(CARD_RING_LIMIT).cast::<AtomicU32>()).store(limit, Ordering::Release) };
}

/// A plan never done with ([`Host::card_done`]) would show only as waits:
/// its run, and every run taken after it, stays out ([`CardAreas`]), the
/// results stay low, and every submission sent ahead first waits for
/// flights to land (`Host::card_results_low`, counted as lists in the
/// air). As each plan takes its run, [`Host::card_before`] asks whether
/// runs are out past [`OVERDUE_RUNS`] times [`late_most`] (the plans in the
/// air are fewer than their lists) or, with the relay, the results were
/// low for [`OVERDUE_LOW`] plans in a row (each found them low even after
/// its submission waited for room; without the relay nothing waits for
/// room, and results low for a while say nothing), and says so at the 1st,
/// 2nd, 4th... time of each time the areas look so.
#[derive(Debug, Default)]
struct CardOverdue {
    /// Plans in a row that found the results low, with the relay.
    low: u32,
    /// Plans in a row that found the areas so.
    seen: u64,
}

/// Runs out past this many times [`late_most`] look held by a plan never
/// done with ([`CardOverdue`]).
const OVERDUE_RUNS: usize = 4;
/// As do the results found low by this many plans in a row.
const OVERDUE_LOW: u32 = 64;

impl CardOverdue {
    /// A plan takes its run: whether the results are low, whether the
    /// relay is on ([`relay`]: only then did its submission wait for room
    /// first), the runs out ([`CardAreas::out`]) and the most host lists in
    /// the air. `Some`: how many plans in a row found the areas so, when
    /// that is to be said. Once they do not, the next time is said from the
    /// first again.
    fn look(&mut self, low: bool, relay: bool, out: usize, most: usize) -> Option<u64> {
        self.low = if low && relay { self.low.saturating_add(1) } else { 0 };
        if out <= OVERDUE_RUNS.saturating_mul(most) && self.low < OVERDUE_LOW {
            self.seen = 0;
            return None;
        }
        self.seen += 1;
        self.seen.is_power_of_two().then_some(self.seen)
    }
}

#[cfg(test)]
mod card_area_tests {
    use super::{
        CARD_COMPARE, CARD_RING_LIMIT, CARD_SLOT_BYTES, CardAreas, CardOverdue, Late, Left, Line, OVERDUE_LOW, RunLayout,
        card_run_done, results_wait_over, set_card_limit,
    };

    /// What a plan's compare left: the count, and the pieces it flagged.
    fn left(count: u32, flagged: u32) -> Option<Left> {
        Some(Left { count, flagged })
    }

    /// Runs follow one another and never pass the area's end: one that
    /// would starts again at the first dword, and holds the end it skips
    /// until it is free.
    #[test]
    fn runs_go_round_the_results() {
        let mut areas = CardAreas::new(100, None);
        let (a_at, a) = areas.take(40, false).unwrap();
        let (b_at, b) = areas.take(40, false).unwrap();
        assert_eq!((a_at, b_at), (0, 40), "runs follow one another");
        assert_eq!(areas.free(), 20);
        // 25 would pass the end at 80, and the start is a's.
        assert!(areas.take(25, false).is_none(), "no room before a is free");
        areas.done(a, None);
        assert_eq!(areas.free(), 60);
        // It starts again at 0, and holds 80..100 with it.
        let (c_at, c) = areas.take(25, false).unwrap();
        assert_eq!(c_at, 0, "a run that would pass the end starts again at the first dword");
        assert_eq!(areas.free(), 15);
        let (d_at, d) = areas.take(15, false).unwrap();
        assert_eq!(d_at, 25, "and the next follows it");
        assert_eq!(areas.free(), 0);
        assert!(areas.take(1, false).is_none());
        areas.done(b, None);
        assert_eq!(areas.free(), 40, "b's dwords are free, the end c skipped is not");
        assert!(areas.take(41, false).is_none(), "40..81 would reach into the end c holds");
        areas.done(c, None);
        assert_eq!(areas.free(), 85, "with c, the end it skipped is free");
        let (e_at, e) = areas.take(41, false).unwrap();
        assert_eq!(e_at, 40);
        areas.done(d, None);
        areas.done(e, None);
        assert_eq!(areas.free(), 100);
        // Nothing out: from the first dword again, whatever fits.
        assert_eq!(areas.take(100, false).map(|r| r.0), Some(0));
        assert!(areas.take(1, false).is_none());
    }

    /// A run is free once the plan and every plan before it are done with,
    /// whichever is done with first; a run bigger than the area, or of no
    /// dwords, is refused.
    #[test]
    fn a_run_is_free_once_the_runs_before_it_are() {
        let mut areas = CardAreas::new(64, None);
        let (_, a) = areas.take(16, false).unwrap();
        let (_, b) = areas.take(16, false).unwrap();
        let (_, c) = areas.take(16, false).unwrap();
        assert_eq!(areas.free(), 16);
        areas.done(b, None);
        assert_eq!(areas.free(), 16, "b is free only after a");
        areas.done(c, None);
        assert_eq!(areas.free(), 16);
        areas.done(a, None);
        assert_eq!(areas.free(), 64, "a frees b and c behind it");
        assert!(areas.take(65, false).is_none());
        assert!(areas.take(0, false).is_none());
        // The ring's dword: one more.
        let (at, e) = areas.take(10, true).unwrap();
        let (next, _) = areas.take(1, false).unwrap();
        assert_eq!(next - at, 11);
        areas.done(e, None);
        assert_eq!(areas.free(), 63, "e is free before the run after it");
        // A run given back twice, or one long free, frees nothing more.
        areas.done(e, None);
        areas.done(a, None);
        assert_eq!(areas.free(), 63);
    }

    /// The ring's limit moves with the count the compare of the last plan
    /// done with left, in the queue's order: a later plan done with first
    /// does not move it past the slots of an earlier one still out.
    #[test]
    fn the_ring_limit_follows_plans_in_order() {
        let mut areas = CardAreas::new(1024, Some(16));
        assert_eq!(areas.limit(), Some(16));
        let (_, a) = areas.take(8, true).unwrap();
        let (_, b) = areas.take(8, true).unwrap();
        let (_, c) = areas.take(8, false).unwrap();
        let (_, d) = areas.take(8, true).unwrap();
        // a's compare flagged five pieces, b's four after them: b left the
        // count at 9, a at 5.
        assert_eq!(areas.done(b, left(9, 4)), None, "a holds slots below 5");
        assert_eq!(areas.limit(), Some(16));
        assert_eq!(areas.done(a, left(5, 5)), Some(9 + 16), "a and b: every slot below 9 was merged");
        // A plan not in the ring moves nothing; d's count moves it on.
        assert_eq!(areas.done(c, None), None);
        assert_eq!(areas.done(d, left(17, 8)), Some(33));
        // A plan that flagged nothing leaves the count as it was.
        let (_, e) = areas.take(1, true).unwrap();
        assert_eq!(areas.done(e, left(17, 0)), None);
        assert_eq!((areas.limit(), areas.refused()), (Some(33), 0));
    }

    /// The count is never set back, and `count % slots` must go on from
    /// where it was when it wraps at 2^32: short of it the limit stops at
    /// the last multiple of the slots, and moves on once the count of a
    /// plan done with has gone round.
    #[test]
    fn the_ring_limit_stops_short_of_the_wrap() {
        // 2^32 - 4 is the last multiple of 7: counts from it on get no slot.
        let slots = 7u32;
        let round = u32::MAX - 3;
        assert_eq!(round % slots, 0);
        let mut areas = CardAreas::new(1024, Some(slots));
        // A plan of eight pieces, `flagged` of them flagged.
        let pass = |areas: &mut CardAreas, flagged: u32| {
            let (_, run) = areas.take(8, true).unwrap();
            let count = areas.merged().unwrap().wrapping_add(flagged);
            areas.done(run, left(count, flagged))
        };
        // As if plans had taken the count that far.
        areas.jump(1 << 31);
        assert_eq!(pass(&mut areas, 5), Some((1 << 31) + 5 + slots));
        areas.jump(round - 15);
        assert_eq!(pass(&mut areas, 5), Some(round - 3));
        assert_eq!(pass(&mut areas, 3), Some(round));
        assert_eq!(pass(&mut areas, 6), Some(round), "not past the last multiple");
        assert_eq!(pass(&mut areas, 1), Some(round));
        assert_eq!(pass(&mut areas, 2), Some(round), "past it, no slot until the count wraps");
        assert_eq!(pass(&mut areas, 3), Some(1 + slots), "gone round: on from slot 1");
        assert_eq!(areas.refused(), 0);
    }

    /// Only the results ask for flights to land (`Host::card_results_low`):
    /// a ring count past half its slots, with room under the limit, does
    /// not (a piece with no slot is fetched, as before); results with an
    /// eighth or less free do.
    #[test]
    fn only_the_results_ask_flights_to_land() {
        let mut areas = CardAreas::new(800, Some(16));
        let (_, a) = areas.take(12, true).unwrap();
        let (_, b) = areas.take(8, true).unwrap();
        // a's compare took 12 of the 16 slots; b is still in the air.
        let limit = areas.done(a, left(12, 12));
        assert!(!areas.low(), "a ring count past half its slots");
        assert_eq!(limit, Some(28));
        let (_, c) = areas.take(600, true).unwrap();
        assert!(!areas.low());
        let (_, d) = areas.take(90, false).unwrap();
        assert!(areas.low(), "an eighth of the results left");
        assert_eq!(areas.done(b, left(13, 1)), Some(29));
        assert!(!areas.low());
        areas.done(c, left(14, 1));
        areas.done(d, None);
        assert_eq!(areas.free(), 800);
    }

    /// A plan's compared ranges take its run one after the other from its
    /// first dword, in the order given, and with the ring the count goes in
    /// the run's last dword: the next run starts right after it, and a run
    /// that starts again at the area's first dword has its count at its own
    /// end too. The host looks for the count, and for the pieces' flags
    /// before it, where the run was laid out.
    #[test]
    fn a_plan_lays_out_its_run() {
        let mut areas = CardAreas::new(32, Some(4));
        let place = |areas: &CardAreas, laid: &RunLayout| areas.count_place(laid.run);
        let a = areas.lay_out(&[3, 5, 2], true).unwrap();
        assert_eq!((&a.ats[..], a.count_at), (&[0, 3, 8][..], Some(10)), "the count after the ranges");
        assert_eq!(place(&areas, &a), Some((10, 0..10)));
        let b = areas.lay_out(&[4], false).unwrap();
        assert_eq!((&b.ats[..], b.count_at), (&[11][..], None), "the next run right after the count");
        assert_eq!(place(&areas, &b), None, "no count without the ring");
        let c = areas.lay_out(&[6, 6], true).unwrap();
        assert_eq!((&c.ats[..], c.count_at), (&[15, 21][..], Some(27)));
        assert_eq!(place(&areas, &c), Some((27, 15..27)));
        areas.done(a.run, left(1, 1));
        assert_eq!(place(&areas, &a), None, "given back");
        // 28 + 6 would pass the end at 32.
        let d = areas.lay_out(&[5], true).unwrap();
        assert_eq!((&d.ats[..], d.count_at), (&[0][..], Some(5)), "from the first dword again");
        assert_eq!(place(&areas, &d), Some((5, 0..5)));
        // The last five dwords before b's.
        let e = areas.lay_out(&[4], true).unwrap();
        assert_eq!((&e.ats[..], e.count_at), (&[6][..], Some(10)));
        assert_eq!(place(&areas, &e), Some((10, 6..10)));
        assert!(areas.lay_out(&[1], false).is_none());
    }

    /// What `Host::card_done` does with the run and the ring
    /// (`card_run_done`): the count the plan's compare left is read in its
    /// run's last dword, with the flags of its pieces before it, the run is
    /// given back, and the limit is written where the compares read it, at
    /// every plan done with: one lost on the way is written again, moved or
    /// not. The count is the compares' own.
    #[test]
    fn the_host_reads_the_count_and_writes_the_limit() {
        let mut areas = CardAreas::new(64, Some(16));
        // Beside the counts the results hold flags, never a count.
        let mut results = vec![0xDEADu32; 64];
        let mut first_slot = vec![0u32; CARD_SLOT_BYTES / 4];
        let ring = first_slot.as_mut_ptr().cast::<u8>();
        // SAFETY: inside the ring's first slot's worth of bytes.
        let count = || unsafe { ring.cast::<u32>().read() };
        // SAFETY: inside the ring's first slot's worth of bytes.
        let limit = || unsafe { ring.add(CARD_RING_LIMIT).cast::<u32>().read() };
        // SAFETY: inside the ring's first slot's worth of bytes.
        let lose_limit = || unsafe { ring.add(CARD_RING_LIMIT).cast::<u32>().write(3) };
        // SAFETY: inside the ring's first slot's worth of bytes.
        unsafe { set_card_limit(ring, areas.limit().unwrap()) };
        assert_eq!((count(), limit()), (0, 16));
        let a = areas.lay_out(&[3, 2], true).unwrap();
        let b = areas.lay_out(&[4], true).unwrap();
        let c = areas.lay_out(&[5], false).unwrap();
        // What their compares left: a flagged three of its pieces (one past
        // the limit) and left the count at 3, b two more (5).
        let leave = |results: &mut [u32], laid: &RunLayout, flags: &[u32], count: u32| {
            let at = laid.count_at.unwrap() as usize;
            results[at - flags.len()..at].copy_from_slice(flags);
            results[at] = count;
        };
        leave(&mut results, &a, &[2, 0, 1, 0, 3], 3);
        leave(&mut results, &b, &[0, 4, 0, 5], 5);
        let done = |areas: &mut CardAreas, results: &[u32], laid: &RunLayout| {
            // SAFETY: the whole results and ring.
            unsafe { card_run_done(areas, results.as_ptr(), Some(ring), Some(laid.run), laid.count_at) }
        };
        done(&mut areas, &results, &a);
        assert_eq!(limit(), 3 + 16, "a's count, read where its compare left it");
        // c, done with before b, moves nothing.
        lose_limit();
        done(&mut areas, &results, &c);
        assert_eq!(limit(), 3 + 16, "written again, not moved");
        done(&mut areas, &results, &b);
        assert_eq!(limit(), 5 + 16);
        assert_eq!(count(), 0, "the count is the compares' own");
        // Without the ring nothing is written; a plan with no run of the
        // results writes the limit all the same.
        lose_limit();
        // SAFETY: `results` holds the 64 dwords the areas were made for; no ring is given.
        unsafe { card_run_done(&mut areas, results.as_ptr(), None, None, None) };
        assert_eq!(limit(), 3);
        // SAFETY: `results` holds the 64 dwords the areas were made for, `ring` one slot's worth of bytes.
        unsafe { card_run_done(&mut areas, results.as_ptr(), Some(ring), None, None) };
        assert_eq!(limit(), 5 + 16);
        assert_eq!(areas.refused(), 0);
    }

    /// A plan's count is taken only if it is the count before it and one
    /// for each piece its compare flagged: a flag read as the count (0, 1 or
    /// 2 + a slot), another plan's count, the count one off, a count that
    /// holds pieces a later plan's compare flagged, or the count's dword
    /// passed one off (the next run's first dword, or the last flag) is
    /// refused, and the limit stays where it was. (A flag can be the count
    /// itself only where the count is small, at the start: then it is the
    /// count, whichever dword gave it.)
    #[test]
    fn a_count_off_what_the_flags_say_moves_nothing() {
        const SLOTS: u32 = 16;
        // Four pieces after plans that took the count to 100: three
        // flagged, two of them with slots 4 and 5.
        let flags = [2 + 4, 0, 1, 2 + 5];
        let right = 100 + 3;
        // What the host makes of it with `read` in the count's dword and
        // `next` in the one after, told the count's dword is `off` dwords
        // away: the count merged, the limit written and the counts refused.
        let with_next = |read: u32, off: i32, next: u32| {
            let mut areas = CardAreas::new(64, Some(SLOTS));
            areas.jump(100);
            let mut results = vec![0u32; 64];
            let mut first_slot = vec![0u32; CARD_SLOT_BYTES / 4];
            let ring = first_slot.as_mut_ptr().cast::<u8>();
            let laid = areas.lay_out(&[4], true).unwrap();
            let at = laid.count_at.unwrap();
            results[at as usize - 4..at as usize].copy_from_slice(&flags);
            results[at as usize] = read;
            results[at as usize + 1] = next;
            // SAFETY: the whole results and ring.
            unsafe {
                card_run_done(&mut areas, results.as_ptr(), Some(ring), Some(laid.run), Some(at.wrapping_add_signed(off)));
                (areas.merged(), ring.add(CARD_RING_LIMIT).cast::<u32>().read(), areas.refused())
            }
        };
        // A later plan's first flag after it.
        let with = |read: u32, off: i32| with_next(read, off, 2 + 7);
        assert_eq!(with(right, 0), (Some(right), right + SLOTS, 0), "the count before and one per piece flagged");
        let refused = (Some(100), 100 + SLOTS, 1);
        for flag in [0, 1, 2, 2 + 4, 2 + SLOTS - 1] {
            assert_eq!(with(flag, 0), refused, "a flag {flag} read as the count");
        }
        assert_eq!(with(100, 0), refused, "the count of the plan before");
        assert_eq!(with(93, 0), refused, "an older plan's count");
        assert_eq!(with(right + 1, 0), refused, "one more");
        assert_eq!(with(right - 1, 0), refused, "one fewer");
        assert_eq!(with(right + 2, 0), refused, "with pieces a later plan's compare flagged");
        assert_eq!(with(right + 4, 0), refused, "past the plan's own pieces");
        assert_eq!(with(right, 1), refused, "the dword after the count's: read nowhere");
        assert_eq!(with_next(0, 1, right), refused, "not even where it holds the count's value");
        assert_eq!(with(right, -1), refused, "the dword before it");
    }

    /// Once a plan whose compare flagged pieces had its count refused, the
    /// counts after it are refused too: the count merged stays below the
    /// counts that plan took, the limit stays, and the ring's slots run out
    /// (the landings fetch). A plan refused that flagged nothing took no
    /// count: the next one goes on. A plan of the ring given back with
    /// nothing left (no dword for the count) is refused, as is one said to
    /// have flagged more pieces than it compared; one not of the ring has
    /// nothing to refuse.
    #[test]
    fn the_ring_stops_after_a_count_refused() {
        let mut areas = CardAreas::new(256, Some(16));
        let (_, a) = areas.take(4, true).unwrap();
        let (_, b) = areas.take(4, true).unwrap();
        // a flagged nothing and its count was read wrong; b flagged two.
        assert_eq!(areas.done(a, left(7, 0)), None);
        assert_eq!(areas.done(b, left(2, 2)), Some(2 + 16));
        assert_eq!(areas.refused(), 1);
        // c flagged three, and its count holds a piece of a later plan.
        let (_, c) = areas.take(4, true).unwrap();
        let (_, d) = areas.take(4, true).unwrap();
        let (_, e) = areas.take(4, false).unwrap();
        let (_, f) = areas.take(4, true).unwrap();
        // d, done with before c, waits for it: its count 7 is 5 + its 2.
        assert_eq!(areas.done(d, left(7, 2)), None);
        assert_eq!(areas.done(c, left(6, 3)), None, "c refused, and with it d");
        assert_eq!((areas.merged(), areas.limit(), areas.refused()), (Some(2), Some(18), 3));
        assert_eq!(areas.done(e, None), None);
        assert_eq!(areas.done(f, left(8, 1)), None);
        assert_eq!(areas.refused(), 4);
        // A plan of the ring that left nothing.
        let (_, g) = areas.take(4, true).unwrap();
        assert_eq!(areas.done(g, None), None);
        assert_eq!((areas.merged(), areas.refused()), (Some(2), 5));
        // No plan flags more pieces than it compared.
        let mut areas = CardAreas::new(256, Some(16));
        let (_, h) = areas.take(4, true).unwrap();
        assert_eq!(areas.done(h, left(5, 5)), None);
        assert_eq!((areas.merged(), areas.refused()), (Some(0), 1));
    }

    /// The compare reads the limit where the host writes it, and cuts the
    /// ring into slots as the host reads them.
    #[test]
    fn the_compare_and_the_host_agree_on_the_ring() {
        assert!(CARD_COMPARE.contains(&format!("static const uint limitAt = {CARD_RING_LIMIT}u;")));
        assert!(CARD_COMPARE.contains("ring.Load(limitAt)"));
        assert!(CARD_COMPARE.contains(&format!("static const uint slotBytes = {CARD_SLOT_BYTES}u;")));
    }

    /// Plans never done with are said once the areas look so, then at the
    /// 2nd, 4th, 8th... plan in a row, and from the first again each time
    /// they look so anew: runs out past a few times the lists in the air,
    /// or with the relay the results low for many plans in a row. Results
    /// low now and then, with runs as many as the lists in the air allow,
    /// say nothing; nor do results low for long without the relay, where
    /// no submission waits for room.
    #[test]
    fn plans_never_done_with_are_said() {
        let mut overdue = CardOverdue::default();
        for i in 0..1000 {
            assert_eq!(overdue.look(i % 50 == 0, true, 128, 128), None);
        }
        for _ in 1..OVERDUE_LOW {
            assert_eq!(overdue.look(true, true, 10, 128), None);
        }
        assert_eq!(overdue.look(false, true, 10, 128), None, "room again: counted from the start");
        let said: Vec<(u32, u64)> = (1..=300u32).filter_map(|n| overdue.look(true, true, 10, 128).map(|times| (n, times))).collect();
        assert_eq!(said[0], (OVERDUE_LOW, 1), "said at the plan that makes it many in a row");
        assert_eq!(said.iter().map(|s| s.1).collect::<Vec<u64>>(), [1, 2, 4, 8, 16, 32, 64, 128]);
        // Room again, then low as long again: said from the first time.
        assert_eq!(overdue.look(false, true, 10, 128), None);
        let again: Vec<u64> = (0..=OVERDUE_LOW).filter_map(|_| overdue.look(true, true, 10, 128)).collect();
        assert_eq!(again, [1, 2], "a new time, said from its first plan");
        // Without the relay.
        let mut overdue = CardOverdue::default();
        assert!((0..1000).all(|_| overdue.look(true, false, 10, 128).is_none()), "nothing waited for room");
        // Runs out: with the relay or without.
        let mut overdue = CardOverdue::default();
        assert_eq!(overdue.look(false, false, 4 * 128, 128), None);
        assert_eq!(overdue.look(false, false, 4 * 128 + 1, 128), Some(1), "runs out past four times the lists in the air");
        assert_eq!(overdue.look(false, true, 4 * 128 + 2, 128), Some(2));
        assert_eq!(overdue.look(false, false, 4 * 128 + 3, 128), None);
        assert_eq!(overdue.look(false, false, 4 * 128, 128), None, "back within");
        assert_eq!(overdue.look(false, false, 4 * 128 + 1, 128), Some(1), "out again: said from the first time");
    }

    /// With the relay's second stage a submission that found the results
    /// low waits until they are not, or until nothing is in the air or
    /// being landed, as a settle would: a plan never done with keeps them
    /// low, and must not keep the submission waiting for good.
    #[test]
    fn a_wait_for_room_ends_with_nothing_in_the_air() {
        let mut line = Line {
            line: Vec::new(),
            written: Vec::new(),
            lists: 0,
            target: 0,
            settling: false,
            poke: false,
            landed: Vec::new(),
            in_hand: Vec::new(),
        };
        assert!(results_wait_over(true, &line), "nothing in the air");
        line.settling = true;
        assert!(!results_wait_over(true, &line), "flights being landed");
        assert!(results_wait_over(false, &line), "room again");
        line.settling = false;
        line.line.push(Late::Signal(0, 1));
        assert!(!results_wait_over(true, &line), "flights in the air");
        assert!(results_wait_over(false, &line));
    }
}

/// The host's own code around the compare's areas, on the host device
/// (`host()`): the work made as `Host::card_ready` makes it, with a ring
/// (`Host::make_card_work`); a plan's run taken as `Host::card_before`
/// takes it (`CardPlan::take_run`) and given back as `Host::card_done`
/// gives it (`CardWork::plan_done`); and the wait of a submission for room
/// in the results (`room_wait_over`). What the compares leave is written
/// here where they leave it (the compare itself on the card:
/// `crate::indirect`'s `warp_card_compare_*`).
#[cfg(test)]
mod card_work_tests {
    use super::{CARD_RING_LIMIT, CardPlan, Late, Line, card_results_low_of, room_wait_over};

    fn plan(in_ring: bool) -> CardPlan {
        CardPlan {
            ranges: Vec::new(),
            check: None,
            _synced: Vec::new(),
            lists: Vec::new(),
            sent_as: 0,
            in_ring,
            run: None,
            count_at: None,
            patches: None,
        }
    }

    /// A plan of the ring keeps its run's last dword for the count; given
    /// back, the count is read there, with the flags before it, and the
    /// limit is written in the ring where the compares read it. A plan not
    /// of the ring keeps no count's dword; a count off what the flags say
    /// moves nothing; and a plan of the ring holding a run without the
    /// count's dword is a bug (a debug assertion), its count refused.
    #[test]
    fn the_host_gives_a_plans_run_back_through_its_ring() {
        let Some(host) = super::host() else {
            return;
        };
        let mut work = host.make_card_work(Some(16)).expect("the compare's work, with a ring");
        let (ring, slots) = work.ring.as_ref().map(|r| (r.1 as *mut u8, r.2)).expect("a ring of 16 MB");
        let results = work.differs.1 as *mut u32;
        // SAFETY: inside the ring's first slot's worth of bytes and
        // the results, both mapped while `work` lives.
        let count = || unsafe { ring.cast::<u32>().read_volatile() };
        // SAFETY: inside the ring's first slot's worth of bytes and the results, both mapped while `work` lives.
        let limit = || unsafe { ring.add(CARD_RING_LIMIT).cast::<u32>().read_volatile() };
        // SAFETY: inside the ring's first slot's worth of bytes and the results, both mapped while `work` lives.
        let put = |d: u32, value: u32| unsafe { results.add(d as usize).write_volatile(value) };
        assert_eq!((count(), limit()), (0, slots), "made with no count, and the limit at the slots");
        // Two ranges, of 3 and 2 pieces: their compares flagged three, the
        // count went from 0 to 3.
        let mut a = plan(true);
        assert!(a.take_run(&mut work.areas, &[3, 2]));
        let at = a.count_at.expect("in the ring, the count's dword");
        for (d, flag) in (at - 5..at).zip([2, 0, 1, 0, 3]) {
            put(d, flag);
        }
        put(at, 3);
        // The next run's first dword, as a later plan's compare may leave it.
        put(at + 1, 2 + 9);
        work.plan_done(a);
        assert_eq!((work.areas.merged(), limit()), (Some(3), 3 + slots), "the count read where the compare left it");
        // Not of the ring.
        let mut b = plan(false);
        assert!(b.take_run(&mut work.areas, &[4]));
        assert_eq!(b.count_at, None);
        work.plan_done(b);
        assert_eq!(limit(), 3 + slots);
        // A count one past what its flags say.
        let mut c = plan(true);
        assert!(c.take_run(&mut work.areas, &[2]));
        let at = c.count_at.unwrap();
        put(at - 2, 2 + 3);
        put(at - 1, 0);
        put(at, 5);
        work.plan_done(c);
        assert_eq!((work.areas.merged(), limit(), work.areas.refused()), (Some(3), 3 + slots, 1));
        assert_eq!(count(), 0, "the count is the compares' own");
        // No count's dword.
        let mut d = plan(true);
        assert!(d.take_run(&mut work.areas, &[1]));
        d.count_at = None;
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.plan_done(d)));
        assert_eq!(done.is_err(), cfg!(debug_assertions), "a debug assertion");
        if done.is_ok() {
            assert_eq!((work.areas.merged(), work.areas.refused()), (Some(3), 2));
        }
    }

    /// `CardPlan::take_run` over ranges: each compared range gets its first
    /// dword in the plan's run, in order; one not compared keeps its own;
    /// without room none is compared and the plan has no run.
    #[test]
    fn take_run_lays_out_each_compared_range() {
        let Some(host) = super::host() else {
            return;
        };
        let (buffer, data) = host.shared_buffer(65536).expect("a buffer");
        let shadow = std::sync::Arc::new(crate::shadow::Shadow::for_test(buffer, data, 0x10000, 0x10000));
        let range = |pieces: u32, compared: bool| super::CardRange {
            shadow: shadow.clone(),
            declared: (0, 0),
            span: crate::shadow::CardSpan { from: 0, to: 0, first: 0, pieces, skew: 0 },
            at: 7777,
            compared,
        };
        let mut areas = super::CardAreas::new(64, Some(16));
        // A run out first, so that the plan's does not start at dword 0.
        let _first = areas.take(5, false).unwrap();
        let mut p = plan(true);
        p.ranges = vec![range(3, true), range(4, false), range(2, true)];
        assert!(p.take_run(&mut areas, &[3, 2]));
        let ats: Vec<(bool, u32)> = p.ranges.iter().map(|r| (r.compared, r.at)).collect();
        assert_eq!(ats, [(true, 5), (false, 7777), (true, 8)]);
        assert_eq!(p.count_at, Some(10));
        let mut q = plan(true);
        q.ranges = vec![range(60, true), range(1, true)];
        assert!(!q.take_run(&mut areas, &[60, 1]), "no room");
        assert!(q.ranges.iter().all(|r| !r.compared));
        assert_eq!((q.run, q.count_at), (None, None));
    }

    /// The wait of a submission that found the compare's results low
    /// (`VICTORY_RELAY=2`) looks at the host's own results: it goes on
    /// while they are low with flights in the air, and is over with room
    /// again, or with nothing in the air.
    #[test]
    fn a_wait_for_room_looks_at_the_card_compares_results() {
        let Some(host) = super::host() else {
            return;
        };
        let work = std::sync::Mutex::new(Some(host.make_card_work(None).expect("the compare's work")));
        let mut line = Line {
            line: vec![Late::Signal(0, 1)],
            written: Vec::new(),
            lists: 1,
            target: 0,
            settling: false,
            poke: false,
            landed: Vec::new(),
            in_hand: Vec::new(),
        };
        assert!(room_wait_over(&work, &line), "room");
        // Plans in the air hold seven eighths of the results.
        let held = {
            let mut guard = work.lock().unwrap();
            let w = guard.as_mut().unwrap();
            let dwords = w.differs.2 / 4;
            w.areas.take(dwords / 8 * 7, false).expect("room for seven eighths").1
        };
        assert!(card_results_low_of(&work));
        assert!(!room_wait_over(&work, &line), "low, with flights in the air: the wait goes on");
        line.line.clear();
        assert!(room_wait_over(&work, &line), "nothing in the air: over");
        line.line.push(Late::Signal(0, 2));
        work.lock().unwrap().as_mut().unwrap().areas.done(held, None);
        assert!(!card_results_low_of(&work));
        assert!(room_wait_over(&work, &line), "room again");
        assert!(room_wait_over(&std::sync::Mutex::new(None), &line), "no compare: nothing to wait for");
    }
}

/// The compare of a declared range of a copy in the card's memory with
/// its bytes from before, 32 threads per 64 KiB piece (aligned in
/// title memory as `Shadow::card_piece` cuts them): `differs`
/// gets a dword per piece, 1 where a byte differs. The offsets are widened
/// to 16 bytes; buffers are multiples of 64 KiB, and a piece flagged for a
/// byte just outside the range costs a fetch that finds nothing.
pub(crate) const CARD_COMPARE_PIECES_PER_GROUP: u32 = 2;

pub(crate) const CARD_COMPARE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), SRV(t0), UAV(u0), UAV(u1), UAV(u2)"
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes (a multiple of 64 KiB)
    uint pieces;
    uint out_at;   // the first piece's dword in `differs`, in bytes
    uint compare;  // 0: the hashes are stored; 1, 2: compared with those stored (2: and then stored); 3: see `ring`
    uint slots;    // with compare: a piece found differing is copied into `ring` if it gets a slot (0: never)
    uint unused;
};
ByteAddressBuffer now : register(t0);
// 16 bytes per piece of the copy: the hash of its bytes when they were
// last sent up (or hashed whole, the first time).
RWByteAddressBuffer hashes : register(u0);
// Per piece compared: 0 the same; 1 it differs; 2 + s it differs and its
// bytes are in slot s of `ring`.
RWByteAddressBuffer differs : register(u1);
// Dword 0 is a count each piece found differing takes, never set back;
// the host's limit is at limitAt, on a cache line of its own. Below the
// limit (wrapping) the piece gets slot count % slots, which holds its bytes
// (widened to 16 bytes as the hashing reads them) from slotBytes * (s + 1)
// on. With compare 3, after a plan's compares: one thread leaves the count
// at out_at, for the host (no later submission's compare has taken one
// yet: the host's limit rests on that).
RWByteAddressBuffer ring : register(u2);
groupshared uint4 stripes[64];
groupshared uint copyTo[2];
static const uint pieceBytes = 65536u;
static const uint slotBytes = 65792u;
static const uint limitAt = 256u;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint index : SV_GroupIndex, uint3 group : SV_GroupID) {
    if (compare == 3u) {
        if (index == 0u && group.x == 0u) {
            uint count;
            ring.InterlockedAdd(0u, 0u, count);
            differs.Store(out_at, count);
        }
        return;
    }
    // Adjacent lanes read adjacent words in each of two pieces.
    uint localPiece = index / 32u;
    uint lane = index % 32u;
    uint offset = group.x * 2u + localPiece;
    uint piece = first + offset;
    uint4 start = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu);
    uint4 mix = uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
    uint4 h = start ^ (lane * mix);
    uint lo = 0u;
    uint hi = 0u;
    if (offset < pieces) {
        lo = piece * pieceBytes;
        lo = lo > skew ? lo - skew : 0u;
        lo = min(lo, size);
        hi = lo + min(size - lo, piece == 0u ? pieceBytes - skew : pieceBytes);
        lo = lo & ~15u;
        hi = (hi + 15u) & ~15u;
        [loop]
        for (uint at = lo + lane * 16u; at < hi; at += 32u * 16u) {
            h = (h ^ now.Load4(at)) * mix;
            h = h.yzwx ^ (h >> 15);
        }
    }
    stripes[index] = h;
    GroupMemoryBarrierWithGroupSync();
    if (lane == 0u) {
        uint slot = 0xFFFFFFFFu;
        if (offset < pieces) {
            [unroll]
            for (uint n = 1u; n < 32u; ++n) {
                h ^= stripes[index + n];
            }
            if (compare != 0u) {
                uint flag = any(h != hashes.Load4(piece * 16u)) ? 1u : 0u;
                // In the queue's order, before any later submission can
                // write the copy: the piece as this submission left it.
                if (flag != 0u && slots != 0u) {
                    uint taken;
                    ring.InterlockedAdd(0u, 1u, taken);
                    // The host raises the limit as landings merge, maybe
                    // while this runs: an older one only gives fewer slots.
                    // Loaded, not atomic: an atomic writes it back, and on
                    // PC memory that may put an older limit over a newer.
                    uint limit = ring.Load(limitAt);
                    if (asint(limit - taken) > 0) {
                        slot = taken % slots;
                        flag = 2u + slot;
                    }
                }
                differs.Store(out_at + offset * 4u, flag);
                // With the copy-back on its own thread: the next compare of
                // the piece finds what is written from here on.
                if (compare == 2u) hashes.Store4(piece * 16u, h);
            } else {
                hashes.Store4(piece * 16u, h);
            }
        }
        copyTo[localPiece] = slot;
    }
    GroupMemoryBarrierWithGroupSync();
    uint to = copyTo[localPiece];
    if (to == 0xFFFFFFFFu) return;
    uint base = slotBytes * (to + 1u);
    [loop]
    for (uint at = lo + lane * 16u; at < hi; at += 32u * 16u) {
        ring.Store4(base + (at - lo), now.Load4(at));
    }
}
"#;

/// Title bytes put into a copy in the card's memory without a copy per
/// run: a patch per dword of the copy (`crate::shadow::dword_patches`):
/// its index, the title's bytes and the mask of those to put in. The other
/// bytes of the dword stay as the card holds them (a flight's writes).
/// The patches name each dword once, so no two threads share one; the
/// offsets are below 4 GiB ([`card_patchable`]).
///
/// Two atomic steps, not a load and a store: the first clears the mask's
/// bits the title's bytes do not set, the second sets those they do, and
/// neither changes a bit outside the mask. A write of the dword's other
/// bytes by anything else on the card at the same time (none today: one
/// queue runs everything in order) is kept, where a load and a store
/// would put back the bytes as they were before it.
pub(crate) const CARD_PATCH: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), SRV(t0), UAV(u0)"
cbuffer Patches : register(b0) { uint count; };
ByteAddressBuffer patches : register(t0);
RWByteAddressBuffer copy : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= count) {
        return;
    }
    uint3 p = patches.Load3(id.x * 12u);
    uint at = p.x * 4u;
    copy.InterlockedAnd(at, ~p.z | p.y);
    copy.InterlockedOr(at, p.y & p.z);
}
"#;

/// Threads per group of [`CARD_PATCH`].
pub(crate) const CARD_PATCH_PER_GROUP: u32 = 64;

/// Whether the copies in the card's memory take title bytes by
/// [`CARD_PATCH`] (it compiled): then the short runs put in under flights
/// go up as patches, held to their own room (`crate::shadow::Room`).
static CARD_PATCHES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a copy of `size` bytes in the card's memory can take patches
/// (byte offsets of a raw view are 32 bits).
pub(crate) fn card_patchable(size: u64) -> bool {
    CARD_PATCHES.load(Ordering::Relaxed) && size <= 1 << 32
}

/// A run of title bytes shorter than this goes to the card's side as
/// patches, a longer one by a copy of its own.
pub(crate) const CARD_PATCH_RUN: usize = 256;

/// The most runs of title bytes put in under flights that go up by a copy
/// each before one submission (`crate::shadow::Room`): a settle is due past
/// it, as it was for every run before the patches.
pub(crate) const CARD_COPIES_MOST: usize = 512;

/// The most bytes of patches ([`CARD_PATCH`], 12 bytes a dword) the short
/// runs put in under flights take before one submission: about 1.4 million
/// patches, where the bus scene of run dd56178 put in 20-80 thousand runs
/// of about 3 bytes a frame. Also the size of the buffer taken for them
/// before any is put in ([`Host::patch_reserve`]).
pub(crate) const CARD_PATCH_BYTES_MOST: usize = 16 << 20;

/// The least bytes of a plan's buffer of patches.
const PATCH_LEAST: u64 = 64 << 10;

/// The most bytes of buffers of patches kept for later plans
/// ([`PatchPool`]), and the plans after which one no plan took goes.
const PATCH_POOL_MOST: u64 = 64 << 20;
const PATCH_POOL_AGE: u64 = 256;

/// From this many ranges sent up to a copy in the card's memory on, the
/// pieces hashed again after them are hashed in joined runs whatever
/// `VICTORY_CARD_REHASH_RUNS` says ([`rehash_runs`]); with patches always.
const REHASH_JOIN_FROM: usize = 512;

/// Since the last frame line: patches of [`CARD_PATCH`] sent up, the
/// copies they were for, the title bytes they put in; copies of ranges
/// recorded to bring card sides up to date (`CopyBufferRegion`); buffers
/// of patches there was no memory for.
static CARD_PATCHED: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// A buffer in the PC's memory for a plan's patches: the buffer and its
/// mapping.
struct PatchMemory(ID3D12Resource, usize);

// SAFETY: a D3D12 buffer (free-threaded) and its persistent mapping, which
// only the submitting thread writes, while no plan holds the buffer.
unsafe impl Send for PatchMemory {}

/// A buffer for a plan's patches and its bytes.
type PatchBuffer = (PatchMemory, u64);

/// The buffers of patches no plan holds ([`PatchPool`]). Of its own lock:
/// a plan's buffer is chosen outside the card's work and the queue's order,
/// which the relay's thread takes too, and one made for it is made outside
/// any lock.
static PATCH_POOL: Mutex<PatchPool<PatchMemory>> = Mutex::new(PatchPool::new());

/// Buffers for plans' patches that no plan holds, for later plans: each
/// with its bytes and the count of plans when it was given back, in the
/// order they were. A plan takes the smallest that holds its patches and is
/// not more than four times the buffer one made for them would be (their
/// bytes' next power of two: up to about eight times their bytes), so that
/// a few patches do not pin a big buffer while their flight is in the air
/// (each plan held one of 1 MiB or more, and up to 128 lists are in the
/// air). Kept: the ones given back last, at most [`PATCH_POOL_MOST`] bytes
/// of them, and none that no plan took for [`PATCH_POOL_AGE`] plans.
struct PatchPool<T> {
    free: Vec<(T, u64, u64)>,
    plans: u64,
}

impl<T> Default for PatchPool<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> PatchPool<T> {
    const fn new() -> Self {
        PatchPool { free: Vec::new(), plans: 0 }
    }

    /// The buffer that holds a submission's room for patches (`most`
    /// bytes, [`Host::patch_reserve`]): one kept of that size or more, else
    /// one made (`make`, outside the pool's lock).
    fn reserve(pool: &Mutex<Self>, most: u64, make: impl FnOnce(u64) -> Option<T>) -> Option<(T, u64)> {
        let kept = pool.lock().unwrap_or_else(|e| e.into_inner()).take(most, u64::MAX);
        kept.or_else(|| make(most).map(|buffer| (buffer, most)))
    }

    /// The buffer for a plan's `bytes` of patches ([`Host::patch_buffer`]),
    /// chosen before the submission's `reserve` goes back to the pool: the
    /// smallest kept that holds them and is at most four times `want` (the
    /// buffer one made for them would be: their bytes' next power of two,
    /// [`PATCH_LEAST`] at least), else one of `want` made (`make`, outside
    /// the pool's lock); without memory, the reserve if it holds them, else
    /// the biggest of the reserve and those kept (the caller sends the
    /// ranges past its room by copies). The reserve goes back to the pool
    /// after the choice, unless taken.
    ///
    /// Given back first, the reserve (16 MiB) lay in the window of a plan
    /// of 2 to 8 MiB of patches, which took it into the air, and the next
    /// submission made another while it flew: eight such plans in the air
    /// held and made 128 MiB, against 32 held and 48 made this way.
    fn for_plan(pool: &Mutex<Self>, bytes: u64, reserve: Option<(T, u64)>, make: impl FnOnce(u64) -> Option<T>) -> Option<(T, u64)> {
        let want = bytes.next_power_of_two().max(PATCH_LEAST);
        let kept = pool.lock().unwrap_or_else(|e| e.into_inner()).take(bytes, want.saturating_mul(4));
        let chosen = kept.or_else(|| make(want).map(|buffer| (buffer, want)));
        let mut pool = pool.lock().unwrap_or_else(|e| e.into_inner());
        let (chosen, spare) = match (chosen, reserve) {
            (Some(chosen), reserve) => (Some(chosen), reserve),
            (None, Some(reserve)) if reserve.1 >= bytes => (Some(reserve), None),
            (None, reserve) => match (pool.take_biggest(), reserve) {
                (Some(kept), Some(reserve)) if reserve.1 > kept.1 => (Some(reserve), Some(kept)),
                (Some(kept), reserve) => (Some(kept), reserve),
                (None, reserve) => (reserve, None),
            },
        };
        if let Some((buffer, size)) = spare {
            pool.give_back(buffer, size);
        }
        chosen
    }

    /// The smallest buffer kept of `least` bytes or more, and of `most` or
    /// fewer.
    fn take(&mut self, least: u64, most: u64) -> Option<(T, u64)> {
        let i = (0..self.free.len())
            .filter(|&i| (least..=most).contains(&self.free[i].1))
            .min_by_key(|&i| self.free[i].1)?;
        let (buffer, bytes, _) = self.free.remove(i);
        Some((buffer, bytes))
    }

    /// The biggest buffer kept.
    fn take_biggest(&mut self) -> Option<(T, u64)> {
        let i = (0..self.free.len()).max_by_key(|&i| self.free[i].1)?;
        let (buffer, bytes, _) = self.free.remove(i);
        Some((buffer, bytes))
    }

    /// Keeps `buffer` (of `bytes`) for later plans, within the bounds.
    fn give_back(&mut self, buffer: T, bytes: u64) {
        self.free.push((buffer, bytes, self.plans));
        self.trim();
    }

    /// A plan is made: the buffers age.
    fn plan(&mut self) {
        self.plans += 1;
        self.trim();
    }

    fn trim(&mut self) {
        let plans = self.plans;
        self.free.retain(|b| plans - b.2 < PATCH_POOL_AGE);
        // From the one given back last: those that still fit.
        let mut kept = 0u64;
        let mut keep = vec![false; self.free.len()];
        for (i, b) in self.free.iter().enumerate().rev() {
            if kept + b.1 <= PATCH_POOL_MOST {
                kept += b.1;
                keep[i] = true;
            }
        }
        let mut keep = keep.into_iter();
        self.free.retain(|_| keep.next() == Some(true));
    }

    #[cfg(test)]
    fn bytes(&self) -> u64 {
        self.free.iter().map(|b| b.1).sum()
    }
}

/// The bytes of a slot of [`CARD_COMPARE`]'s ring: a piece widened to 16
/// bytes at both ends, as the hashing reads it. The ring's first slot's
/// worth of bytes holds the count (dword 0) and the limit
/// ([`CARD_RING_LIMIT`]) of [`CardAreas`].
pub(crate) const CARD_SLOT_BYTES: usize = 65792;

/// Where in the ring the host keeps its limit for the compares
/// ([`CardAreas`]), in bytes (the shader's `limitAt`): in the first slot's
/// worth of bytes, which no slot takes, and 256 bytes past the count, so on
/// a cache line of its own on the card and on the PC. The compares'
/// atomics on the count neither hold the limit's line nor write it back,
/// and nothing written back with the count can put an older limit over the
/// host's.
pub(crate) const CARD_RING_LIMIT: usize = 256;

const _: () = assert!(CARD_RING_LIMIT >= 256 && CARD_RING_LIMIT.is_multiple_of(4) && CARD_RING_LIMIT + 4 <= CARD_SLOT_BYTES);

/// Where the bytes of piece `piece` of a copy start in its slot of the
/// ring: the piece's first offset in the copy as [`CARD_COMPARE`] widens
/// it (`skew`: where the copy starts in its piece 0).
pub(crate) fn card_slot_start(piece: u32, skew: u32) -> usize {
    let lo = (piece as usize) * 65536;
    let lo = lo.saturating_sub(skew as usize);
    lo & !15
}

/// One range of a plan's compare ([`record_card_compare`]): the copy's
/// buffer and its hashes (GPU addresses), the copy's bytes, its span
/// (`crate::shadow::CardSpan`: first piece, skew, pieces) and the span's
/// first dword in the plan's run ([`RunLayout`]).
pub(crate) struct RangeCompare {
    pub(crate) copy: u64,
    pub(crate) hashes: u64,
    pub(crate) size: u32,
    pub(crate) first: u32,
    pub(crate) skew: u32,
    pub(crate) pieces: u32,
    pub(crate) at: u32,
}

/// What a plan's compare is recorded with ([`record_card_compare`]):
/// [`CARD_COMPARE`]'s root signature and pipeline, its results and its ring
/// (GPU addresses; with no slots the ring is any buffer, never written),
/// and the ring's slots (0: none).
pub(crate) struct CompareViews<'a> {
    pub(crate) root: &'a ID3D12RootSignature,
    pub(crate) pso: &'a ID3D12PipelineState,
    pub(crate) results: u64,
    pub(crate) ring: u64,
    pub(crate) slots: u32,
}

/// Records into `list` a plan's compare, run after its submission's lists
/// (`Host::card_before`): what their shaders wrote first, then the compare
/// of each range told `mode` ([`CARD_COMPARE`]'s `compare`), its results
/// from the range's `at` on, and with `count_at` (with the ring) the count
/// the compares left, there ([`RunLayout`]).
///
/// # Safety
/// `list` is open; the views and ranges are of live buffers that hold what
/// the compares read and write.
pub(crate) unsafe fn record_card_compare(
    list: &ID3D12GraphicsCommandList,
    views: &CompareViews,
    ranges: &[RangeCompare],
    mode: u32,
    count_at: Option<u32>,
) {
    let all_uavs = || D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    };
    // SAFETY: the caller's.
    unsafe {
        // What the submission's shaders wrote, first.
        list.ResourceBarrier(&[all_uavs()]);
        list.SetComputeRootSignature(views.root);
        list.SetPipelineState(views.pso);
        list.SetComputeRootUnorderedAccessView(3, views.results);
        list.SetComputeRootUnorderedAccessView(4, views.ring);
        for r in ranges {
            let constants = [r.first, r.skew, r.size, r.pieces, r.at * 4, mode, views.slots, 0];
            list.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
            list.SetComputeRootShaderResourceView(1, r.copy);
            list.SetComputeRootUnorderedAccessView(2, r.hashes);
            list.Dispatch(r.pieces.div_ceil(CARD_COMPARE_PIECES_PER_GROUP), 1, 1);
        }
        if let Some(at) = count_at {
            // Once the compares are over (they take the count in any
            // order): where they left it, for the host to move the limit
            // past their slots (`card_run_done`). Not only the count's
            // exactness but the slots' safety rests on the boundaries
            // between `ExecuteCommandLists` calls on one queue being full
            // barriers: a later plan's compares, in a later submission's
            // call, take counts only after this has left it. Were they to
            // run beside it, the count left could hold slots they took, and
            // the host would let the limit past those slots before their
            // landing merged them; it takes the count only if it is the
            // count before this plan and one per piece flagged here
            // ([`CardAreas`]).
            list.ResourceBarrier(&[all_uavs()]);
            let constants = [0, 0, 0, 0, at * 4, 3, views.slots, 0];
            list.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
            list.Dispatch(1, 1, 1);
        }
    }
}

/// `VICTORY_RELAY_SNAPSHOT=1` (with [`relay`]): the compare after a flight's
/// lists also copies each piece it finds differing into a ring in PC
/// memory, on the queue, before any later submission can write the copy;
/// the thread that lands the flight merges from there, with no fetch of
/// its own and no wait behind the submissions sent since (75 ms a frame of
/// the relay's 111 ms in the prison-bus scene, which kept 128 lists in the
/// air and the submitting thread waiting 88 ms a frame). A piece the ring
/// had no slot for, or a range not compared, makes its landing fetch as
/// before. `VICTORY_RELAY_SNAPSHOT_MB` sizes the ring (256).
fn relay_snapshot() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| relay() && std::env::var_os("VICTORY_RELAY_SNAPSHOT").is_some_and(|v| v == "1"))
}

/// The ring's megabytes with [`relay_snapshot`] (`VICTORY_RELAY_SNAPSHOT_MB`,
/// 256; from 16 to 4096).
fn card_ring_mb() -> u64 {
    std::env::var("VICTORY_RELAY_SNAPSHOT_MB")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(256)
        .clamp(16, 4096)
}

/// Since the last frame-rate line, of [`relay_snapshot`]: landings merged
/// from the ring, landings that fetched (a piece without a slot, or a range
/// not compared), pieces merged from the ring, their bytes, pieces left
/// out because a fetch had brought newer bytes of them (`MERGED_UPTO`),
/// plans that compared nothing for want of room in the compare's results,
/// and pieces found differing past the ring's limit (no slot).
static SNAPSHOT_STATS: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];

/// With [`relay_snapshot`], by copy (its `Arc` pointer) and piece: the
/// queue order (`QUEUE_ORDER`) a landing that fetched it ran behind. Such a
/// fetch runs behind every submission queued so far and may bring the bytes
/// of flights still in the air; a flight numbered below it (`CardPlan`'s
/// `sent_as`) ran wholly before the fetch, so its ring copy of the piece is
/// older than what was merged, and putting it in would take title memory
/// back until the newer flight lands. Kept only while such flights are in
/// the air ([`forget_merged_upto`]).
///
/// By bytes, not pieces: a fetch merges a piece cut to the landing plan's
/// own span, so another flight's range elsewhere in the same piece was not
/// brought by it (each entry: offsets in the copy, and the fetch's order).
static MERGED_UPTO: Mutex<Option<MergedUpto>> = Mutex::new(None);

/// [`MERGED_UPTO`]'s table: by copy and piece, the byte ranges fetches
/// merged, each with the queue order the fetch ran behind.
type MergedUpto = std::collections::HashMap<(usize, u32), Vec<(usize, usize, u64)>>;

/// The flights numbered up to `landed` have landed: an entry of
/// [`MERGED_UPTO`] no later flight is numbered below is of no use.
fn forget_merged_upto(landed: Option<u64>) {
    let Some(landed) = landed else { return };
    if let Some(upto) = MERGED_UPTO.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        forget_upto(upto, landed);
    }
}

/// [`forget_merged_upto`] on a table: a later flight is numbered above
/// `landed`, so an entry at `landed + 1` or below can leave out none.
fn forget_upto(upto: &mut MergedUpto, landed: u64) {
    upto.retain(|_, ranges| {
        ranges.retain(|r| r.2 > landed + 1);
        !ranges.is_empty()
    });
}

/// A fetch that ran behind queue order `after` merged these byte ranges
/// (copy, piece, offsets in the copy).
fn note_upto(upto: &mut MergedUpto, merged: impl IntoIterator<Item = ((usize, u32), (usize, usize))>, after: u64) {
    for (key, (lo, hi)) in merged {
        let ranges = upto.entry(key).or_default();
        match ranges.iter_mut().find(|r| r.0 == lo && r.1 == hi) {
            Some(r) => r.2 = r.2.max(after),
            None => ranges.push((lo, hi, after)),
        }
    }
}

/// What a landing from the ring does with a flagged piece of a flight
/// numbered `sent_as`, cut to `[lo, hi)`: `slot` is what the compare left
/// for it (`None`: no flag with a slot), `merged` what fetches merged of
/// the piece already ([`MERGED_UPTO`]).
#[derive(Debug, PartialEq, Eq)]
enum RingPiece {
    /// Merge its bytes from this slot.
    Merge(u32),
    /// Leave it out: fetches that ran after this flight merged newer bytes
    /// of all of it.
    Older,
    /// The ring cannot serve it (no slot), or a later fetch merged part of
    /// it only: the landing fetches, as before, which brings the newest
    /// bytes of all of it.
    Fetch,
}

fn ring_piece(slot: Option<u32>, slots: u32, sent_as: u64, merged: &[(usize, usize, u64)], lo: usize, hi: usize) -> RingPiece {
    let Some(slot) = slot.filter(|&s| s < slots) else {
        return RingPiece::Fetch;
    };
    // The parts of [lo, hi) a fetch that ran after this flight merged.
    let mut newer: Vec<(usize, usize)> = merged
        .iter()
        .filter(|r| sent_as < r.2 && r.0 < hi && r.1 > lo)
        .map(|r| (r.0.max(lo), r.1.min(hi)))
        .collect();
    if newer.is_empty() {
        return RingPiece::Merge(slot);
    }
    newer.sort_unstable();
    let mut covered = lo;
    for (a, b) in newer {
        if a > covered {
            return RingPiece::Fetch;
        }
        covered = covered.max(b);
    }
    if covered >= hi { RingPiece::Older } else { RingPiece::Fetch }
}

/// Whether the pieces hashed again after ranges were sent up to a copy in
/// the card's memory are hashed in runs of pieces that touch, each piece
/// once (`VICTORY_CARD_REHASH_RUNS=1`; off by default), and not in a pass
/// per range sent up.
///
/// These passes run on the card before the submission's lists. Run 102
/// sent up 514 ranges a frame, each with its own dispatch before joining.
/// The same pieces are hashed either way;
/// a piece hashed when it need not be, or not hashed when it should, costs
/// a fetch that finds nothing and loses no shader write.
fn rehash_runs() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_REHASH_RUNS").is_some_and(|v| v == "1"))
}

/// Whether the pieces hashed again after `ranges` ranges went up to a copy
/// in the card's memory are hashed in joined runs: with [`rehash_runs`],
/// and whatever it says with patches (`patches`) or past
/// [`REHASH_JOIN_FROM`] ranges. With patches a send takes up to a million
/// short ranges, and a pass of its own for each hashed the same pieces
/// thousands of times (80 thousand runs of 3 bytes: 80 thousand
/// dispatches, about 5 GB read on the card).
fn rehash_joined(patches: bool, ranges: usize) -> bool {
    rehash_runs() || patches || ranges > REHASH_JOIN_FROM
}

/// How the pieces a send touched of a copy in the card's memory are hashed
/// again after it ([`Host::card_before`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rehash {
    /// Not at all: the copy has no hashes yet (no shader writes declared).
    No,
    /// Every piece of the copy: its hashes were just made.
    Whole,
    /// The pieces of the ranges sent.
    Ranges,
}

/// One copy's part of a send to copies in the card's memory, as
/// [`plan_send`] takes it.
struct SendCopy<'a> {
    /// What `card_dirty` took of it: offsets, in order and apart.
    ranges: &'a [(usize, usize)],
    /// Whether its short ranges may go as patches ([`card_patchable`]).
    patchable: bool,
    /// The title memory it is a copy of (start, bytes): where its pieces lie.
    span: (usize, usize),
    rehash: Rehash,
}

/// Byte ranges (start, end).
type ByteRanges = Vec<(usize, usize)>;

/// A send to copies in the card's memory, planned before the queue's order
/// and the card's work are taken ([`plan_send`]).
struct SendPlan<B> {
    /// Per copy: the ranges that go up by a copy each, and those that go
    /// as patches.
    splits: Vec<(ByteRanges, ByteRanges)>,
    /// The patches of all the copies, and per copy where its own lie.
    patches: Vec<[u32; 3]>,
    patched_at: Vec<std::ops::Range<usize>>,
    /// The buffer the patches go up in and its bytes; `None` without any.
    buffer: Option<(B, u64)>,
    /// Per copy: the runs of pieces hashed again after the send.
    hashes: Vec<Vec<crate::shadow::CardSpan>>,
}

/// What [`Host::card_before`] makes of a send before it takes the queue's
/// order and the card's work, per copy: the ranges that go by a copy each
/// and those that go as patches ([`CARD_PATCH`]: the short ones of a copy
/// that takes them, [`crate::shadow::card_split`]), the buffer the patches
/// go in, the patches, and the runs of pieces hashed again after the send
/// (joined, each piece once, with `patching`, the patch compiled, or past
/// [`REHASH_JOIN_FROM`] ranges: [`rehash_joined`]). Both used to be made
/// under those locks, which the relay's fetches wait for: the hash runs of
/// a send of 1.4 million short ranges took 10-12 ms.
///
/// `buffer_for(bytes)` is the buffer for that many bytes of patches
/// ([`Host::patch_buffer`]): asked once, and only if some range goes as
/// patches. One that holds fewer than all of them (there was no memory for
/// more) takes as many as it holds, in the copies' order, and the short
/// ranges past it go by copies. One no patch went in (the ranges were cut
/// to the copy's mapping) goes to `give_back`. `patches_of(i, ranges, out)`
/// appends copy `i`'s patches of `ranges` (`Shadow::card_patches`).
fn plan_send<B>(
    copies: &[SendCopy<'_>],
    patching: bool,
    buffer_for: impl FnOnce(u64) -> Option<(B, u64)>,
    give_back: impl FnOnce((B, u64)),
    mut patches_of: impl FnMut(usize, &[(usize, usize)], &mut Vec<[u32; 3]>),
) -> SendPlan<B> {
    let split = |room: usize| -> (Vec<(ByteRanges, ByteRanges)>, usize) {
        let mut left = room;
        let split = copies.iter().map(|c| crate::shadow::card_split(c.ranges, c.patchable, &mut left)).collect();
        (split, room - left)
    };
    let (mut splits, dwords) = split(usize::MAX);
    let mut buffer = None;
    if dwords > 0 {
        buffer = buffer_for(dwords as u64 * 12);
        // Without memory for all of them: as many as the buffer holds,
        // the short ranges past it by copies.
        let held = buffer.as_ref().map_or(0, |b| (b.1 / 12) as usize);
        if held < dwords {
            splits = split(held).0;
        }
    }
    let mut patches = Vec::new();
    let patched_at = splits
        .iter()
        .enumerate()
        .map(|(i, (_, short))| {
            let first = patches.len();
            patches_of(i, short, &mut patches);
            first..patches.len()
        })
        .collect();
    if patches.is_empty()
        && let Some(unused) = buffer.take()
    {
        give_back(unused);
    }
    let hashes = copies
        .iter()
        .map(|c| match c.rehash {
            Rehash::No => Vec::new(),
            Rehash::Whole => crate::shadow::card_hash_runs(c.span, c.ranges, true, false),
            Rehash::Ranges => crate::shadow::card_hash_runs(c.span, c.ranges, false, rehash_joined(patching, c.ranges.len())),
        })
        .collect();
    SendPlan {
        splits,
        patches,
        patched_at,
        buffer,
        hashes,
    }
}

/// Since the last frame-rate line, of our own passes over copies in the
/// card's memory: hash passes after ranges were sent up, the pieces they
/// hashed, and compare passes after the lists.
static CARD_PASSES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether what shaders wrote to copies in the card's memory is merged
/// into title memory on several threads (`VICTORY_WIDE_MERGE=1`; off by
/// default). The pieces the card's compare found differing come down in
/// one fetch and were then merged one after the other on the thread that
/// settles: about 630 pieces and 40 MB a frame (run 104), each compared
/// with the copy's CPU side byte by byte and put into title memory through
/// the kernel. A piece is its own bytes of its copy and of title memory,
/// so eight threads take an eighth of them each.
fn wide_merge() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_MERGE").is_some_and(|v| v == "1"))
}

/// One declared range of a copy in the card's memory, in a submission.
struct CardRange {
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    /// As declared (title addresses), for the statistics.
    declared: (usize, usize),
    span: crate::shadow::CardSpan,
    /// Its first piece's dword in the compare's results.
    at: u32,
    /// Whether the card compares it; else every piece is fetched.
    compared: bool,
}

/// What a submission does for copies in the card's memory.
struct CardPlan {
    ranges: Vec<CardRange>,
    /// The compare, run after the submission's lists.
    check: Option<ID3D12CommandList>,
    /// The copies whose card side was brought up to date, alive until the
    /// submission has run.
    _synced: Vec<std::sync::Arc<crate::shadow::Shadow>>,
    /// The lists of ours the plan recorded (whether the compare, the
    /// list), given back by `Host::card_done`.
    lists: Vec<(bool, Prelude)>,
    /// With [`relay`]: the number its sends of title bytes to the card's
    /// side were given in the order of the queue (`QUEUE_ORDER`). Every
    /// send numbered up to it ran before the compare; every later one
    /// after it.
    sent_as: u64,
    /// Whether the compare copies the pieces it finds differing into the
    /// ring ([`relay_snapshot`]).
    in_ring: bool,
    /// Its run of the compare's results ([`CardAreas`]), given back by
    /// `Host::card_done`.
    run: Option<ResultRun>,
    /// With the ring: the run's last dword, where the compare leaves the
    /// ring's count after it.
    count_at: Option<u32>,
    /// The buffer its patches of title bytes were in ([`CARD_PATCH`]),
    /// given back with its lists.
    patches: Option<PatchBuffer>,
}

impl CardPlan {
    /// Takes the plan's run of the compare's results ([`CardAreas::lay_out`])
    /// for its compared ranges, which hold `pieces` pieces each, in their
    /// order: each range gets its first dword, and in the ring the plan
    /// keeps the run's last dword, where its compare leaves the count.
    /// Without room none of its ranges is compared (every piece of them is
    /// fetched): false.
    fn take_run(&mut self, areas: &mut CardAreas, pieces: &[u32]) -> bool {
        let Some(laid) = areas.lay_out(pieces, self.in_ring) else {
            for r in &mut self.ranges {
                r.compared = false;
            }
            return false;
        };
        for (r, at) in self.ranges.iter_mut().filter(|r| r.compared).zip(laid.ats) {
            r.at = at;
        }
        self.run = Some(laid.run);
        self.count_at = laid.count_at;
        true
    }
}

/// One piece of submission-time work, in recorded order.
enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory.
    Fill(usize, usize, usize),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only.
    Host(Box<dyn Fn() + Send>),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a texture's staging buffer up to date: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    Texture(std::sync::Arc<crate::texture::Staging>),
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload buffers its GPU commands read, kept until the list is reset.
#[derive(Default)]
struct Deferred {
    /// Destination, source, bytes.
    copies: Vec<Step>,
    uploads: Vec<ID3D12Resource>,
    /// The bytes of `uploads` (counted in [`ALIVE`] while they are held).
    upload_bytes: u64,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    kept: Vec<windows::core::IUnknown>,
    /// Pipeline and root-signature pairs already retained by this recording.
    /// Keeping both objects alive prevents their addresses from being reused.
    bound_pipelines: std::collections::HashSet<(usize, usize)>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    /// Textures whose contents the list brings up to date.
    textures: std::collections::HashSet<usize>,
    /// Staging buffers the list's commands copy from (`crate::texture::upload`
    /// recorded a copy): the flight of its submission reads them on the card,
    /// so a later detile into them waits for it ([`staging_wait`]).
    staging_copies: Vec<std::sync::Arc<crate::texture::Staging>>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
    /// Textures read back (texture to buffer copies), done once the list
    /// has run: the host buffer the list copies the texels into, the title
    /// memory they go to, the bytes of a row, the rows, the title's row
    /// pitch.
    backs: Vec<(ID3D12Resource, usize, [usize; 3])>,
    /// Whether a step zeroes the indirect-work budget.
    budget_zeroed: bool,
    /// The indirect scratch buffer (`HostList::indirect_args`, by its
    /// interface pointer) that a clamp in this recording left readable as
    /// indirect arguments; 0 if none. It stays readable until the next
    /// clamp into it ([`HostList::indirect_state`]).
    indirect_readable: usize,
}

impl Deferred {
    /// Holds `buffer` (`size` bytes of upload memory) until the list is
    /// reset.
    fn hold_upload(&mut self, buffer: ID3D12Resource, size: u64) {
        ALIVE[6].fetch_add(1, Ordering::Relaxed);
        ALIVE[7].fetch_add(size, Ordering::Relaxed);
        self.upload_bytes += size;
        self.uploads.push(buffer);
    }

    /// Clears all recorded entries while retaining container capacities so
    /// list resets do not free and reallocate `Vec`/`HashSet` storage every
    /// frame.
    fn clear(&mut self) {
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
        self.upload_bytes = 0;
        self.copies.clear();
        self.uploads.clear();
        self.kept.clear();
        self.bound_pipelines.clear();
        self.refreshed.clear();
        self.textures.clear();
        self.staging_copies.clear();
        self.writes.clear();
        self.backs.clear();
        self.budget_zeroed = false;
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
    }
}

/// Whether a queue's submissions go to the card without a wait for each
/// (`VICTORY_LATE_WAIT=1`; off by default, and off in the timing run, which
/// waits for every list on its own).
///
/// Every submission was prepared, sent, waited for and copied back before
/// the next one was looked at, so the card stood idle while we prepared
/// and we stood idle while it ran: in run 86 a frame of 446 ms is 134 ms of
/// preparing, 194 ms of waiting and 78 ms of copying back, one after the
/// other, on the one thread that submits.
///
/// With the switch a queue's submission is sent and joins a line of
/// "flights" ([`Flight`]); the thread goes on to the next one while the
/// card runs. The flights are settled together ([`Host::settle`]: one wait
/// for the card, what their shaders wrote copied back once per byte, then
/// their bottom-of-pipe writes and the fence signals behind them, in
/// order) at the next point where the title, or our own preparing, can
/// observe what they do. Those points, exactly:
///
/// 1. a synchronous submission of ours ([`Host::execute`]): a present, a
///    read-back or map of a texture, a clear or upload run at once;
/// 2. the next submission reads title memory that a flight's shaders may
///    write: a title buffer copy, a fill of upload memory, a texture's
///    memory, any step that runs code of ours on title memory
///    ([`late_blocked`]);
/// 3. the next submission refreshes a range of a host copy that a flight's
///    shaders may write, and the title wrote a page of it since it was last
///    copied ([`stale_under_flights`]): the refresh leaves such a range
///    alone until the flight's writes are back, as it always did within a
///    submission;
/// 4. a queue stands at a wait for a memory value that no flight's
///    bottom-of-pipe write is known to satisfy, at an address a flight may
///    write ([`late_met`]);
/// 5. the title blocks on a fence without an event;
/// 6. the card has run every flight (a thread of ours waits for that:
///    [`late_helper`]), or the flights hold [`late_most`] host lists.
///
/// What the title is told (bottom-of-pipe writes, fence values) is told
/// when the card has really run the work, later than before and never
/// earlier; a fence signalled on a queue while flights are in the air is
/// set behind them. A wait for a memory value that a flight's
/// bottom-of-pipe write satisfies counts as met: the waiting part goes to
/// the card behind that flight, which is the order the wait asks for.
///
/// What it assumes beyond today: memory the title rewrites between two of
/// its submissions, without waiting for the first, may be seen by the
/// first's shaders (as on the console, where the card reads title memory
/// when it runs, not when the list is submitted).
pub(crate) fn late() -> bool {
    late_stage() > 0
}

/// `VICTORY_LATE_WAIT`: 0 (off), 1, or 2 ([`late_exact`]).
fn late_stage() -> u8 {
    static STAGE: OnceLock<u8> = OnceLock::new();
    *STAGE.get_or_init(|| {
        if std::env::var_os("VICTORY_GPU_TIMING").is_some() {
            return 0;
        }
        match std::env::var("VICTORY_LATE_WAIT").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            Ok("3") => 3,
            // The relay alone: on top of stage 3.
            Err(_) if crate::shadow::relay() => 3,
            _ => 0,
        }
    })
}

/// Stage 2 of [`late`] (`VICTORY_LATE_WAIT=2`): point 3 of its list is no
/// longer a point. Where the title wrote a page that lies in a range a
/// flight's shaders may write, the bytes the title changed are found (the
/// page as it is now against what the copy held when the flight was
/// prepared) and put into the copy, and only those: what the flight's
/// shaders write beside them stays (`Shadow::merge_under_writes`). The
/// flights are settled for it only where that cannot be done: memory that
/// is not write-watched, or a copy in the card's memory with more changed
/// runs than a submission should send up one by one.
///
/// In run 92 (stage 1) this point settled the flights five times a frame
/// of twelve, each time with the card's work still to wait for: 172 ms a
/// frame of waiting were left of 200.
///
/// What differs from settling first: a byte both the title and a flight's
/// shader wrote is the title's if the shader wrote it first, the shader's
/// if it wrote it afterwards; settled first it was always the shader's.
/// The title writing a byte a shader of its own is writing is a race on
/// the console too.
fn late_exact() -> bool {
    late_stage() > 1
}

/// Stage 3 of [`late`] (`VICTORY_LATE_WAIT=3`: stage 2, and the present is
/// no longer a point where flights are settled). The copy to the back
/// buffer is sent as a flight of its own (`Host::execute_own`, a list of
/// its own for each present in the air) and the swap chain is told to
/// present behind it; the queue's call returns, and the card finishes the
/// frame while the title records the next one. What the title is told
/// about the frame (bottom-of-pipe writes, fences) is told when the card
/// has run it, by the settling thread.
///
/// What is left out with it: the look whether the presented frame is black
/// (diagnostics; it reads the frame back at once). A frame that is saved
/// or listed is presented the old way. Once every second and a half a
/// submission settles the flights all the same, for the sweep that lets
/// idle host objects go (it needs nothing in the air).
pub(crate) fn late_free_present() -> bool {
    late_stage() > 2
}

/// The relay's first stage (`VICTORY_RELAY=1`; off by default; it turns
/// [`late`] on at stage 3 unless `VICTORY_LATE_WAIT` says another stage,
/// and is off with it, in the timing run and with kept snapshots).
///
/// With [`late`] alone every settle does three things on the thread that
/// holds the submission lock: it waits for everything in the air, copies
/// back with the card standing, and only then goes on preparing. The frame
/// stays a relay: the card works about 242 ms of it and one thread
/// prepares, waits and copies back for about 220.
///
/// With the switch a thread of its own ([`relay_worker`]) lands flights as
/// soon as the card has run them, in the order they were sent and without
/// the submission lock: it fetches and merges what their shaders wrote to
/// copies in the card's memory, compares and merges the other copies,
/// frees the flights' ranges, and then does their read-backs, their
/// bottom-of-pipe writes and the fence signals behind them, in the line's
/// order. The submitting thread meanwhile prepares and sends the next
/// submissions. [`Host::settle`] no longer settles anything: it waits until
/// that thread has landed everything in the air, at the same points as
/// before (the list in [`late`]), so whoever can observe title memory, a
/// fence or a present still does so only after the flights before it are
/// merged.
///
/// What makes that safe is in `crate::shadow::relay` (ranges counted per
/// flight; a merge that never stores into memory the card may be writing;
/// sends to card copies numbered) and here: flights land in the order of
/// [`LINE`] by one thread, so a later flight's results never reach title
/// memory before an earlier flight's; the compare on the card stores the
/// hash it made, so each flight's compare flags exactly what changed since
/// the compare before it, however late the pieces are fetched; the fetch
/// goes to the queue under [`QUEUE_ORDER`], which says which sends of
/// title bytes the card had received when it ran.
pub(crate) fn relay() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| late_stage() > 0 && crate::shadow::relay())
}

/// The relay's second stage (`VICTORY_RELAY=2`): a submission that reads
/// title memory a flight's shaders may write (a title buffer copy, a fill
/// of upload memory, a texture) waits only until the flights that declared
/// that memory have landed, not for everything in the air; at
/// [`late_most`] lists in the air it waits until there are fewer; and with
/// the compare's results low until they are not ([`results_wait_over`]).
/// At `1` all three wait for everything.
fn relay_exact() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| relay() && std::env::var_os("VICTORY_RELAY").is_some_and(|v| v == "2"))
}

/// Told when something joins [`LINE`], for the thread that lands flights
/// ([`relay`]); [`LINE_MOVED`] stays the word for the thread that looks at
/// what queues hold.
static RELAY_MOVED: std::sync::Condvar = std::sync::Condvar::new();

/// Told when flights have landed ([`relay`]): who waits for that looks
/// again.
static LINE_LANDED: std::sync::Condvar = std::sync::Condvar::new();

/// With [`relay`], the order of what we put on the queue: the number of
/// sends of changed ranges to copies in the card's memory so far
/// (`Host::card_before`). Held from the taking of the ranges until their
/// list is on the queue, and while a fetch of pieces goes to the queue: a
/// fetch sent with the count at N ran behind sends 1..N and ahead of every
/// later one.
static QUEUE_ORDER: Mutex<u64> = Mutex::new(0);

/// Since the last frame-rate line, of the thread that lands flights
/// ([`relay`]): landings, flights landed, microseconds it waited for the
/// card with nothing to land, microseconds it waited for fetches of card
/// copies' pieces, microseconds of landing in all.
static RELAY_STATS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Of the line's things in order (a flight: the value signalled on the
/// host's fence behind it; a write or a signal behind a flight: `None`):
/// how many, from the front, can be landed when the card has reached
/// `completed`: everything ahead of the first flight it has not run. A
/// later flight never lands before an earlier one, and what follows a
/// flight (its bottom-of-pipe writes, signals) never before the flight.
fn landable(line: &[Option<u64>], completed: u64) -> usize {
    line.iter().position(|f| f.is_some_and(|f| f > completed)).unwrap_or(line.len())
}

/// For one plan of copies in the card's memory, with [`relay`]: `flags`
/// holds, for every piece of every range of the plan, the copy, the
/// piece's number in the copy and whether the compare flagged it there (or
/// did not run). The pieces to fetch, under whichever range: all of them
/// flagged under any. Two ranges of a plan may hold parts of the same
/// 64 KiB piece; the compare hashes the whole piece for each and stores
/// the hash, so the second of them may find it stored already and not
/// flag it.
fn flagged_pieces(flags: &[(usize, u32, bool)]) -> std::collections::HashSet<(usize, u32)> {
    flags.iter().filter(|f| f.2).map(|f| (f.0, f.1)).collect()
}

#[cfg(test)]
mod relay_tests {
    use super::{RingPiece, flagged_pieces, forget_upto, landable, note_upto, ring_piece};

    /// The change from fetching to the ring (`VICTORY_RELAY_SNAPSHOT`): a
    /// landing that had to fetch ran behind every submission queued so far,
    /// so it may have merged a later flight's write to a piece. A flight
    /// numbered below that fetch then lands from the ring with an older
    /// copy of the same piece: it must be left out, not put over the newer
    /// bytes. A flight numbered at or above the fetch may have run after it
    /// and is merged. The note goes once no flight below it can land.
    #[test]
    fn an_older_ring_copy_does_not_undo_a_newer_fetch() {
        let mut upto = std::collections::HashMap::new();
        let piece = (0x1000usize, 7u32);
        let other = (0x1000usize, 8u32);
        type Notes = std::collections::HashMap<(usize, u32), Vec<(usize, usize, u64)>>;
    let notes = |upto: &Notes, key| {
            upto.get(&key).cloned().unwrap_or_default()
        };
        // Flights 8, 9 and 10 are in the air; flight 7's landing fetches
        // behind queue order 10 and merges [0x70000, 0x71000) of `piece`,
        // where flight 9 wrote too.
        note_upto(&mut upto, [(piece, (0x70000, 0x71000))], 10);
        // Flights 8 and 9 land from the ring with their copies of those bytes.
        assert_eq!(ring_piece(Some(3), 16, 8, &notes(&upto, piece), 0x70000, 0x71000), RingPiece::Older);
        assert_eq!(ring_piece(Some(4), 16, 9, &notes(&upto, piece), 0x70800, 0x70c00), RingPiece::Older);
        // Another piece the fetch did not bring is merged.
        assert_eq!(ring_piece(Some(5), 16, 9, &notes(&upto, other), 0x80000, 0x81000), RingPiece::Merge(5));
        // Another range of the same piece that the fetch did not bring
        // (flight 9 wrote [0x78000, 0x79000)): merged, not left out.
        assert_eq!(ring_piece(Some(5), 16, 9, &notes(&upto, piece), 0x78000, 0x79000), RingPiece::Merge(5));
        // A range the fetch brought only part of: the landing fetches, which
        // brings the newest bytes of all of it.
        assert_eq!(ring_piece(Some(5), 16, 9, &notes(&upto, piece), 0x70800, 0x71800), RingPiece::Fetch);
        // Two fetched ranges that together cover it: left out.
        note_upto(&mut upto, [(piece, (0x71000, 0x72000))], 10);
        assert_eq!(ring_piece(Some(5), 16, 9, &notes(&upto, piece), 0x70800, 0x71800), RingPiece::Older);
        // Flight 10 took its sends' number before the fetch: its lists may
        // have run after it, so its copy is merged.
        assert_eq!(ring_piece(Some(6), 16, 10, &notes(&upto, piece), 0x70000, 0x71000), RingPiece::Merge(6));
        // A later fetch of the same bytes only raises the note.
        note_upto(&mut upto, [(piece, (0x70000, 0x71000))], 9);
        assert_eq!(notes(&upto, piece), [(0x70000, 0x71000, 10), (0x71000, 0x72000, 10)]);
        // Flight 8 landed: flight 9 still needs the notes.
        forget_upto(&mut upto, 8);
        assert!(upto.contains_key(&piece));
        // Flight 9 landed: no flight below 10 is left.
        forget_upto(&mut upto, 9);
        assert!(upto.is_empty());
        // Without a note, a slot merges; a piece flagged with no slot (the
        // count past the limit) or with a slot past the ring makes the
        // landing fetch.
        assert_eq!(ring_piece(Some(0), 1, 3, &[], 0, 16), RingPiece::Merge(0));
        assert_eq!(ring_piece(None, 16, 3, &[], 0, 16), RingPiece::Fetch);
        assert_eq!(ring_piece(Some(16), 16, 3, &[], 0, 16), RingPiece::Fetch);
        assert_eq!(ring_piece(None, 16, 3, &[(0, 16, 99)], 0, 16), RingPiece::Fetch);
    }

    /// Two flights and what follows each: nothing of the second lands
    /// before the card has run it, and nothing behind a flight lands
    /// before the flight.
    #[test]
    fn flights_land_in_the_order_they_were_sent() {
        // Flight (fence 5), its write, a signal, flight (fence 6), its write.
        let line = [Some(5), None, None, Some(6), None];
        assert_eq!(landable(&line, 4), 0);
        assert_eq!(landable(&line, 5), 3);
        assert_eq!(landable(&line, 6), 5);
        assert_eq!(landable(&line, u64::MAX), 5);
        // Signals with no flight ahead land at once; behind one they wait.
        assert_eq!(landable(&[None, None], 0), 2);
        assert_eq!(landable(&[None, Some(9), None], 8), 1);
        assert_eq!(landable(&[], 3), 0);
        // After the first three landed, the rest when its turn comes.
        assert_eq!(landable(&line[3..], 5), 0);
        assert_eq!(landable(&line[3..], 6), 2);
    }

    /// A piece flagged under one range of a plan is fetched under every
    /// range that holds a part of it; pieces of other copies are not.
    #[test]
    fn a_piece_flagged_under_one_range_is_fetched_under_all() {
        let (a, b) = (0x1000usize, 0x2000usize);
        // Copy a: piece 3 under two ranges, flagged under the first only;
        // piece 4 not flagged; copy b: piece 3 not flagged, piece 9 flagged.
        let flags = [(a, 3, true), (a, 4, false), (a, 3, false), (b, 3, false), (b, 9, true)];
        let fetch = flagged_pieces(&flags);
        assert!(fetch.contains(&(a, 3)));
        assert!(!fetch.contains(&(a, 4)));
        assert!(!fetch.contains(&(b, 3)));
        assert!(fetch.contains(&(b, 9)));
        assert_eq!(fetch.len(), 2);
    }
}

/// The most host lists in the air before the flights are settled
/// (`VICTORY_LATE_WAIT_LISTS`, 128; 32 settled the flights three times a
/// frame in run 92 and 64 still 1.3-1.7 times in run 99, with 12,919 tables
/// asked and none refused; watch "tables asked, N refused"): their dynamic
/// descriptor slots, upload
/// memory and scratch chunks are held until then, and a frame's lists held
/// all at once ran the slots out before (run 45).
fn late_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_LATE_WAIT_LISTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(128).max(1)
    })
}

/// Whether a submission with bottom-of-pipe writes may fly
/// (`VICTORY_LATE_WAIT_WRITES=0`: it is settled as soon as it is sent, so
/// the title is told at once, as before; for telling a title that polls
/// such a value from one that does not).
fn late_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LATE_WAIT_WRITES").is_none_or(|v| v != "0"))
}

/// Per host copy, the ranges a submission's shaders may write, as declared
/// (address, end).
type Declared = (std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>);

/// What [`Host::submit`] hands back for a submission it sent and did not
/// wait for.
struct Sent {
    /// Its number among the submissions started.
    number: u64,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    /// Its bottom-of-pipe writes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
}

/// A submission on the card that has not been waited for ([`late`]): its
/// host lists, taken from the title's (which it may reset meanwhile), and
/// what is to be done once it has run.
struct Flight {
    number: u64,
    /// The value signalled on the host's fence behind it.
    fence: u64,
    lists: Vec<crate::list::Detached>,
    order: Vec<usize>,
    card: Option<CardPlan>,
    back: Vec<Declared>,
}

/// One thing in the line of what waits for the card, in order.
// A few dozen a frame, each moved once: a box would cost more than the bytes.
#[allow(clippy::large_enum_variant)]
enum Late {
    Run(Flight),
    /// A bottom-of-pipe write of the flight before it: address, what, bytes.
    Write(usize, crate::queue::Written, usize),
    /// A fence (its address) signalled behind the flights before it.
    Signal(usize, u64),
}

struct Line {
    line: Vec<Late>,
    /// The title memory the flights' shaders may write (address, end), in
    /// order and joined.
    written: Vec<(usize, usize)>,
    /// Host lists in the flights.
    lists: usize,
    /// The value signalled on the host's fence behind the last flight.
    target: u64,
    /// A settle has taken the line and is not done: a signal still goes
    /// behind it.
    settling: bool,
    /// Something was settled away from a queue's own calls: the helper
    /// thread looks at what queues hold.
    poke: bool,
    /// Host lists of settled flights, reset and given back outside the
    /// submission lock ([`land`]: a settle may run under a caller's own
    /// locks).
    landed: Vec<(Vec<crate::list::Detached>, Vec<usize>)>,
    /// With [`relay`]: the bottom-of-pipe writes of the flights being
    /// landed (taken out of `line`, not done yet), for [`late_met`].
    in_hand: Vec<(usize, crate::queue::Written, usize)>,
}

// SAFETY: D3D12 objects (free-threaded) and host lists nothing records
// into; touched under the lock.
unsafe impl Send for Line {}

static LINE: Mutex<Line> = Mutex::new(Line {
    line: Vec::new(),
    written: Vec::new(),
    lists: 0,
    target: 0,
    settling: false,
    poke: false,
    landed: Vec::new(),
    in_hand: Vec::new(),
});
/// Told when a flight joins [`LINE`] or it is poked.
static LINE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

fn line() -> std::sync::MutexGuard<'static, Line> {
    LINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why flights were settled ([`LATE_STATS`]' places from 2 on).
const WHY_SYNC: usize = 2;
const WHY_CARD_DONE: usize = 3;
const WHY_VALUE: usize = 4;
const WHY_STALE: usize = 5;
const WHY_READ: usize = 6;
const WHY_RUN: usize = 7;
const WHY_MOST: usize = 8;
const WHY_WRITES: usize = 9;
const WHY_FENCE: usize = 10;

/// Since the last frame-rate line: submissions sent without a wait;
/// settles; settles by reason ([`WHY_SYNC`]..); flights settled;
/// microseconds settles waited for the card; value waits met by a flight's
/// bottom-of-pipe write; fence signals set behind flights.
/// Then: the settles for title memory read, by what read it (a buffer
/// copy, a fill of upload memory, a texture).
static LATE_STATS: [std::sync::atomic::AtomicU64; 18] = [const { std::sync::atomic::AtomicU64::new(0) }; 18];

/// Since the last frame-rate line: the microseconds settles waited for the
/// card, by why they settled (the `WHY_` places).
static LATE_WAITED: [std::sync::atomic::AtomicU64; 11] = [const { std::sync::atomic::AtomicU64::new(0) }; 11];

/// Whether `[from, to)` touches one of `ranges` (in order, joined).
fn touches(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    let at = ranges.partition_point(|r| r.1 <= from);
    ranges.get(at).is_some_and(|r| r.0 < to)
}

/// A fill of upload memory from title memory `[src, src + len)` waits for
/// flights whose shaders may write `written` (in order, joined): said for
/// the first 32 places, each at its 1st, 2nd, 4th... time, with where the
/// first written byte lies in the fill. A root constant view has no size
/// and is filled 64 KiB long (`draw::root_view`): a first written byte far
/// into the fill is a neighbour, not the constants the shader reads.
fn note_blocked_fill(written: &[(usize, usize)], src: usize, len: usize) {
    static SEEN: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());
    let at = written.partition_point(|r| r.1 <= src);
    let Some(&(a, b)) = written.get(at) else {
        return;
    };
    let n = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        match seen.iter().position(|s| s.0 == src) {
            Some(i) => {
                seen[i].1 += 1;
                seen[i].1
            }
            None if seen.len() < 32 => {
                seen.push((src, 1));
                1
            }
            None => return,
        }
    };
    if !n.is_power_of_two() {
        return;
    }
    tracing::info!(
        src = format_args!("{src:#x}"),
        len = format_args!("{len:#x}"),
        written = format_args!("{a:#x}..{b:#x}"),
        first_written_at = format_args!("+{:#x}", a.saturating_sub(src)),
        times = n,
        "a fill of upload memory waits for flights that may write what it reads"
    );
}

/// `ranges` with `more`, in order, joined where they overlap or touch.
fn join_ranges(ranges: &mut Vec<(usize, usize)>, more: impl IntoIterator<Item = (usize, usize)>) {
    ranges.extend(more.into_iter().filter(|r| r.0 < r.1));
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(from, to) in ranges.iter() {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    *ranges = out;
}

/// Whether anything is in the air.
fn flying() -> bool {
    if !late() {
        return false;
    }
    let line = line();
    // (With the relay: also while flights are being landed.)
    line.settling || line.line.iter().any(|l| matches!(l, Late::Run(_)))
}

/// Whether a title buffer copy whose source alone lies in memory that
/// flights' shaders may write runs without waiting for them
/// (`VICTORY_COPY_NO_WAIT=1`). The title's 174 MiB heap is declared written
/// whole by a table view, and one 20 KiB copy out of it a frame waited about
/// 40 ms for bytes no shader writes. Off: such a copy waits, like every read.
fn copy_no_wait() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPY_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Whether a title buffer copy must wait for the flights that declared
/// `written`: always for its destination, and for its source unless
/// `no_wait` ([`copy_no_wait`]).
fn copy_blocks(written: &[(usize, usize)], dst: usize, src: usize, len: usize, no_wait: bool) -> bool {
    touches(written, dst, dst + len) || !no_wait && touches(written, src, src + len)
}

/// A title buffer copy that ran while a flight could still write its source.
struct UnsettledCopy {
    dst: usize,
    src: usize,
    len: usize,
    /// The last submission whose shaders the copy must see.
    until: u64,
    /// A copy-back wrote over the source since the copy ran.
    written: bool,
}

/// The copies [`copy_no_wait`] let run: if a flight's copy-back writes over
/// a source, the copy is made again when that flight has landed, as the
/// console's copy, in order after those shaders, would have read it.
/// shortcut: a write by a shader that follows the copy in its own
/// submission is taken too, and a source the title has used again by then
/// gives its new bytes; neither was seen (0 of about 9,000 copies had a
/// copy-back over their source at all).
struct Unsettled(Vec<UnsettledCopy>);

impl Unsettled {
    /// A copy ran during the preparing of submission `until`. An earlier
    /// copy into the same bytes is not made again.
    fn ran(&mut self, dst: usize, src: usize, len: usize, until: u64) {
        self.0.retain(|c| !(c.dst < dst + len && dst < c.dst + c.len));
        self.0.push(UnsettledCopy {
            dst,
            src,
            len,
            until,
            written: false,
        });
    }

    /// The copy-back writes title memory `[at, at + len)`.
    fn written(&mut self, at: usize, len: usize) {
        for c in &mut self.0 {
            if c.src < at + len && at < c.src + c.len {
                c.written = true;
            }
        }
    }

    /// The copies to make again (destination, source, bytes), each once
    /// per write over its source.
    fn due(&mut self) -> Vec<(usize, usize, usize)> {
        let due = self.0.iter().filter(|c| c.written).map(|c| (c.dst, c.src, c.len)).collect();
        for c in &mut self.0 {
            c.written = false;
        }
        due
    }

    /// Every flight up to `last` has landed.
    fn landed(&mut self, last: u64) {
        self.0.retain(|c| c.until > last);
    }
}

static UNSETTLED: Mutex<Unsettled> = Mutex::new(Unsettled(Vec::new()));
/// For the log: copies let run without waiting, copies made again, fills
/// let run without waiting, fills whose source a copy-back wrote after.
static COPY_NO_WAIT: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The fills of upload memory [`copy_no_wait`] let run: per submission, the
/// source of each and its bytes. A fill cannot be made again (the list that
/// reads the upload memory is on the card by then), so a copy-back over a
/// source is only counted: the list drew with the bytes from before those
/// shaders ran. None was seen; the count in the log says if that changes.
struct UnsettledFills {
    /// The submission whose preparing ran them, and source to bytes.
    by_submission: std::collections::VecDeque<(u64, std::collections::BTreeMap<usize, usize>)>,
    /// The longest fill noted.
    longest: usize,
}

impl UnsettledFills {
    fn ran(&mut self, src: usize, len: usize, until: u64) {
        if self.by_submission.back().is_none_or(|b| b.0 != until) {
            self.by_submission.push_back((until, Default::default()));
        }
        let fills = &mut self.by_submission.back_mut().expect("just pushed").1;
        let bytes = fills.entry(src).or_insert(0);
        *bytes = len.max(*bytes);
        self.longest = self.longest.max(len);
    }

    /// The copy-back writes title memory `[at, at + len)`: how many noted
    /// fills read from there, each counted once.
    fn written(&mut self, at: usize, len: usize) -> u64 {
        let from = at.saturating_sub(self.longest);
        let mut hit = 0;
        for (_, fills) in &mut self.by_submission {
            let over: Vec<usize> = fills.range(from..at + len).filter(|&(&src, &n)| at < src + n).map(|(&src, _)| src).collect();
            hit += over.len() as u64;
            for src in over {
                fills.remove(&src);
            }
        }
        hit
    }

    /// Every flight up to `last` has landed.
    fn landed(&mut self, last: u64) {
        while self.by_submission.front().is_some_and(|f| f.0 <= last) {
            self.by_submission.pop_front();
        }
    }
}

static UNSETTLED_FILLS: Mutex<UnsettledFills> = Mutex::new(UnsettledFills {
    by_submission: std::collections::VecDeque::new(),
    longest: 0,
});

fn unsettled_fills() -> std::sync::MutexGuard<'static, UnsettledFills> {
    UNSETTLED_FILLS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A fill of upload memory ran at a submission's preparing.
fn fill_ran(src: usize, len: usize) {
    if copy_no_wait() && late() {
        unsettled_fills().ran(src, len, crate::shadow::prepared());
    }
}

fn unsettled() -> std::sync::MutexGuard<'static, Unsettled> {
    UNSETTLED.lock().unwrap_or_else(|e| e.into_inner())
}

/// A title buffer copy ran at a submission's preparing.
fn copy_ran(dst: usize, src: usize, len: usize) {
    if copy_no_wait() && late() {
        unsettled().ran(dst, src, len, crate::shadow::prepared());
    }
}

/// The copy-back writes title memory `[at, at + len)` (`crate::shadow`).
pub(crate) fn copy_source_written(at: usize, len: usize) {
    if copy_no_wait() {
        unsettled().written(at, len);
        let fills = unsettled_fills().written(at, len);
        if fills > 0 && COPY_NO_WAIT[3].fetch_add(fills, Ordering::Relaxed) == 0 {
            tracing::warn!(at, len, "a flight's shaders wrote the source of a fill of upload memory that had not waited: its list drew with the bytes from before");
        }
    }
}

/// `VICTORY_BACK_ALIAS=1`: the copy-back writes title memory that is a view
/// of a section through a view of our own of it ([`write_title_unseen`]),
/// so the guard stays closed and the copy that merged the bytes does not
/// take them again at its next refresh. Half the bytes refreshed a frame
/// were that echo (65 of 129 MB, standing in the first room).
fn back_alias() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BACK_ALIAS").is_some_and(|v| v == "1"))
}

/// Copy-back writes per frame for the log: through our own view, through
/// the title's address (all), and of those: not view memory, the runtime
/// remapping, a fault.
pub(crate) static BACK_WRITES: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Writes `n` bytes from `src` into title memory at `at` through our view
/// of its section ([`back_alias`]) and tells the journal, for every
/// address the pages show at. Returns the journal's asking and, per page
/// (page number, asking that had last found it written), for
/// `Shadow::wrote_unseen`. `None`: nothing written that way (off, not a
/// view, the runtime is remapping, or the write faulted); the caller
/// writes as before.
pub(crate) fn write_title_unseen(at: usize, src: *const u8, n: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    if !back_alias() {
        return None;
    }
    write_unseen(at, src, n)
}

/// [`write_title_unseen`] whatever [`back_alias`] says.
pub(crate) fn write_unseen(at: usize, src: *const u8, n: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    if n == 0 || !direct_copy_enabled() {
        return None;
    }
    // The runtime does not move a physical page between finding where the
    // bytes show and writing them; if it is moving pages now, write as before.
    let Some(_still) = crate::watch::title_memory_reading_now() else {
        BACK_WRITES[3].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    // By title view: the range may cross several.
    let mut parts = Vec::new();
    let mut a = at;
    while a < at + n {
        let Some((alias, held)) = crate::inplace::alias_run(a, at + n - a) else {
            BACK_WRITES[2].fetch_add(1, Ordering::Relaxed);
            return None;
        };
        parts.push((alias, a - at, held));
        a += held;
    }
    for (alias, from, held) in parts {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `alias` shows `held` bytes of the section; a fault is caught.
        let done = unsafe { direct_copy::try_copy(alias as *mut u8, src.add(from), held) };
        #[cfg(not(target_arch = "x86_64"))]
        let done = false;
        if !done {
            // What was written already is written again the old way.
            BACK_WRITES[4].fetch_add(1, Ordering::Relaxed);
            return None;
        }
    }
    let first = at / crate::watch::PAGE;
    let last = (at + n).div_ceil(crate::watch::PAGE);
    let mut pages = Vec::with_capacity(last - first);
    for page in first..last {
        let shown = crate::inplace::pages_of(page * crate::watch::PAGE);
        if shown.is_empty() {
            pages.push(page);
        } else {
            pages.extend(shown.iter().map(|a| a / crate::watch::PAGE));
        }
    }
    let (now, before) = crate::watch::wrote_unseen(&pages);
    crate::descriptor::host::remapped(at, n);
    BACK_WRITES[0].fetch_add(1, Ordering::Relaxed);
    Some((now, pages.into_iter().zip(before).collect()))
}

/// After a landing's copy-backs: makes again the copies whose source they
/// wrote over.
fn recopy_written_sources() {
    if !copy_no_wait() {
        return;
    }
    let due = unsettled().due();
    for (dst, src, len) in due {
        if COPY_NO_WAIT[1].fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::warn!(dst, src, len, "a flight's shaders wrote the source of a title buffer copy that had not waited: copied again");
        }
        crate::watch::writing(dst, len);
        copy_mapped(dst as *mut u8, src as *const u8, len, true);
    }
}

/// Every flight up to `last` has landed: their copies are settled.
fn copies_settled(last: u64) {
    if copy_no_wait() {
        unsettled().landed(last);
        unsettled_fills().landed(last);
    }
}

#[cfg(test)]
mod copy_no_wait_tests {
    use super::{Unsettled, copy_blocks};

    #[test]
    fn a_copy_waits_for_its_destination_and_for_its_source_unless_told_not_to() {
        let written = [(0x1000, 0x2000)];
        // source inside, destination outside
        assert!(copy_blocks(&written, 0x9000, 0x1800, 0x100, false));
        assert!(!copy_blocks(&written, 0x9000, 0x1800, 0x100, true));
        // destination inside: waits either way
        assert!(copy_blocks(&written, 0x1800, 0x9000, 0x100, true));
        assert!(copy_blocks(&written, 0x1800, 0x9000, 0x100, false));
        // neither
        assert!(!copy_blocks(&written, 0x9000, 0xa000, 0x100, false));
    }

    #[test]
    fn a_copy_is_made_again_once_when_a_copy_back_writes_its_source() {
        let mut u = Unsettled(Vec::new());
        u.ran(0x9000, 0x1800, 0x100, 7);
        u.ran(0xa000, 0x3000, 0x100, 7);
        // beside both sources, then over the end of the first
        u.written(0x1900, 0x10);
        assert!(u.due().is_empty());
        u.written(0x18ff, 1);
        assert_eq!(u.due(), [(0x9000, 0x1800, 0x100)]);
        assert!(u.due().is_empty(), "once per write");
    }

    #[test]
    fn a_later_copy_into_the_same_bytes_replaces_the_earlier_one() {
        let mut u = Unsettled(Vec::new());
        u.ran(0x9000, 0x1800, 0x100, 7);
        u.ran(0x9080, 0x5000, 0x100, 8);
        u.written(0x1800, 0x100);
        assert!(u.due().is_empty(), "the newer copy's bytes stay");
        u.written(0x5000, 1);
        assert_eq!(u.due(), [(0x9080, 0x5000, 0x100)]);
    }

    #[test]
    fn a_copy_back_over_a_fills_source_counts_it_once_until_its_submission_lands() {
        let mut f = super::UnsettledFills {
            by_submission: Default::default(),
            longest: 0,
        };
        f.ran(0x1000, 0x1e0, 7);
        f.ran(0x3000, 0xa0, 7);
        f.ran(0x5000, 0xa0, 8);
        // beside every source, then over the tail of the first and the head of the third
        assert_eq!(f.written(0x11e0, 0x20), 0);
        assert_eq!(f.written(0x11df, 1), 1);
        assert_eq!(f.written(0x1000, 0x1e0), 0, "counted once");
        assert_eq!(f.written(0x4ff0, 0x11), 1);
        // submission 7 lands: its fill at 0x3000 is settled
        f.landed(7);
        assert_eq!(f.written(0x3000, 0xa0), 0);
        f.ran(0x7000, 0xa0, 9);
        f.landed(8);
        assert_eq!(f.written(0x7000, 1), 1);
    }

    #[test]
    fn a_copy_is_settled_when_its_own_submission_has_landed() {
        let mut u = Unsettled(Vec::new());
        u.ran(0x9000, 0x1800, 0x100, 7);
        u.landed(6);
        u.written(0x1800, 1);
        assert_eq!(u.due().len(), 1, "flight 7 can still write it");
        u.landed(7);
        u.written(0x1800, 1);
        assert!(u.due().is_empty());
    }
}

/// With flights in the air ([`late`]): why `runs` cannot be prepared
/// before they are settled, if so. Preparing reads title memory on the
/// CPU (the title's buffer copies, fills of upload memory, textures) and
/// runs steps of ours that may read or write it anywhere; what the
/// flights' shaders write is not there yet.
fn late_blocked(runs: &[&HostList]) -> Option<usize> {
    let written = {
        let line = line();
        if line.written.is_empty() {
            return None;
        }
        line.written.clone()
    };
    blocked_by(&written, runs, true)
}

/// [`late_blocked`] against `written`, the title memory flights' shaders
/// may write (in order, joined); `count` whether the kind of read is
/// counted for the frame line.
fn blocked_by(written: &[(usize, usize)], runs: &[&HostList], count: bool) -> Option<usize> {
    if written.is_empty() {
        return None;
    }
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    let no_wait = copy_no_wait();
                    if count && no_wait && copy_blocks(written, dst, src, len, false) && !copy_blocks(written, dst, src, len, true) {
                        COPY_NO_WAIT[0].fetch_add(1, Ordering::Relaxed);
                    }
                    if copy_blocks(written, dst, src, len, no_wait) {
                        if count {
                            LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                &Step::Fill(_, src, len) => {
                    if touches(written, src, src + len) && copy_no_wait() {
                        if count {
                            COPY_NO_WAIT[2].fetch_add(1, Ordering::Relaxed);
                        }
                    } else if touches(written, src, src + len) {
                        if count {
                            LATE_STATS[16].fetch_add(1, Ordering::Relaxed);
                            note_blocked_fill(written, src, len);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    if touches(written, at, at + len) {
                        if count {
                            LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                        }
                        return Some(WHY_READ);
                    }
                }
                Step::Run(_) => return Some(WHY_RUN),
                Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(_) => {}
            }
        }
    }
    None
}

/// Whether any texture staging buffer refreshed by `runs` is still being
/// read on the GPU by an unfinished flight (`in_flight > finished`):
/// refreshing it now would overwrite its mapped upload buffer mid-copy.
fn staging_in_flight_at(runs: &[&HostList], finished: u64) -> bool {
    runs.iter().any(|list| {
        list.deferred().copies.iter().any(|step| {
            matches!(step, Step::Texture(staging) if staging.in_flight() > finished)
        })
    })
}

fn staging_in_flight(runs: &[&HostList]) -> bool {
    staging_in_flight_at(runs, FINISHED.load(Ordering::Acquire))
}

/// With [`relay_exact`] flights in the air: before a texture's staging
/// buffer is detiled into (its title memory changed), the flight numbered
/// `in_flight`, the last that copies from it on the card, lands. Nothing
/// waits when the texture is only sampled, or when it changed and no
/// flight in the air copies from it. Called from the submission's
/// preparing (`Staging::detile`), which may hold the submission lock; the
/// thread that lands flights never takes it.
pub(crate) fn staging_wait(in_flight: u64) {
    if in_flight == 0 || !late() || !relay_exact() || FINISHED.load(Ordering::Acquire) >= in_flight {
        return;
    }
    relay_wait(WHY_READ, |_| FINISHED.load(Ordering::Acquire) >= in_flight);
}

/// With flights in the air, after the submission's write-watch asking:
/// whether `runs` refresh a range of a host copy that a flight's shaders
/// may write and that the title wrote since it was last copied. The
/// refresh leaves ranges shaders are writing alone (`Shadow::update`), so
/// the title's bytes there would reach the card a submission late.
///
/// `reserve`: where the title's bytes are put in beside the flights' writes
/// and a copy in the card's memory is among those refreshed, the buffer for
/// the submission's patches ([`Host::patch_reserve`]), taken before any
/// run is put in, for [`Host::card_before`].
fn stale_under_flights(host: &Host, runs: &[&HostList], reserve: &mut Option<PatchBuffer>) -> bool {
    let spans = refreshed_under_flights(runs);
    if late_exact() {
        return put_in_under_flights(&spans, reserve, || host.patch_reserve());
    }
    spans.iter().any(|(shadow, from, to)| shadow.stale_under_writes(*from, *to - *from))
}

/// Per copy that shaders of flights in the air may be writing, the span of
/// what `runs` refresh of it (title addresses: a flush brings that much up
/// to date at most).
fn refreshed_under_flights(runs: &[&HostList]) -> Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> {
    use std::sync::Arc;
    let mut spans: Vec<(Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::Refresh(shadow, address, len) = step
                && shadow.writes_pending()
            {
                match spans.iter_mut().find(|s| Arc::ptr_eq(&s.0, shadow)) {
                    Some(s) => (s.1, s.2) = (s.1.min(*address), s.2.max(*address + *len)),
                    None => spans.push((shadow.clone(), *address, *address + *len)),
                }
            }
        }
    }
    spans
}

/// [`late_exact`]: the title's bytes are put in beside the flights' writes
/// in `spans` (per copy, title addresses); whether a settle is still due,
/// where that could not be done. What goes up to copies in the card's
/// memory is held to a submission's room: 512 long runs, each a copy of its
/// own there, and the short ones as dword patches ([`CARD_PATCH`]) while
/// 16 MiB of patches last (the bus scene of run dd56178 put in 20-80
/// thousand runs of about 3 bytes a frame under flights, and still settled
/// for title memory changed under them about twice a frame: 75 ms of
/// waiting for the card). The buffer the patches go up in, the `reserve`,
/// is taken first (`take_reserve`, [`Host::patch_reserve`]) where a copy in
/// the card's memory is among those refreshed: without it the short runs
/// take copies too, and past 512 a settle is due, as before the patches.
/// Nanoseconds of a submission's preparing, for the log: title bytes put in
/// under flights, the lists' steps, the flush of their refreshes, the card
/// copies' sends and compare.
static PREPARING_NS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Runs `f`, its time added to part `i` of [`PREPARING_NS`].
fn timed<T>(i: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    PREPARING_NS[i].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    out
}

fn put_in_under_flights<B>(
    spans: &[(std::sync::Arc<crate::shadow::Shadow>, usize, usize)],
    reserve: &mut Option<B>,
    take_reserve: impl FnOnce() -> Option<B>,
) -> bool {
    if reserve.is_none() && spans.iter().any(|(shadow, ..)| shadow.carded()) {
        *reserve = take_reserve();
    }
    let mut room = crate::shadow::Room::new(reserve.is_some());
    spans.iter().any(|(shadow, from, to)| shadow.merge_under_writes(*from, *to - *from, &mut room))
}

/// A fence signal on a queue ([`late`]): goes behind the flights in the
/// air, if there are any, and is set when they are settled. Whether it
/// did; if not the caller sets the fence now.
pub(crate) fn late_signal(fence: usize, value: u64) -> bool {
    if !late() {
        return false;
    }
    let mut line = line();
    if line.line.is_empty() && !line.settling {
        return false;
    }
    line.line.push(Late::Signal(fence, value));
    LATE_STATS[14].fetch_add(1, Ordering::Relaxed);
    drop(line);
    // A signal alone (behind a settle under way) has no flight to wake
    // the helper thread for it.
    LINE_MOVED.notify_one();
    RELAY_MOVED.notify_one();
    true
}

/// A queue stands at `wait`, which memory does not meet now ([`late`]):
/// whether it is met once the flights in the air have run. A flight's
/// bottom-of-pipe write of a value that meets it says yes without a wait
/// for the card: the part behind the wait goes to the card behind that
/// flight, which is the order the wait asks for. If the flights may write
/// the value in a way not known here (shaders, a timestamp, another
/// width), they are settled and memory is asked again.
pub(crate) fn late_met(wait: &crate::list::ValueWait) -> bool {
    let (address, bytes) = wait.place();
    {
        let line = line();
        if line.line.is_empty() && line.in_hand.is_empty() {
            return false;
        }
        let mut unsure = touches(&line.written, address, address + bytes);
        // (With the relay: first the writes of flights being landed.)
        let writes = line.in_hand.iter().map(|(at, what, n)| (at, what, n)).chain(line.line.iter().filter_map(|item| match item {
            Late::Write(at, what, n) => Some((at, what, n)),
            _ => None,
        }));
        for (at, what, n) in writes {
            if *at >= address + bytes || address >= *at + *n {
                continue;
            }
            match what {
                crate::queue::Written::Value(v) if *at == address && *n >= bytes => {
                    if wait.holds_for(*v) {
                        LATE_STATS[13].fetch_add(1, Ordering::Relaxed);
                        return true;
                    }
                }
                _ => unsure = true,
            }
        }
        if !unsure {
            return false;
        }
    }
    settle_now(WHY_VALUE);
    wait.met()
}

/// Settles the flights in the air, if any ([`Host::settle`]), for reason
/// `why` (one of the `WHY_` places). Not from inside a submission.
pub(crate) fn settle_now(why: usize) {
    if !late() {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
    host.settle(&mut submitted, why);
}

/// [`settle_now`] at a present, and for a fence the title blocks on.
pub(crate) fn settle_for_present() {
    settle_now(WHY_SYNC);
}
pub(crate) fn settle_for_fence() {
    settle_now(WHY_FENCE);
}

/// Resets the host lists of settled flights and keeps them for reuse, as
/// a queue does for the lists it ran (`crate::list::recycle`).
fn land() {
    let landed = std::mem::take(&mut line().landed);
    for (lists, order) in landed {
        crate::list::recycle(lists, &order);
    }
}

/// The thread that settles the flights once the card has run them all
/// ([`late`]): without it a bottom-of-pipe write would reach the title only
/// at the next call that settles, and a title thread that polls such a
/// value while the submitting thread waits for that thread would never
/// see it.
fn late_helper() {
    above_normal();
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        tracing::warn!("no event for the thread that settles submissions sent ahead");
        return;
    };
    loop {
        let target = {
            let mut line = line();
            loop {
                if line.poke {
                    line.poke = false;
                    break None;
                }
                // (With the relay the flights are another thread's.)
                if !relay() && !line.line.is_empty() && !line.settling {
                    break Some(line.target);
                }
                line = LINE_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(target) = target else {
            land();
            crate::queue::release_held(false);
            continue;
        };
        // The card first, without the submission lock: the submitting
        // thread prepares and sends meanwhile.
        // SAFETY: our fence and event.
        unsafe {
            if host.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        {
            let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            // Something newer went up meanwhile: the card is waited for
            // again, the submitting thread is not held up for it.
            if line().target != target {
                continue;
            }
            host.settle(&mut submitted, WHY_CARD_DONE);
        }
        land();
        crate::queue::release_held(false);
    }
}

/// The threads of [`late`] wake the title's threads that wait for the card
/// and land what it ran: work on the frame's path. At the normal priority
/// the title leaves its workers at, they took turns for a core with every
/// other program's threads; a capture with all 32 cores busy (builds) had
/// the settling thread ready but not running for 542 ms of 580, while the
/// card and the title waited. One step above normal (the title's submitting
/// thread runs higher still); `VICTORY_HELPER_PRIORITY=0` keeps them normal.
fn above_normal() {
    if std::env::var_os("VICTORY_HELPER_PRIORITY").is_some_and(|v| v == "0") {
        return;
    }
    // SAFETY: a plain call on our own thread.
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
        if let Err(e) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) {
            tracing::warn!(error = %e, "a helper thread stays at normal priority");
        }
    }
}

/// Starts the threads of [`late`], once: the one that settles flights when
/// the card has run them and looks at what queues hold, and with [`relay`]
/// the one that lands flights.
fn late_threads() {
    static HELPER: std::sync::Once = std::sync::Once::new();
    HELPER.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("victory-late-wait".into()).spawn(late_helper) {
            tracing::warn!(error = %e, "no thread to settle submissions sent ahead: they are settled by the calls that follow");
        }
        tracing::info!(
            most = late_most(),
            writes = late_writes(),
            "submissions are sent to the card without a wait for each (VICTORY_LATE_WAIT=1)"
        );
        if relay() {
            // Without it nothing would ever land.
            if let Err(e) = std::thread::Builder::new().name("victory-relay".into()).spawn(relay_worker) {
                fail(format_args!("no thread to land submissions sent ahead (VICTORY_RELAY=1): {e}"));
            }
            tracing::info!(
                exact = relay_exact(),
                "what shaders wrote is copied back by a thread of its own, beside the submitting thread (VICTORY_RELAY=1)"
            );
        }
    });
}

/// With [`relay`]: waits until `done` says so of the line, looked at each
/// time flights have landed. Counted as a settle for `why`, and its time
/// as waiting for the card. The caller may hold the submission lock: the
/// thread that lands flights never takes it.
fn relay_wait(why: usize, done: impl Fn(&Line) -> bool) {
    let mut line = line();
    if done(&line) {
        return;
    }
    LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
    LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
    let was_working = working(false);
    let waiting = std::time::Instant::now();
    while !done(&line) {
        line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
    }
    drop(line);
    stood(waiting);
    working(was_working);
    let us = waiting.elapsed().as_micros() as u64;
    LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
    LATE_WAITED[why.min(10)].fetch_add(us, Ordering::Relaxed);
    SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
}

/// With [`relay_exact`], the wait of a submission that found the compare's
/// results low (`Host::card_results_low`, now `low`) is over once they are
/// not, as flights land and give their runs back; or once nothing is in
/// the air or being landed, where a settle's would be over: a plan never
/// done with keeps them low for good ([`CardOverdue`]). The room left may
/// still not hold the next plan's run (an eighth of the results, in two
/// parts if it goes round): that plan then compares nothing, and its
/// landing fetches, as when the room ran out before.
///
/// Not a wait until the plan's own run fits ([`CardAreas::take`]): its
/// pieces are known only inside `Host::card_before`, from the declared
/// ranges joined per copy and the copies hashed for the first time, under
/// the queue's order and the compare's lock, which the thread that lands
/// flights takes too; a wait there would stop it. Counting them before
/// would go over every list's steps once more per submission, for plans
/// past half the room left (16,000 pieces and more, a GiB of declared
/// ranges in one submission) where a frame's plans held some 14,000
/// dwords in all.
fn results_wait_over(low: bool, line: &Line) -> bool {
    !low || line.line.is_empty() && !line.settling
}

/// What the thread that lands flights fetches card copies' pieces with
/// ([`relay`]): a list, a fence and an event of its own (the host's are
/// behind the submission lock), and the memory the pieces come down to.
struct RelayFetch {
    list: Prelude,
    fence: ID3D12Fence,
    event: HANDLE,
    /// The last value signalled on `fence`.
    sent: u64,
    /// Buffer, mapping, bytes.
    buffer: Option<(ID3D12Resource, usize, u64)>,
}

/// The thread that lands flights ([`relay`]): as soon as the card has run
/// the first flights of the line it takes them, with what follows them,
/// and does what they left to do ([`Host::relay_land`]); else it waits for
/// the card, or for a flight. It takes neither the submission lock nor
/// anything a caller of [`Host::settle`] may hold: resetting the landed
/// lists and looking at what queues hold are left to [`late_helper`].
fn relay_worker() {
    above_normal();
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        fail(format_args!("no event for the thread that lands submissions sent ahead (VICTORY_RELAY=1)"));
    };
    let mut fetch: Option<RelayFetch> = None;
    loop {
        let next = {
            let mut line = line();
            loop {
                if line.line.is_empty() {
                    line = RELAY_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let fences: Vec<Option<u64>> = line
                    .line
                    .iter()
                    .map(|l| match l {
                        Late::Run(flight) => Some(flight.fence),
                        _ => None,
                    })
                    .collect();
                // SAFETY: a plain query.
                let completed = unsafe { host.fence.GetCompletedValue() };
                let n = landable(&fences, completed);
                if n == 0 {
                    // The first thing is a flight the card has not run.
                    break Err(fences[0].unwrap_or(0));
                }
                line.settling = true;
                let items: Vec<Late> = line.line.drain(..n).collect();
                line.in_hand = items
                    .iter()
                    .filter_map(|l| match l {
                        Late::Write(at, what, bytes) => Some((*at, *what, *bytes)),
                        _ => None,
                    })
                    .collect();
                break Ok(items);
            }
        };
        match next {
            Ok(items) => host.relay_land(items, &mut fetch),
            Err(target) => {
                let waiting = std::time::Instant::now();
                // SAFETY: the host's fence and this thread's event.
                let waited = unsafe {
                    host.fence.SetEventOnCompletion(target, event).map(|()| {
                        WaitForSingleObject(event, INFINITE);
                    })
                };
                if waited.is_err() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
    }
}

/// [`relay`]: what shaders wrote to the declared ranges `back` of copies
/// in the PC's memory goes into title memory. As [`hand_back`], each byte
/// once, the pieces that differ found on several threads; but the merge is
/// `Shadow::relay_merge`, which does not write the copy, and the ranges
/// are not done with here (each flight's are, afterwards).
fn relay_hand_back(back: &[Declared]) {
    let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for (shadow, declared) in back {
        if shadow.carded() {
            continue;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        join_ranges(&mut spans, declared.iter().copied());
        for &(from, to) in &spans {
            shadow.relay_compared(from, to - from);
        }
        work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
    }
    if work.is_empty() {
        return;
    }
    #[cfg(feature = "diagnostics")]
    let scanning = std::time::Instant::now();
    // Looked through without a lock: a piece that changes meanwhile (a
    // later flight writing, the title's bytes going in) is found now or by
    // that flight's landing, and the merge looks again under the lock.
    let found = crate::shadow::find_all(&work);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
    let (mut now, mut title) = (Vec::new(), Vec::new());
    for ((shadow, from, to), found) in work.iter().zip(found) {
        let mut written = 0;
        for (at, n) in found {
            written += shadow.relay_merge(at, n, &mut now, &mut title);
        }
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::compared(*from, to - from, written);
        let _ = (from, to);
    }
}

/// For the frame-rate line, with [`late`]: what the line of flights did
/// per frame since the last one.
fn late_line(frames: f64) {
    if !late() {
        return;
    }
    let s: Vec<f64> = LATE_STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "submissions sent ahead per frame: {:.1} in {:.1} settles ({:.1} flights each time), {:.1} ms waiting for the card in them; \
         settled for: a synchronous submission or present {:.1}, the card had run them {:.1}, a wait for a memory value {:.1}, \
         title memory changed under shader writes {:.1}, title memory read {:.1} (by a buffer copy {:.1}, a fill of upload memory {:.1}, \
         a texture {:.1}), a step of ours {:.1}, {} lists in the air {:.1}, \
         bottom-of-pipe writes (VICTORY_LATE_WAIT_WRITES=0) {:.1}, a blocking fence wait {:.1}; \
         {:.1} waits for memory values met by a flight's bottom-of-pipe write, {:.1} fence signals set behind flights",
        s[0],
        s[1],
        if s[1] > 0.0 { s[11] / s[1] } else { 0.0 },
        s[12] / 1000.0,
        s[WHY_SYNC],
        s[WHY_CARD_DONE],
        s[WHY_VALUE],
        s[WHY_STALE],
        s[WHY_READ],
        s[15],
        s[16],
        s[17],
        s[WHY_RUN],
        late_most(),
        s[WHY_MOST],
        s[WHY_WRITES],
        s[WHY_FENCE],
        s[13],
        s[14],
    );
    {
        let w: Vec<f64> = LATE_WAITED.iter().map(|w| w.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames).collect();
        tracing::info!(
            "of the waiting for the card in settles, per frame, by what settled: a synchronous submission or present {:.1} ms, \
             the card had run them {:.1}, a wait for a memory value {:.1}, title memory changed under shader writes {:.1}, \
             title memory read {:.1}, a step of ours {:.1}, lists in the air {:.1}, bottom-of-pipe writes {:.1}, a blocking fence wait {:.1}",
            w[WHY_SYNC],
            w[WHY_CARD_DONE],
            w[WHY_VALUE],
            w[WHY_STALE],
            w[WHY_READ],
            w[WHY_RUN],
            w[WHY_MOST],
            w[WHY_WRITES],
            w[WHY_FENCE],
        );
    }
    if relay() {
        let r: Vec<f64> = RELAY_STATS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "the relay per frame (VICTORY_RELAY=1): {:.1} flights landed in {:.1} landings by the thread that copies back, \
             beside the submitting thread; that thread waited {:.1} ms for the card with nothing to land and {:.1} ms for \
             fetches of card copies' pieces, and was {:.1} ms landing in all (the frame line's \"copying back\" is its time; \
             \"settles\" above are waits of other threads for it)",
            r[1],
            r[0],
            r[2] / 1000.0,
            r[3] / 1000.0,
            r[4] / 1000.0,
        );
    }
    if relay_snapshot() {
        let [ring, fetched, pieces, bytes, older, crowded, unslotted] =
            SNAPSHOT_STATS.each_ref().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames);
        tracing::info!(
            "pieces copied by the compare in the queue's order, per frame (VICTORY_RELAY_SNAPSHOT=1): {ring:.1} landings merged \
             from the ring, {fetched:.1} that fetched (a piece without a slot, or a range not compared); {pieces:.0} pieces, \
             {:.1} MB from the ring; {older:.1} pieces left out, a fetch having merged newer bytes of them; {unslotted:.1} \
             pieces found differing past the ring's limit (no slot), {crowded:.2} plans that compared nothing (no room in the \
             compare's results)",
            bytes / 1e6
        );
    }
    if late_exact() {
        let [pages, runs, bytes] = crate::shadow::merged_stats().map(|n| n as f64 / frames);
        let [unwatched, unknown, over, over_runs, unsnapped, over_patches] =
            crate::shadow::unmerged_stats().map(|n| n as f64 / frames);
        let [patches, patched, patched_bytes, copies, no_memory] =
            std::array::from_fn(|i| CARD_PATCHED[i].swap(0, Ordering::Relaxed) as f64 / frames);
        tracing::info!(
            "title bytes put in under flights' shader writes per frame (VICTORY_LATE_WAIT=2): {pages:.0} pages looked at, \
             {runs:.0} changed runs, {:.2} MB; not put in, a settle due, for: a copy not write-watched {unwatched:.1}, \
             title writes not known {unknown:.1}, more runs of {CARD_PATCH_RUN} bytes or more than the {CARD_COPIES_MOST} \
             copies a submission sends up {over:.1}, more short runs than its {} MiB of patches {over_patches:.1} (the page \
             past a limit had {over_runs:.0} runs), a copy without its snapshot {unsnapped:.1}; sent up to copies in the \
             card's memory: {patches:.0} dword patches with {:.2} MB of title bytes to {patched:.1} copies, {copies:.0} \
             copies of ranges; {no_memory:.2} buffers of patches without memory",
            bytes / (1 << 20) as f64,
            CARD_PATCH_BYTES_MOST >> 20,
            patched_bytes / (1 << 20) as f64
        );
    }
}

#[cfg(test)]
mod patch_tests {
    use super::{
        CARD_PATCH_BYTES_MOST, PATCH_LEAST, PATCH_POOL_AGE, PATCH_POOL_MOST, PatchPool, REHASH_JOIN_FROM, Rehash, SendCopy, plan_send,
        rehash_joined, rehash_runs,
    };
    use std::cell::Cell;
    use std::sync::Mutex;

    /// Buffers of patches kept for later plans: a plan gets the smallest
    /// that holds its patches within the window it is given (four times
    /// the buffer one made for them would be, [`PatchPool::for_plan`]), so
    /// a few patches do not pin the 16 MiB the merge's room took; the pool
    /// keeps at most 64 MiB, the buffers given back last, and lets go of
    /// one no plan took for 256 plans.
    #[test]
    fn the_pool_gives_the_smallest_buffer_that_holds_and_lets_go_of_old_ones() {
        const K: u64 = 1 << 10;
        let mut pool = PatchPool::<u32>::default();
        pool.give_back(1, 64 * K);
        pool.give_back(2, 256 * K);
        pool.give_back(3, 16 << 20);
        pool.give_back(4, 128 * K);
        assert_eq!(pool.take(100 * K, 4 * 128 * K), Some((4, 128 * K)), "the smallest of 128 and 256 KiB");
        assert_eq!(pool.take(100 * K, 4 * 128 * K), Some((2, 256 * K)));
        assert_eq!(pool.take(300 * K, 4 * 512 * K), None, "16 MiB for 300 KiB of patches");
        assert_eq!(pool.take(300 * K, u64::MAX), Some((3, 16 << 20)));
        pool.give_back(5, 1 << 20);
        assert_eq!(pool.take_biggest(), Some((5, 1 << 20)));
        assert_eq!(pool.take_biggest(), Some((1, 64 * K)));
        assert_eq!(pool.take_biggest(), None);
        // At most 64 MiB, the last given back first.
        for i in 0..10 {
            pool.give_back(10 + i, 16 << 20);
        }
        assert_eq!(pool.bytes(), PATCH_POOL_MOST);
        let mut kept: Vec<u32> = std::iter::from_fn(|| pool.take_biggest().map(|b| b.0)).collect();
        kept.sort_unstable();
        assert_eq!(kept, [16, 17, 18, 19]);
        // One no plan took for 256 plans goes; one given back since stays.
        pool.give_back(30, 64 * K);
        for _ in 0..PATCH_POOL_AGE - 1 {
            pool.plan();
        }
        pool.give_back(31, 64 * K);
        assert_eq!(pool.bytes(), 128 * K);
        pool.plan();
        assert_eq!(pool.take_biggest(), Some((31, 64 * K)));
        assert_eq!(pool.bytes(), 0);
    }

    /// Review: `Host::patch_reserve` and `Host::patch_buffer` (the pool's
    /// `reserve` and `for_plan`), for 8 submissions in the air whose plans
    /// take 2.5 MiB of patches each (about 218 thousand dwords; the bus
    /// scene put in 20-80 thousand runs a frame). The reserve went back to
    /// the pool first and lay in the window a plan takes from ([bytes, 4 *
    /// next power of two]), so each plan took the 16 MiB reserve and the
    /// next submission made a new one: 128 MiB held and made. Chosen before
    /// the reserve goes back, each plan makes 4 MiB and the one reserve
    /// goes round: 32 MiB held, 48 made.
    #[test]
    fn rv_a_plan_of_a_few_mib_does_not_take_the_16_mib_reserve() {
        let most = CARD_PATCH_BYTES_MOST as u64;
        let bytes: u64 = 5 << 19;
        let want = bytes.next_power_of_two().max(PATCH_LEAST);
        let pool = Mutex::new(PatchPool::<u32>::new());
        let (made, names) = (Cell::new(0u64), Cell::new(0u32));
        let make = |size: u64| {
            made.set(made.get() + size);
            names.set(names.get() + 1);
            Some(names.get())
        };
        let mut in_air: Vec<(u32, u64)> = Vec::new();
        for _ in 0..8 {
            // patch_reserve, before the merge; patch_buffer, for the plan.
            let reserve = PatchPool::reserve(&pool, most, make);
            in_air.push(PatchPool::for_plan(&pool, bytes, reserve, make).expect("a buffer"));
            pool.lock().unwrap().plan();
        }
        let pinned: u64 = in_air.iter().map(|b| b.1).sum();
        assert_eq!(
            (pinned >> 20, made.get() >> 20),
            ((8 * want) >> 20, (most + 8 * want) >> 20),
            "MiB held by the plans in the air, MiB made"
        );
        assert_eq!(pool.lock().unwrap().bytes(), most, "the reserve is back, alone");
    }

    /// The same over the window that held the reserve, plans of 2 to 8 MiB
    /// of patches: each of eight in the air has a buffer made for it (the
    /// next power of two of its bytes), never the reserve, and one reserve
    /// goes round.
    #[test]
    fn plans_of_two_to_eight_mib_leave_the_reserve_alone() {
        const M: u64 = 1 << 20;
        let most = CARD_PATCH_BYTES_MOST as u64;
        for bytes in [2 * M + 12, 5 * M / 2, 3 * M, 4 * M, 4 * M + 12, 6 * M, 8 * M] {
            let want = bytes.next_power_of_two().max(PATCH_LEAST);
            let pool = Mutex::new(PatchPool::<u32>::new());
            let (made, names) = (Cell::new(0u64), Cell::new(0u32));
            let make = |size: u64| {
                made.set(made.get() + size);
                names.set(names.get() + 1);
                Some(names.get())
            };
            let (mut reserves, mut in_air) = (Vec::new(), Vec::new());
            for _ in 0..8 {
                let reserve = PatchPool::reserve(&pool, most, make).expect("a reserve");
                reserves.push(reserve.0);
                in_air.push(PatchPool::for_plan(&pool, bytes, Some(reserve), make).expect("a buffer"));
                pool.lock().unwrap().plan();
            }
            assert!(in_air.iter().all(|b| b.1 == want && !reserves.contains(&b.0)), "{bytes} bytes: {in_air:?}");
            assert_eq!(made.get(), most + 8 * want, "{bytes} bytes: one reserve and eight plans made");
            reserves.dedup();
            assert_eq!(reserves.len(), 1, "{bytes} bytes: one reserve goes round");
        }
    }

    /// A plan's buffer is chosen before the submission's reserve goes back
    /// ([`PatchPool::for_plan`]): one kept in its window, else one made;
    /// without memory the reserve if it holds the patches, else the biggest
    /// of the reserve and those kept; and nothing without either. The
    /// reserve goes back unless taken.
    #[test]
    fn a_plans_buffer_is_chosen_before_the_reserve_goes_back() {
        const M: u64 = 1 << 20;
        let most = CARD_PATCH_BYTES_MOST as u64;
        let none = |_: u64| -> Option<u32> { None };
        let made = |size: u64| -> Option<u32> { Some(1000 + (size >> 20) as u32) };
        let kept = |pool: &Mutex<PatchPool<u32>>| {
            let mut sizes: Vec<(u32, u64)> = pool.lock().unwrap().free.iter().map(|b| (b.0, b.1)).collect();
            sizes.sort_unstable();
            sizes
        };
        // A kept buffer in the window ([3 MiB, 16 MiB] for 3 MiB): the
        // smallest; the reserve goes back.
        let pool = Mutex::new(PatchPool::new());
        pool.lock().unwrap().give_back(1, 8 * M);
        pool.lock().unwrap().give_back(2, 5 * M);
        assert_eq!(PatchPool::for_plan(&pool, 3 * M, Some((9, most)), made), Some((2, 5 * M)));
        assert_eq!(kept(&pool), [(1, 8 * M), (9, most)]);
        // None in the window: one made, of the next power of two.
        let pool = Mutex::new(PatchPool::new());
        pool.lock().unwrap().give_back(1, 32 * M);
        assert_eq!(PatchPool::for_plan(&pool, 3 * M, Some((9, most)), made), Some((1004, 4 * M)));
        assert_eq!(kept(&pool), [(1, 32 * M), (9, most)]);
        // No memory: the reserve, which holds them.
        let pool = Mutex::new(PatchPool::new());
        pool.lock().unwrap().give_back(1, 32 * M);
        assert_eq!(PatchPool::for_plan(&pool, 3 * M, Some((9, most)), none), Some((9, most)));
        assert_eq!(kept(&pool), [(1, 32 * M)]);
        // No memory and more patches than the reserve holds: the biggest
        // kept, if bigger (none holds them: the rest go by copies); the
        // reserve goes back.
        let pool = Mutex::new(PatchPool::new());
        pool.lock().unwrap().give_back(1, 18 * M);
        assert_eq!(PatchPool::for_plan(&pool, 20 * M, Some((9, most)), none), Some((1, 18 * M)));
        assert_eq!(kept(&pool), [(9, most)]);
        // The reserve, if it is the biggest; what was kept stays.
        let pool = Mutex::new(PatchPool::new());
        pool.lock().unwrap().give_back(1, 2 * M);
        assert_eq!(PatchPool::for_plan(&pool, 20 * M, Some((9, most)), none), Some((9, most)));
        assert_eq!(kept(&pool), [(1, 2 * M)]);
        // Neither memory nor anything kept or reserved: none.
        let pool = Mutex::new(PatchPool::new());
        assert_eq!(PatchPool::for_plan(&pool, 3 * M, None, none), None);
        // A few patches: 64 KiB at least, and the reserve back.
        let pool = Mutex::new(PatchPool::new());
        assert_eq!(PatchPool::for_plan(&pool, 300, Some((9, most)), |size| Some(size as u32)), Some((PATCH_LEAST as u32, PATCH_LEAST)));
        assert_eq!(kept(&pool), [(9, most)]);
        // The reserve: kept if there is one of its size or more, else made.
        assert_eq!(PatchPool::reserve(&pool, most, none), Some((9, most)));
        assert_eq!(PatchPool::reserve(&pool, most, made), Some((1016, most)));
        assert_eq!(PatchPool::reserve(&pool, most, none), None);
    }

    /// Ranges of three copies, offsets in order and apart, as `card_dirty`
    /// takes them, and the CPU side's bytes of each.
    fn three_copies() -> ([super::ByteRanges; 3], [Vec<u8>; 3]) {
        use super::CARD_PATCH_RUN;
        // Copy 0: short ranges (dwords 1, 1 and 64: 255 bytes from byte 1
        // of a dword) and a long one. Copy 1: short ranges of a copy that
        // takes no patches. Copy 2: nothing sent, its hashes just made.
        let ranges = [
            vec![(0, 3), (8, 8 + CARD_PATCH_RUN + 44), (400, 403), (1001, 1256)],
            vec![(0, 1), (16, 20)],
            vec![],
        ];
        let data = [0u8, 1, 2].map(|c| (0..4096u32).map(|i| (i * 7 + u32::from(c) * 31 + 5) as u8).collect());
        (ranges, data)
    }

    fn copies_of<'a>(ranges: &'a [Vec<(usize, usize)>; 3]) -> [SendCopy<'a>; 3] {
        let span = |c: usize| (0x10_0000 * (c + 1) + 0x1f00, 4096 + 65536 * c);
        [
            SendCopy {
                ranges: &ranges[0],
                patchable: true,
                span: span(0),
                rehash: Rehash::Ranges,
            },
            SendCopy {
                ranges: &ranges[1],
                patchable: false,
                span: span(1),
                rehash: Rehash::No,
            },
            SendCopy {
                ranges: &ranges[2],
                patchable: true,
                span: span(2),
                rehash: Rehash::Whole,
            },
        ]
    }

    /// A send is planned before the queue's order and the card's work are
    /// taken (`plan_send`): the short ranges of a copy that takes patches
    /// go as patches, in a buffer asked once for all of them, the rest by
    /// copies; each copy's patches lie where `patched_at` says; and the
    /// runs of pieces hashed again are joined with the patch (none for a
    /// copy without hashes, all of one whose hashes were just made).
    #[test]
    fn a_send_is_planned_before_the_locks() {
        let (ranges, data) = three_copies();
        let copies = copies_of(&ranges);
        let asked = Cell::new(0u64);
        let plan = plan_send(
            &copies,
            true,
            |bytes| {
                asked.set(bytes);
                Some((7u32, bytes))
            },
            |_| panic!("the buffer holds patches"),
            |i, short, out| crate::shadow::dword_patches(short, &data[i], out),
        );
        assert_eq!(asked.get(), (1 + 1 + 64) * 12, "one buffer for every short range's dwords");
        assert_eq!(plan.splits[0], (vec![ranges[0][1]], vec![ranges[0][0], ranges[0][2], ranges[0][3]]));
        assert_eq!(plan.splits[1], (ranges[1].clone(), vec![]), "no patches for a copy that takes none");
        assert_eq!(plan.splits[2], (vec![], vec![]));
        let mut wanted = Vec::new();
        crate::shadow::dword_patches(&plan.splits[0].1, &data[0], &mut wanted);
        assert_eq!(plan.patches, wanted);
        assert_eq!(plan.patched_at, [0..wanted.len(), wanted.len()..wanted.len(), wanted.len()..wanted.len()]);
        assert_eq!(plan.buffer, Some((7, 66 * 12)));
        assert_eq!(plan.hashes[0], crate::shadow::card_hash_runs(copies[0].span, &ranges[0], false, true));
        assert!(plan.hashes[0].len() < ranges[0].len(), "joined with the patch");
        assert!(plan.hashes[1].is_empty(), "a copy without hashes");
        assert_eq!(plan.hashes[2], crate::shadow::card_hash_runs(copies[2].span, &[], true, false));
        // Without the patch: one run per range (unless the switch joins them).
        let plan = plan_send(&copies, false, |bytes| Some((7u32, bytes)), |_| {}, |i, short, out| {
            crate::shadow::dword_patches(short, &data[i], out)
        });
        assert_eq!(plan.hashes[0], crate::shadow::card_hash_runs(copies[0].span, &ranges[0], false, rehash_joined(false, 4)));
    }

    /// A buffer that holds fewer patches than the short ranges make (there
    /// was no memory for more) takes as many as it holds, and the short
    /// ranges past it go by copies; with no buffer at all every range goes
    /// by a copy. A buffer no patch went in goes back. No short range, no
    /// buffer asked for.
    #[test]
    fn short_ranges_past_the_plans_buffer_go_by_copies() {
        let (ranges, data) = three_copies();
        let copies = copies_of(&ranges);
        let patches_of = |i: usize, short: &[(usize, usize)], out: &mut Vec<[u32; 3]>| crate::shadow::dword_patches(short, &data[i], out);
        // Room for two dwords: the first two short ranges.
        let plan = plan_send(&copies, true, |_| Some((7u32, 2 * 12)), |_| panic!("it holds patches"), patches_of);
        assert_eq!(plan.splits[0], (vec![ranges[0][1], ranges[0][3]], vec![ranges[0][0], ranges[0][2]]));
        assert!(plan.patches.len() * 12 <= 2 * 12, "{} patches in a buffer of 2", plan.patches.len());
        assert_eq!(plan.buffer, Some((7, 24)));
        // No buffer: all by copies.
        let plan = plan_send(&copies, true, |_| None::<(u32, u64)>, |_| panic!("no buffer"), patches_of);
        assert_eq!(plan.splits[0], (ranges[0].clone(), vec![]));
        assert!(plan.patches.is_empty() && plan.buffer.is_none());
        // A buffer no patch went in (the ranges were cut away) goes back.
        let given = Cell::new(None);
        let plan = plan_send(&copies, true, |bytes| Some((7u32, bytes)), |b| given.set(Some(b)), |_, _, _| {});
        assert_eq!((plan.buffer, given.get()), (None, Some((7, 66 * 12))));
        // No short range: no buffer asked for.
        let long = [vec![ranges[0][1]], vec![], vec![]];
        let plan = plan_send(&copies_of(&long), true, |_| -> Option<(u32, u64)> { panic!("no patches") }, |_| {}, patches_of);
        assert_eq!(plan.splits[0], (long[0].clone(), vec![]));
    }

    /// The pieces hashed again after ranges went up are hashed in joined
    /// runs with patches, or with more than 512 ranges, whatever
    /// `VICTORY_CARD_REHASH_RUNS` says; else as it says.
    #[test]
    fn rehash_runs_join_with_patches_or_many_ranges() {
        assert!(rehash_joined(true, 1));
        assert!(rehash_joined(false, REHASH_JOIN_FROM + 1));
        assert_eq!(rehash_joined(false, REHASH_JOIN_FROM), rehash_runs());
    }
}

/// Copies in the card's memory through the host's own path, on the host
/// GPU (the software adapter where there is no card): title bytes put in
/// under flights' writes (`put_in_under_flights`, `Shadow::merge_under_writes`),
/// the sends that take them up by copies and patches and hash their pieces
/// again (`Host::card_before`, numbered in the queue's order with the
/// relay), what the card side then holds, and a merge of the relay that
/// leaves out the title bytes a fetch ran ahead of (`Shadow::card_merge_relay`).
#[cfg(test)]
mod card_path_tests {
    use super::{
        CARD_PATCHED, CARD_PATCHES, CARD_WORK, CardPlan, Host, PatchBuffer, QUEUE_ORDER, late_exact, put_in_under_flights,
        refreshed_under_flights, stale_under_flights,
    };
    use crate::shadow::{Room, Shadow};
    use crate::watch::PAGE;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::core::Interface;

    /// One at a time: they count the merge's statistics and number sends in
    /// the queue's order, both the process's.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// Title memory as the runtime reserves it: write-watched, on a 64 KiB
    /// line. Never given back: the write watch's journal remembers its
    /// pages as written for good, and memory another test made at the same
    /// address would show them written (`watch`'s own tests expect a fresh
    /// region to show none, and failed so, now and then, while these freed
    /// theirs).
    struct TitleMemory {
        at: *mut u8,
        len: usize,
    }

    impl TitleMemory {
        fn new(len: usize) -> TitleMemory {
            use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
            const MEM_WRITE_WATCH: u32 = 0x20_0000;
            // SAFETY: a fresh private region of our own.
            let at = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) };
            assert!(!at.is_null() && (at as usize).is_multiple_of(PAGE), "write-watched memory on a 64 KiB line");
            TitleMemory { at: at.cast(), len }
        }

        fn address(&self) -> usize {
            self.at as usize
        }

        fn bytes(&self) -> Vec<u8> {
            // SAFETY: our committed region.
            unsafe { std::slice::from_raw_parts(self.at, self.len) }.to_vec()
        }

        fn write(&self, at: usize, bytes: &[u8]) {
            assert!(at + bytes.len() <= self.len);
            // SAFETY: inside our committed region.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.at.add(at), bytes.len()) };
        }
    }

    /// Until the host's queue has run what is on it (a fence of our own).
    fn wait_for_queue(host: &Host) {
        // SAFETY: a fence of our own; a null event makes the call wait.
        unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            host.queue.Signal(&fence, 1).unwrap();
            fence.SetEventOnCompletion(1, HANDLE::default()).unwrap();
        }
    }

    /// The plan's compare runs, as it does after a submission's lists, and
    /// the queue is waited for.
    fn compare_and_wait(host: &Host, plan: &CardPlan) {
        if let Some(check) = &plan.check {
            // SAFETY: a closed list of ours.
            unsafe { host.queue.ExecuteCommandLists(&[Some(check.clone())]) };
        }
        wait_for_queue(host);
    }

    /// The pieces the plan's compare found differing from their hashes,
    /// and whether it compared every declared range.
    fn flagged(plan: &CardPlan) -> (usize, bool) {
        let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let work = work.as_ref().expect("the card's work");
        // SAFETY: the compare's results, which it wrote; it has run.
        let differs = unsafe { std::slice::from_raw_parts(work.differs.1 as *const u32, (work.differs.2 / 4) as usize) };
        let flagged = plan
            .ranges
            .iter()
            .filter(|r| r.compared)
            .map(|r| (0..r.span.pieces).filter(|&p| differs[(r.at + p) as usize] != 0).count())
            .sum();
        (flagged, !plan.ranges.is_empty() && plan.ranges.iter().all(|r| r.compared))
    }

    /// What the card side of `shadow` holds, read back.
    fn card_side(host: &Host, shadow: &Shadow) -> Vec<u8> {
        let (_, _, size) = shadow.card_sides().expect("a copy in the card's memory");
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: a plain read-back buffer.
        unsafe {
            host.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut readback)
                .unwrap();
        }
        let readback = readback.unwrap();
        let (allocator, list) = host.make_prelude().expect("a list");
        // SAFETY: our own list and buffers; the card side's last list has
        // run (its state decayed to common).
        unsafe {
            allocator.Reset().unwrap();
            list.Reset(&allocator, None).unwrap();
            list.CopyBufferRegion(&readback, 0, &shadow.buffer, 0, size);
            list.Close().unwrap();
            host.queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
        }
        wait_for_queue(host);
        let mut data = std::ptr::null_mut();
        // SAFETY: the copy has run; the buffer holds `size` bytes.
        unsafe {
            readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: size as usize }), Some(&mut data)).unwrap();
            std::slice::from_raw_parts(data.cast::<u8>(), size as usize).to_vec()
        }
    }

    /// A copy in the card's memory of four pages, whose shaders write all
    /// of it, through two sends with the relay's numbering:
    ///
    /// - the first takes the copy whole (one range, a copy) and hashes every
    ///   piece; the queue's order goes up by one, `card_dirty` notes the
    ///   range with that number, the card side holds the title's bytes, and
    ///   the compare after it finds every piece as hashed;
    /// - the title then writes pages 1 and 2, which flights' shaders may be
    ///   writing (declared over each other, the higher first): 600 runs of
    ///   3 bytes and two long ones in page 1, a long one in page 2. Without
    ///   a buffer for patches (`Room::new(false)`) page 1's 602 runs are
    ///   past the 512 copies: a settle is due, counted as such, and nothing
    ///   of the page goes in; past the room's patches likewise; with the
    ///   reserve every run goes in, and no settle is due;
    /// - the second send takes them up: 600 dword patches and 3 copies, the
    ///   order one more, the card side the title's bytes, and the compare
    ///   finds the pieces sent to hashed again (the hash runs, made before
    ///   the locks, held them);
    /// - a merge of bytes fetched between the two sends leaves the second
    ///   send's title bytes out, and takes a shader's bytes beside them.
    #[test]
    fn title_bytes_put_in_under_flights_go_up_by_copies_and_patches() {
        let Some(host) = super::host() else {
            return; // no host GPU here
        };
        if !crate::shadow::cards() || !host.card_ready() {
            return;
        }
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // No submission meanwhile: each takes every listed copy's notes.
        let _submitting = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        assert!(CARD_PATCHES.load(Ordering::Relaxed), "the patch compiles");
        const LEN: usize = 4 * PAGE;
        let title = TitleMemory::new(LEN);
        let p0: Vec<u8> = (0..LEN).map(|i| (i * 13 + i / 4093) as u8).collect();
        title.write(0, &p0);
        let shadow = Shadow::on_card_for_test(host, title.address(), LEN, true).expect("a copy in the card's memory");
        let writes = host.new_list().expect("a host list");
        shadow.gpu_writes(&writes, title.address() as u64, LEN as u64);
        writes.close();

        // The first send: the copy took title memory, and goes up whole.
        // (Each send's lists are waited for before anything is checked: a
        // check that fails then drops nothing the card still uses.)
        shadow.refresh_for_test(0, &p0);
        let order = *QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner());
        let plan = host.card_before(&[&writes], false, None, true).expect("a send");
        compare_and_wait(host, &plan);
        assert_eq!((plan.sent_as, *QUEUE_ORDER.lock().unwrap()), (order + 1, order + 1), "a send in the queue's order");
        let (unsent, sent) = shadow.notes_for_test();
        assert!(unsent.is_empty(), "taken");
        assert_eq!(sent.last(), Some(&(order + 1, vec![(0, LEN)])), "noted with the send's number");
        assert_eq!(flagged(&plan), (0, true), "every piece as hashed");
        host.card_done(plan);
        assert!(card_side(host, &shadow) == p0, "the card side holds the title's bytes");
        // Nothing to send, nothing declared: no send, and no number.
        assert!(host.card_before(&[], false, None, true).is_none());
        assert_eq!(*QUEUE_ORDER.lock().unwrap(), order + 1, "only sends that take ranges are numbered");

        // The title writes what flights' shaders may be writing.
        shadow.writing(title.address() + 2 * PAGE - 300, PAGE + 300);
        shadow.writing(title.address() + PAGE, PAGE);
        let mut p1 = p0.clone();
        let mut change = |at: usize, n: usize| p1[at..at + n].iter_mut().for_each(|b| *b ^= 0x5a);
        for i in 0..600 {
            change(PAGE + 64 * i + 1, 3);
        }
        change(PAGE + 40_000, 300);
        change(PAGE + 50_000, 1000);
        change(2 * PAGE + 10, 400);
        title.write(0, &p1);
        let spans = vec![(shadow.clone(), title.address(), title.address() + LEN)];
        // Without patches: past the copies, a settle is due.
        let _ = crate::shadow::unmerged_stats();
        assert!(put_in_under_flights(&spans, &mut None::<()>, || None), "602 runs, 512 copies: a settle is due");
        let [_, _, over_copies, over_runs, _, over_patches] = crate::shadow::unmerged_stats();
        assert_eq!((over_copies, over_runs, over_patches), (1, 602, 0), "counted past the copies");
        assert!(shadow.cpu_side_for_test() == p0, "nothing of the page went in");
        assert!(shadow.notes_for_test().0.is_empty(), "nor was noted");
        // Past the room's patches.
        let mut room = Room::new(true);
        room.patches = Some(10);
        assert!(shadow.merge_under_writes(title.address(), LEN, &mut room), "past the patches: a settle is due");
        let [_, _, over_copies, _, _, over_patches] = crate::shadow::unmerged_stats();
        assert_eq!((over_copies, over_patches), (0, 1), "counted past the patches");
        // With the reserve: all of it goes in.
        let mut reserve: Option<PatchBuffer> = None;
        assert!(!put_in_under_flights(&spans, &mut reserve, || host.patch_reserve()), "no settle");
        assert!(reserve.is_some(), "the buffer for the patches was taken first");
        assert!(shadow.cpu_side_for_test() == p1, "the title's bytes are in the CPU side");
        assert_eq!(shadow.notes_for_test().0.len(), 603, "and noted, run by run");

        // The second send.
        let before = CARD_PATCHED.each_ref().map(|c| c.load(Ordering::Relaxed));
        let plan = host.card_before(&[&writes], false, reserve, true).expect("a send");
        compare_and_wait(host, &plan);
        let after = CARD_PATCHED.each_ref().map(|c| c.load(Ordering::Relaxed));
        assert_eq!((after[0] - before[0], after[3] - before[3]), (600, 3), "600 dword patches, 3 copies");
        assert!(plan.patches.is_some(), "in the plan's buffer");
        assert_eq!((plan.sent_as, *QUEUE_ORDER.lock().unwrap()), (order + 2, order + 2), "one more send");
        let (unsent, sent) = shadow.notes_for_test();
        assert!(unsent.is_empty());
        assert_eq!(sent.last().map(|s| (s.0, s.1.len())), Some((order + 2, 603)), "noted with its number");
        assert_eq!(flagged(&plan), (0, true), "the pieces sent to were hashed again");
        host.card_done(plan);
        assert!(card_side(host, &shadow) == p1, "the card side holds the title's new bytes");

        // A fetch that ran between the two sends: the card held the first
        // send's bytes there, and a flight's shader wrote beside them.
        let mut fetched = p0.clone();
        for i in 0..100 {
            fetched[PAGE + 64 * i + 33] ^= 0xa5;
        }
        let mut scratch = Vec::new();
        assert!(shadow.card_merge_relay(PAGE, &fetched[PAGE..2 * PAGE], order + 1, &mut scratch) > 0);
        let mut wanted = p1.clone();
        for i in 0..100 {
            wanted[PAGE + 64 * i + 33] = fetched[PAGE + 64 * i + 33];
        }
        assert!(title.bytes() == wanted, "the second send's title bytes stay, the shader's go in");
        assert!(shadow.cpu_side_for_test() == wanted, "and into the CPU side");
    }

    /// The merge's room starts again for each copy (`Room::next_copy`): a
    /// run of one copy that starts where another copy's long run ends is
    /// not one range with it (`card_dirty` sends each copy's own), and
    /// takes a patch of its own.
    #[test]
    fn the_merge_takes_each_copys_runs_apart() {
        let Some(host) = super::host() else {
            return; // no host GPU here
        };
        if !crate::shadow::cards() || !host.card_ready() {
            return;
        }
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let memories = [TitleMemory::new(PAGE), TitleMemory::new(PAGE)];
        let copies = memories.each_ref().map(|m| Shadow::on_card_for_test(host, m.address(), PAGE, false).expect("a copy"));
        for (copy, memory) in copies.iter().zip(&memories) {
            copy.refresh_for_test(0, &memory.bytes());
            copy.writing(memory.address(), PAGE);
        }
        memories[0].write(700, &[1; 300]);
        memories[1].write(1000, &[2; 3]);
        let mut room = Room::new(true);
        let full = room;
        for (copy, memory) in copies.iter().zip(&memories) {
            assert!(!copy.merge_under_writes(memory.address(), PAGE, &mut room));
        }
        let took = (full.copies - room.copies, full.patches.unwrap() - room.patches.unwrap());
        assert_eq!(took, (1, 1), "a copy for the first copy's run, a patch for the second's");
    }

    /// The copies a submission refreshes while flights' shaders may write
    /// them, and the span refreshed of each; without `late_exact` the
    /// journal says whether a settle is due (it was not asked ahead about
    /// these pages: yes), and no buffer for patches is taken.
    #[test]
    fn stale_under_flights_takes_the_copies_refreshed_under_flights() {
        let Some(host) = super::host() else {
            return; // no host GPU here
        };
        if !crate::shadow::cards() || !host.card_ready() {
            return;
        }
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let memory = TitleMemory::new(2 * PAGE);
        let copy = Shadow::on_card_for_test(host, memory.address(), 2 * PAGE, false).expect("a copy");
        let list = host.new_list().expect("a host list");
        copy.refresh(&list, memory.address() as u64 + 100, 1000);
        copy.refresh(&list, (memory.address() + PAGE) as u64, 500);
        assert!(refreshed_under_flights(&[&list]).is_empty(), "no shader writes it");
        copy.writing(memory.address() + 50, 200);
        let spans = refreshed_under_flights(&[&list]);
        assert_eq!(spans.len(), 1);
        assert!(Arc::ptr_eq(&spans[0].0, &copy));
        assert_eq!((spans[0].1, spans[0].2), (memory.address() + 100, memory.address() + PAGE + 500));
        if !late_exact() {
            let mut reserve: Option<PatchBuffer> = None;
            assert!(stale_under_flights(host, &[&list], &mut reserve), "the journal cannot say: a settle is due");
            assert!(reserve.is_none(), "no buffer for patches without the merge");
        }
        list.close();
    }

    /// Review (timing, not a check): how long a send of the most short
    /// ranges the room takes (1,398,101 ranges of 3 bytes, 8 apart, to a
    /// copy with hashes) holds the queue's order and the card's work, as
    /// threads that wait for each over and over find it; and what its hash
    /// runs cost, which `card_before` made under both before and now makes
    /// with its plan.
    #[test]
    #[ignore = "review timing"]
    fn rv_time_the_locks_a_send_of_the_most_short_ranges_holds() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};
        let Some(host) = super::host() else {
            return; // no host GPU here
        };
        if !crate::shadow::cards() || !host.card_ready() {
            return;
        }
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let _submitting = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        const RANGES: usize = 1_398_101;
        let len = RANGES * 8;
        // A title address the send does not read.
        let start = 0x7_0000_0000usize;
        let shadow = Shadow::on_card_for_test(host, start, len, true).expect("a copy");
        let writes = host.new_list().expect("a host list");
        shadow.gpu_writes(&writes, start as u64, len as u64);
        writes.close();
        // The first send makes the hashes, of every piece.
        let plan = host.card_before(&[&writes], false, None, true).expect("a send");
        wait_for_queue(host);
        host.card_done(plan);
        for i in 0..RANGES {
            shadow.refresh_for_test(i * 8, &[0x5a; 3]);
        }
        let reserve = host.patch_reserve();
        let stop = AtomicBool::new(false);
        let wait_for = |lock: &(dyn Fn() + Sync)| {
            let mut most = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                let asked = Instant::now();
                lock();
                most = most.max(asked.elapsed());
                std::thread::yield_now();
            }
            most
        };
        let (order, work, sent, plan) = std::thread::scope(|s| {
            let order = s.spawn(|| wait_for(&|| drop(QUEUE_ORDER.lock())));
            let work = s.spawn(|| wait_for(&|| drop(CARD_WORK.lock())));
            std::thread::sleep(Duration::from_millis(20));
            let sending = Instant::now();
            let plan = host.card_before(&[&writes], false, reserve, true).expect("a send");
            let sent = sending.elapsed();
            std::thread::sleep(Duration::from_millis(20));
            stop.store(true, Ordering::Relaxed);
            (order.join().unwrap(), work.join().unwrap(), sent, plan)
        });
        wait_for_queue(host);
        assert!(plan.patches.is_some());
        host.card_done(plan);
        let ranges: Vec<(usize, usize)> = (0..RANGES).map(|i| (i * 8, i * 8 + 3)).collect();
        let hashing = Instant::now();
        let runs = crate::shadow::card_hash_runs((start, len), &ranges, false, true);
        let hashing = hashing.elapsed();
        eprintln!(
            "rv timing: a send of {RANGES} short ranges took {sent:?}; a thread waited up to {order:?} for the queue's order and \
             up to {work:?} for the card's work meanwhile; its hash runs ({} of them), made before both now, take {hashing:?}",
            runs.len()
        );
    }
}

#[cfg(test)]
mod late_tests {
    use super::{continues, join_ranges, touches};

    /// Copies join only when both ends continue and the whole does not
    /// overlap itself.
    #[test]
    fn copies_join_only_end_to_end_and_apart() {
        // (destination, source, bytes)
        assert!(continues((1000, 5000, 16), (1016, 5016, 32)));
        assert!(continues((5000, 1000, 16), (5016, 1016, 32)));
        // A gap, or only one side continues.
        assert!(!continues((1000, 5000, 16), (1020, 5016, 32)));
        assert!(!continues((1000, 5000, 16), (1016, 5020, 32)));
        assert!(!continues((1000, 5000, 16), (1000, 5000, 16)));
        // Together they would overlap: the second reads what the first wrote.
        assert!(!continues((1000, 1016, 16), (1016, 1032, 16)));
        assert!(!continues((1016, 1000, 16), (1032, 1016, 16)));
        // They touch without overlapping.
        assert!(continues((1000, 1032, 16), (1016, 1048, 16)));
        assert!(!continues((usize::MAX - 8, 0, 8), (0, 8, 8)));
    }

    /// Ranges are found by what they hold, not by where they start, and a
    /// range that only touches another's end does not touch it.
    #[test]
    fn ranges_touch_where_they_overlap() {
        let mut ranges = Vec::new();
        join_ranges(&mut ranges, [(40, 50), (10, 20), (20, 25), (45, 60), (7, 7)]);
        assert_eq!(ranges, [(10, 25), (40, 60)]);
        join_ranges(&mut ranges, [(25, 40)]);
        assert_eq!(ranges, [(10, 60)]);
        let ranges = [(10usize, 25usize), (40, 60), (100, 101)];
        for (from, to, hit) in [
            (0, 10, false),
            (0, 11, true),
            (24, 25, true),
            (25, 40, false),
            (25, 41, true),
            (59, 200, true),
            (60, 100, false),
            (100, 101, true),
            (101, 500, false),
            (30, 30, false),
            (50, 50, false),
        ] {
            assert_eq!(touches(&ranges, from, to), hit, "{from}..{to}");
        }
        assert!(!touches(&[], 0, usize::MAX));
    }

    /// A texture staging buffer marked in flight for submission N blocks
    /// later flights until `FINISHED` reaches N.
    #[test]
    fn staging_in_flight_tracks_submission_completion() {
        let Some(host) = super::host() else { return };
        let heap_words: [u32; 5] = [1, 0, 0, 1, 1];
        let desc_words: [u32; 14] = [3, 0, 0, 0, 16, 0, 4, 65537, 28, 1, 0, 256, 0, 0];
        // SAFETY: plain repr(C) structs matching D3D12 layouts.
        let (heap, desc) = unsafe {
            (
                std::mem::transmute::<[u32; 5], crate::resource::HeapProperties>(heap_words),
                std::mem::transmute::<[u32; 14], crate::resource::ResourceDesc>(desc_words),
            )
        };
        let iid = crate::object::guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let mut resource = std::ptr::null();
        assert_eq!(
            crate::resource::create_committed(
                std::ptr::null(),
                &heap,
                0,
                &desc,
                0,
                std::ptr::null(),
                &iid,
                &mut resource,
            ),
            crate::object::S_OK
        );
        let tex = crate::resource::host_resource(resource).expect("host texture");
        let list = host.new_list().expect("host list");
        crate::texture::upload(&list, resource, tex);
        assert!(!super::staging_in_flight_at(&[&list], 0));
        for step in &list.deferred().copies {
            if let super::Step::Texture(staging) = step {
                staging.mark_in_flight(10);
            }
        }
        assert!(super::staging_in_flight_at(&[&list], 9));
        assert!(!super::staging_in_flight_at(&[&list], 10));
        assert!(!super::staging_in_flight_at(&[&list], 11));
        list.close();
        drop(list);
        let release = crate::object::slot::<extern "system" fn(crate::object::This) -> u32>(
            resource, 0x10,
        );
        assert_eq!(release(resource), 0);
    }

    /// Only a list that records a copy from a texture's staging buffer notes
    /// it for its flight (`staging_copies`, marked in flight at submission);
    /// a later list that only samples the unchanged texture notes nothing,
    /// so flights that sample the same textures do not wait for each other.
    #[test]
    fn only_lists_that_copy_from_a_staging_buffer_note_it() {
        let Some(host) = super::host() else { return };
        let heap_words: [u32; 5] = [1, 0, 0, 1, 1];
        let desc_words: [u32; 14] = [3, 0, 0, 0, 16, 0, 4, 65537, 28, 1, 0, 256, 0, 0];
        // SAFETY: plain repr(C) structs matching D3D12 layouts.
        let (heap, desc) = unsafe {
            (
                std::mem::transmute::<[u32; 5], crate::resource::HeapProperties>(heap_words),
                std::mem::transmute::<[u32; 14], crate::resource::ResourceDesc>(desc_words),
            )
        };
        let iid = crate::object::guid(0x696442be, 0xa72e, 0x4059, [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad]);
        let mut resource = std::ptr::null();
        assert_eq!(
            crate::resource::create_committed(std::ptr::null(), &heap, 0, &desc, 0, std::ptr::null(), &iid, &mut resource),
            crate::object::S_OK
        );
        let tex = crate::resource::host_resource(resource).expect("host texture");
        let first = host.new_list().expect("host list");
        crate::texture::upload(&first, resource, tex);
        assert_eq!(first.deferred().staging_copies.len(), 1, "the first use records the copy");
        // What its submission does after the copy: uploaded, not dirty.
        for step in &first.deferred().copies {
            if let super::Step::Host(work) = step {
                work();
            }
        }
        let second = host.new_list().expect("host list");
        crate::texture::upload(&second, resource, tex);
        assert!(second.deferred().staging_copies.is_empty(), "a list that only samples copies nothing");
        assert!(
            second.deferred().copies.iter().any(|s| matches!(s, super::Step::Texture(_))),
            "its check at submission stays"
        );
        first.close();
        second.close();
        drop(first);
        drop(second);
        let release = crate::object::slot::<extern "system" fn(crate::object::This) -> u32>(resource, 0x10);
        assert_eq!(release(resource), 0);
    }
}

/// Whether a title buffer copy that continues the one recorded before it
/// in its list becomes one step with it (`VICTORY_JOIN_COPIES=1`; off by
/// default).
///
/// Each copy is done at submission through the kernel, which reports a
/// page that is not there instead of faulting (`copy_mapped`), tells the
/// write watch and the guards, and ends the refreshes gathered so far: a
/// call into the kernel for some 480 bytes, 14,042 times a frame in run 86
/// (6.8 MB; "moves 19 ms" with the refreshes' own reads). How many of them
/// continue the one before is counted either way and said on its own line
/// with the frame rate; the switch is worth that many kernel calls.
///
/// Joined, the same bytes move: [`continues`] joins only copies whose
/// ranges, taken together, do not overlap, and nothing is recorded between
/// two steps that follow each other. What differs: a page that is not
/// mapped now fails the joined copy as a whole (it then goes page by page,
/// zeros for what cannot be read, as a single copy does), where before
/// only the copy that held it did.
fn join_copies() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_JOIN_COPIES").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line: title buffer copies recorded, and those
/// that continue the one before ([`join_copies`]).
static COPIES_JOINED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether the copy `next` (destination, source, bytes) goes on exactly
/// where `last` ended, source and destination both, and the two as one do
/// not overlap (source over destination): then one copy of both moves the
/// bytes the two would move in turn.
fn continues(last: (usize, usize, usize), next: (usize, usize, usize)) -> bool {
    let ((d, s, n), (dst, src, len)) = (last, next);
    let Some(all) = n.checked_add(len) else {
        return false;
    };
    d.checked_add(n) == Some(dst)
        && s.checked_add(n) == Some(src)
        && (d.checked_add(all).is_some_and(|end| end <= s) || s.checked_add(all).is_some_and(|end| end <= d))
}

/// The ranges the shaders of `runs` may write, per host copy, as declared.
/// One list's lock at a time: a submission may name a list twice.
fn declared_writes(runs: &[&HostList]) -> Vec<Declared> {
    let mut back: Vec<Declared> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::GpuWrite(shadow, address, len) = step {
                let range = (*address, *address + *len);
                match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                    Some((_, ranges)) => {
                        if !ranges.contains(&range) {
                            ranges.push(range);
                        }
                    }
                    None => back.push((shadow.clone(), vec![range])),
                }
            }
        }
    }
    back
}

/// The walls trace (`VICTORY_WALLS_TRACE=1` or `2`; off by default): log
/// lines that say what the title's CPU was given while it decided whether
/// the camera is in a room. The values of the small read-backs
/// ([`read_backs`]), the frame's main pass by scissor rectangle with its
/// culling dispatches (`draw.rs`, `walls_census`), the shader-written
/// buffers merged into title memory (`shadow.rs`, `walls_merged`), host
/// copies found stale after their refresh (`shadow.rs`), and, in the
/// runtime, the read requests that fail (with 2: every request).
pub(crate) fn walls_trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WALLS_TRACE").is_some_and(|v| v == "1" || v == "2"))
}

/// After `list` has run: the texels its read-backs copied into host
/// buffers of ours go to the title's memory (`HostList::defer_read_back`).
///
/// A submission that is waited for did this itself. One sent ahead
/// ([`late`]) did not, at any stage: the flights' lists were settled
/// without it, so the title read a buffer nobody wrote, as before read-backs
/// were made (the exposure metering's two small float targets, every frame).
fn read_backs(list: &HostList) {
    for (buffer, at, [row, rows, pitch]) in &list.deferred().backs {
        let mut data = std::ptr::null_mut();
        // SAFETY: a read-back buffer of ours, written by the list that has
        // run; the title's destination holds every row (checked when the
        // copy was recorded).
        unsafe {
            if buffer.Map(0, None, Some(&mut data)).is_ok() && !data.is_null() {
                let from = data.cast::<u8>().cast_const();
                for line in 0..*rows {
                    std::ptr::copy_nonoverlapping(from.add(line * row.next_multiple_of(256)), (*at as *mut u8).add(line * pitch), *row);
                }
                // The walls trace: what the title is handed (one row of at
                // most 24 values: its 5x1 and 1x1 float targets).
                if walls_trace() && *rows == 1 && *row <= 96 {
                    let mut values = [0f32; 24];
                    std::ptr::copy_nonoverlapping(from, values.as_mut_ptr().cast::<u8>(), *row & !3);
                    tracing::info!(
                        frame = crate::frame::current(),
                        to = format_args!("{:#x}", *at),
                        bytes = *row,
                        values = ?&values[..*row / 4],
                        "walls trace: a read-back lands in title memory"
                    );
                }
                buffer.Unmap(0, None);
            }
        }
    }
}

/// After the card ran what declared `back`: what shaders wrote there goes
/// into title memory, and the ranges are done with.
fn hand_back(back: &[Declared]) {
    if crate::shadow::wide_back() {
        // Every range first (the pieces that differ, found on several
        // threads), then the merges, in order, as below.
        let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
        for (shadow, declared) in back {
            if shadow.carded() {
                continue;
            }
            let mut sorted = declared.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            work.extend(spans.into_iter().map(|(from, to)| (shadow.clone(), from, to)));
        }
        #[cfg(feature = "diagnostics")]
        let scanning = std::time::Instant::now();
        let found = crate::shadow::find_all(&work);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::BackScanUs, scanning.elapsed().as_micros() as u64);
        for ((shadow, from, to), found) in work.iter().zip(found) {
            let written = shadow.copy_back_with(*from, to - from, Some(found));
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(*from, to - from, written);
        }
        for (shadow, declared) in back {
            shadow.copy_back_done(declared);
        }
        return;
    }
    for (shadow, declared) in back {
        if shadow.carded() {
            // Done before (`Host::card_back`).
            shadow.copy_back_done(declared);
            continue;
        }
        // Each byte compared once: the world declares dozens of views
        // of its 174 MB heap that overlap almost entirely, and comparing
        // every one whole was gigabytes a frame (0.3-0.5 s) for the few
        // MB shaders wrote. Only ranges that overlap or touch are
        // joined: what lies between two was not given to shaders (the
        // snapshot does not hold it).
        let mut sorted = declared.clone();
        sorted.sort_unstable();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (from, to) in sorted {
            match spans.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => spans.push((from, to)),
            }
        }
        for (from, to) in spans {
            let written = shadow.copy_back(from, to - from);
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
        }
        shadow.copy_back_done(declared);
    }
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both); logged once.
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    /// Whether the host GPU supports the depth bounds test; logged once.
    pub(crate) fn depth_bounds(&self) -> bool {
        static BOUNDS: OnceLock<bool> = OnceLock::new();
        *BOUNDS.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
            // SAFETY: writes `options` of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS2,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let supported = ok && options.DepthBoundsTestSupported.as_bool();
            tracing::info!(supported, "host depth bounds support");
            supported
        })
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let before = private_bytes();
        let list = self.make_list();
        LISTS[0].fetch_add(1, Ordering::Relaxed);
        // (May shrink while a list is made, on another thread's account:
        // added as it is, the sum is what counts.)
        #[cfg(feature = "diagnostics")]
        LISTS[2].fetch_add(private_bytes().wrapping_sub(before), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::HostLists, 1);
            add(C::HostListNs, started.elapsed().as_nanos() as u64);
        }
        list
    }

    /// An allocator and a closed list for a prelude.
    fn make_prelude(&self) -> Option<Prelude> {
        // SAFETY: plain object creation.
        let made: windows::core::Result<Prelude> = unsafe {
            (|| {
                let allocator: ID3D12CommandAllocator =
                    self.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList =
                    self.device
                        .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.Close()?;
                Ok((allocator, list))
            })()
        };
        LISTS[1].fetch_add(1, Ordering::Relaxed);
        match made {
            Ok(prelude) => Some(prelude),
            Err(e) => {
                tracing::warn!(error = %e, "no host list for a prelude");
                None
            }
        }
    }

    fn make_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            let prelude = OnceLock::new();
            if !lazy_prelude() {
                let _ = prelude.set(self.make_prelude());
            }
            Ok(HostList {
                allocator,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                twins: Mutex::default(),
                prelude,
                repeats: Mutex::default(),
                pipelines: Mutex::default(),
                budget: OnceLock::new(),
                indirect_args: Mutex::default(),
                _alive: Counted::new(),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    /// The card's clock ([`CARD_CLOCK_US`]), made at the first send.
    fn card_clock(&self) -> Option<&'static CardClock> {
        static CLOCK: OnceLock<Option<CardClock>> = OnceLock::new();
        static WATCH: std::sync::Once = std::sync::Once::new();
        let clock = CLOCK
            .get_or_init(|| {
                // SAFETY: a plain fence on our device.
                let fence = unsafe { self.device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) }.ok()?;
                Some(CardClock {
                    fence,
                    state: Mutex::new((0, None)),
                    sent: std::sync::Condvar::new(),
                })
            })
            .as_ref()?;
        WATCH.call_once(|| {
            if let Err(e) = std::thread::Builder::new().name("victory-card-clock".into()).spawn(move || card_clock_watch(clock)) {
                tracing::warn!(error = %e, "no thread for the card's clock: the frame line's idle times stay 0");
            }
        });
        Some(clock)
    }

    /// Lists of ours just went to the queue: the clock's signal goes
    /// behind them, and an idle stretch of the card, if one was under way,
    /// ends here ([`CARD_CLOCK_US`]).
    fn card_sent(&self) {
        let Some(clock) = self.card_clock() else {
            return;
        };
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        // SAFETY: our queue and the clock's fence.
        let _ = unsafe { self.queue.Signal(&clock.fence, state.0) };
        if let Some((since, worked_then)) = state.1.take() {
            CARD_CLOCK_US[0].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
            CARD_CLOCK_US[1].fetch_add(worked().saturating_sub(worked_then), Ordering::Relaxed);
        }
        drop(state);
        clock.sent.notify_one();
    }

    /// Whether the host has what copies in the card's memory need (three
    /// lists and the compare); made at the first ask. A copy goes to the
    /// card's memory only if it has.
    pub(crate) fn card_ready(&self) -> bool {
        static READY: OnceLock<bool> = OnceLock::new();
        *READY.get_or_init(|| match self.make_card_work(relay_snapshot().then(card_ring_mb)) {
            Ok(work) => {
                *CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()) = Some(work);
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no host copies in the card's memory: the compare could not be made");
                false
            }
        })
    }

    /// What copies in the card's memory need ([`CardWork`]); with
    /// `ring_mb`, the ring of pieces of that many megabytes
    /// ([`relay_snapshot`]).
    fn make_card_work(&self, ring_mb: Option<u64>) -> windows::core::Result<CardWork> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out
        // pointer is ours.
        unsafe {
            D3DCompile(
                CARD_COMPARE.as_ptr().cast(),
                CARD_COMPARE.len(),
                windows::core::s!("card compare"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let list = || self.make_prelude().ok_or_else(windows::core::Error::empty);
        // A megabyte of results: 262,144 pieces, 16 GiB of declared ranges
        // in one submission.
        const RESULTS: u64 = 1 << 20;
        let (differs, data) = self.shared_buffer(RESULTS).ok_or_else(windows::core::Error::empty)?;
        name(&differs, || "card compare results".into());
        // The ring of pieces the compare copies (`relay_snapshot`): its
        // first slot's worth of bytes holds the count and the limit
        // (`CardAreas`).
        let ring = ring_mb.and_then(|mb| {
            let slots = ((mb << 20) / CARD_SLOT_BYTES as u64).saturating_sub(1).max(1) as u32;
            let bytes = (u64::from(slots) + 1) * CARD_SLOT_BYTES as u64;
            let (ring, data) = self.shared_buffer(bytes)?;
            name(&ring, || "card compare ring of pieces".into());
            tracing::info!(slots, mb, "pieces the card's compare finds differing are copied in the queue's order (VICTORY_RELAY_SNAPSHOT=1)");
            Some((ring, data as usize, slots))
        });
        if ring_mb.is_some() && ring.is_none() {
            tracing::warn!("no ring for the pieces the card's compare finds differing: they are fetched as before");
        }
        let areas = CardAreas::new(RESULTS / 4, ring.as_ref().map(|r| r.2));
        if let (Some((_, data, _)), Some(limit)) = (&ring, areas.limit()) {
            // SAFETY: the mapping's first slot's worth of bytes; nothing
            // runs on the card yet.
            unsafe {
                (*data as *mut u32).write_volatile(0);
                set_card_limit(*data as *mut u8, limit);
            }
        }
        // SAFETY: the blob's own bytes; the root signature is inside the
        // shader.
        unsafe {
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState = self.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })?;
            Ok(CardWork {
                sync: vec![list()?],
                check: vec![list()?],
                fetch: list()?,
                root,
                pso,
                differs: (differs, data as usize, RESULTS),
                fetched: None,
                ring,
                areas,
                overdue: CardOverdue::default(),
                patch: self.make_card_patch(),
            })
        }
    }

    /// [`CARD_PATCH`]'s root signature and pipeline; none (and title bytes
    /// go up by a copy per run, as before) if it does not compile.
    fn make_card_patch(&self) -> Option<(ID3D12RootSignature, ID3D12PipelineState)> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out
        // pointer is ours.
        let made = unsafe {
            D3DCompile(
                CARD_PATCH.as_ptr().cast(),
                CARD_PATCH.len(),
                windows::core::s!("card patch"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )
            .ok()
            .and(code)
            .and_then(|code| {
                let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
                let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes).ok()?;
                let pso: ID3D12PipelineState = self
                    .device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    })
                    .ok()?;
                Some((root, pso))
            })
        };
        if made.is_none() {
            tracing::warn!("the patch of title bytes into copies in the card's memory did not compile: a copy per run, as before");
        }
        CARD_PATCHES.store(made.is_some(), Ordering::Relaxed);
        made
    }

    /// The buffer for a submission's patches of title bytes ([`CARD_PATCH`]),
    /// taken before the title's bytes are put in under flights
    /// (`stale_under_flights`), so that the merge takes no more short runs
    /// than can go up as patches: [`CARD_PATCH_BYTES_MOST`] bytes, its whole
    /// room, kept or made. `None` without the patch or without memory: the
    /// short runs then take copies, as before. [`Host::card_before`] gives
    /// it back, or a plan whose patches it holds.
    fn patch_reserve(&self) -> Option<PatchBuffer> {
        if !CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|work| work.patch.is_some()) {
            return None;
        }
        PatchPool::reserve(&PATCH_POOL, CARD_PATCH_BYTES_MOST as u64, |bytes| self.make_patch_buffer(bytes))
    }

    /// A buffer in the PC's memory of `bytes` for patches.
    fn make_patch_buffer(&self, bytes: u64) -> Option<PatchMemory> {
        let made = self.shared_buffer(bytes).map(|(buffer, data)| {
            name(&buffer, || "patches of title bytes for copies in the card's memory".into());
            PatchMemory(buffer, data as usize)
        });
        if made.is_none() {
            CARD_PATCHED[4].fetch_add(1, Ordering::Relaxed);
        }
        made
    }

    /// A buffer for a plan's `bytes` of patches ([`PatchPool::for_plan`]):
    /// the smallest kept that holds them within four times the buffer one
    /// made for them would be, else one made of their bytes' next power of
    /// two (64 KiB at least, where it was 1 MiB); without memory the
    /// submission's `reserve` if it holds them, else the biggest there is,
    /// and the caller sends the ranges past its room by copies. The reserve
    /// goes back to the pool once the plan's buffer is chosen.
    fn patch_buffer(&self, bytes: u64, reserve: Option<PatchBuffer>) -> Option<PatchBuffer> {
        PatchPool::for_plan(&PATCH_POOL, bytes, reserve, |want| self.make_patch_buffer(want))
    }

    /// A buffer of patches that no plan holds goes back to the pool.
    fn patch_back(&self, buffer: Option<PatchBuffer>) {
        if let Some((buffer, bytes)) = buffer {
            PATCH_POOL.lock().unwrap_or_else(|e| e.into_inner()).give_back(buffer, bytes);
        }
    }

    /// The compare's ring UAV and its slots for a compare of `flown` lists:
    /// the ring's own buffer when [`relay_snapshot`] is on and the lists
    /// are sent ahead, else the results buffer and no slots (the shader
    /// never writes the ring then, but the root parameter is set).
    fn card_ring(work: &CardWork, flown: bool) -> (u64, u32) {
        match (&work.ring, flown) {
            // SAFETY: plain queries of live buffers.
            (Some((ring, _, slots)), true) => (unsafe { ring.GetGPUVirtualAddress() }, *slots),
            // SAFETY: plain queries of live buffers.
            _ => (unsafe { work.differs.0.GetGPUVirtualAddress() }, 0),
        }
    }

    /// Before a submission's lists run, for copies in the card's memory
    /// ([`crate::shadow::cards`]): what changed on their CPU side since
    /// the last submission goes to their card side (a list of ours, run
    /// now; the submission's lists follow it on the queue), and the ranges
    /// this submission's shaders may write get a compare to run after its
    /// lists. `None` if there is nothing to do.
    ///
    /// `flown`: the submission is sent ahead with [`relay`]. Its compare
    /// then stores the hashes it makes (see [`flagged_pieces`] and
    /// `relay`). `reserve`: the buffer of patches taken for the title's
    /// bytes put in under flights ([`Host::patch_reserve`]). `relay`:
    /// whether [`relay`] is on (the caller's [`relay`]); the send is then
    /// numbered in the order of the queue.
    fn card_before(&self, runs: &[&HostList], flown: bool, reserve: Option<PatchBuffer>, relay: bool) -> Option<CardPlan> {
        use std::sync::Arc;
        if !crate::shadow::cards() {
            self.patch_back(reserve);
            return None;
        }
        // The ranges this submission's shaders may write, per copy.
        let mut declared: Vec<(Arc<crate::shadow::Shadow>, ByteRanges)> = Vec::new();
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::GpuWrite(shadow, address, len) = step
                    && shadow.carded()
                {
                    let range = (*address, *address + *len);
                    match declared.iter_mut().find(|(s, _)| Arc::ptr_eq(s, shadow)) {
                        Some((_, ranges)) => {
                            if !ranges.contains(&range) {
                                ranges.push(range);
                            }
                        }
                        None => declared.push((shadow.clone(), vec![range])),
                    }
                }
            }
        }
        // With the relay, the number of this send in the order of the queue
        // (`QUEUE_ORDER`): the count so far and one. Only this thread sends
        // (it holds the submission lock), and the count is set to it below,
        // under the order's lock, with the list put on the queue: a fetch
        // that goes to the queue before then has a lower count and leaves
        // the ranges taken now out (`Shadow::card_merge_relay`), as it did
        // while the order's lock was held from the taking on. The patches
        // and the runs of pieces hashed again are made before it is taken:
        // the relay's fetches waited for them.
        let sent_as = if relay { *QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()) + 1 } else { 0 };
        let mut dirty = crate::shadow::card_dirty(sent_as, relay);
        let patching = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|work| work.patch.is_some());
        if (declared.is_empty() && dirty.is_empty()) || patching.is_none() {
            self.patch_back(reserve);
            return None;
        }
        let patching = patching == Some(true);
        // A copy whose shader writes are declared for the first time gets
        // its bytes from before, whole.
        let mut whole: Vec<Arc<crate::shadow::Shadow>> = Vec::new();
        for (shadow, _) in &declared {
            if shadow.card_make_before(|size| {
                let before = self.uav_buffer(crate::shadow::card_hash_bytes(size))?;
                name(&before, || "title buffer on the card, hashes of its pieces".into());
                Some(before)
            }) {
                whole.push(shadow.clone());
                if !dirty.iter().any(|(s, _)| Arc::ptr_eq(s, shadow)) {
                    dirty.push((shadow.clone(), Vec::new().into()));
                }
            }
        }
        // Per copy: the ranges that go up by a copy each, and the short
        // ones that go up as patches ([`CARD_PATCH`]), which title bytes
        // put in under flights are (thousands of runs of a few bytes a
        // frame), with where its patches are in the plan's buffer; and the
        // runs of pieces hashed again after them. Made before the order of
        // the queue and the card's work are taken; the bytes are read under
        // each copy's CPU side's lock alone. The buffer may hold fewer
        // patches than the short ranges make: the plan's patches are not
        // all the merge's (short ranges of refreshes and of merges of what
        // the card wrote go as patches too, past the room the merge took),
        // and a buffer made for more than 16 MiB of them may find no
        // memory; the short ranges past what it holds go by copies, past
        // the 512 the merge counted.
        let copies: Vec<SendCopy<'_>> = dirty
            .iter()
            .map(|(shadow, ranges)| {
                let sides = shadow.card_sides();
                let twin = sides.as_ref().is_some_and(|(_, before, _)| before.is_some());
                let (start, len) = shadow.span();
                SendCopy {
                    ranges,
                    patchable: patching && sides.as_ref().is_some_and(|(_, _, size)| card_patchable(*size)),
                    span: (start as usize, len as usize),
                    rehash: match (twin, whole.iter().any(|s| Arc::ptr_eq(s, shadow))) {
                        (false, _) => Rehash::No,
                        (true, true) => Rehash::Whole,
                        (true, false) => Rehash::Ranges,
                    },
                }
            })
            .collect();
        let mut reserve = reserve;
        let SendPlan {
            splits,
            patches,
            patched_at,
            buffer: patches_in,
            hashes,
        } = plan_send(
            &copies,
            patching,
            |bytes| self.patch_buffer(bytes, reserve.take()),
            |buffer| self.patch_back(Some(buffer)),
            |i, short, out| dirty[i].0.card_patches(short, out),
        );
        drop(copies);
        self.patch_back(reserve);
        if let Some((PatchMemory(_, data), size)) = &patches_in {
            let bytes = patches.len() * 12;
            if bytes as u64 > *size {
                fail(format_args!("{} patches of title bytes for a buffer of {size} bytes", patches.len()));
            }
            // SAFETY: the mapping holds `size` bytes, `bytes` or more; the
            // buffer is no plan's, so the card does not read it now.
            unsafe { std::ptr::copy_nonoverlapping(patches.as_ptr().cast::<u8>(), *data as *mut u8, bytes) };
            let patched = splits.iter().flat_map(|(_, short)| short).map(|&(a, b)| (b - a) as u64).sum::<u64>();
            CARD_PATCHED[0].fetch_add(patches.len() as u64, Ordering::Relaxed);
            CARD_PATCHED[1].fetch_add(patched_at.iter().filter(|p| !p.is_empty()).count() as u64, Ordering::Relaxed);
            CARD_PATCHED[2].fetch_add(patched, Ordering::Relaxed);
            crate::shadow::CARD_STATS[0].fetch_add(patched, Ordering::Relaxed);
        }
        let mut order = relay.then(|| QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(order) = order.as_mut() {
            if **order + 1 != sent_as {
                fail(format_args!("the order of the queue moved from {} while send {sent_as} was made", sent_as - 1));
            }
            **order = sent_as;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let work = work.as_mut()?;
        let mut used: Vec<(bool, Prelude)> = Vec::new();
        if !dirty.is_empty() {
            let Some(sync) = work.sync.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!("no host list to bring host copies in the card's memory up to date"));
            };
            let (allocator, list) = &sync;
            // SAFETY: the list's last run finished (it ran ahead of a
            // submission that was waited for, and came back after that);
            // live buffers, and the ranges lie in them.
            let recorded = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for ((((shadow, ranges), (copied, _)), patched), rehash) in dirty.iter().zip(&splits).zip(&patched_at).zip(&hashes) {
                        let Some((staging, before, size)) = shadow.card_sides() else {
                            continue;
                        };
                        for &(a, b) in copied {
                            list.CopyBufferRegion(&shadow.buffer, a as u64, staging, a as u64, (b - a) as u64);
                            crate::shadow::CARD_STATS[0].fetch_add((b - a) as u64, Ordering::Relaxed);
                        }
                        CARD_PATCHED[3].fetch_add(copied.len() as u64, Ordering::Relaxed);
                        crate::shadow::CARD_STATS[1].fetch_add(ranges.len() as u64, Ordering::Relaxed);
                        // The card side's state: common at the list's start
                        // (a buffer's decays when its submission has run).
                        let mut state = if copied.is_empty() {
                            D3D12_RESOURCE_STATE_COMMON
                        } else {
                            D3D12_RESOURCE_STATE_COPY_DEST
                        };
                        if let (false, Some((PatchMemory(buffer, _), _)), Some((root, pso))) =
                            (patched.is_empty(), &patches_in, &work.patch)
                        {
                            // After the copies; each patch puts the title's
                            // bytes into its dword over what the card holds.
                            list.ResourceBarrier(&[transition(&shadow.buffer, state, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
                            state = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
                            list.SetComputeRootSignature(root);
                            list.SetPipelineState(pso);
                            list.SetComputeRootUnorderedAccessView(2, shadow.buffer.GetGPUVirtualAddress());
                            const MOST: usize = 65535 * CARD_PATCH_PER_GROUP as usize;
                            let mut first = patched.start;
                            while first < patched.end {
                                let n = (patched.end - first).min(MOST);
                                list.SetComputeRoot32BitConstant(0, n as u32, 0);
                                list.SetComputeRootShaderResourceView(1, buffer.GetGPUVirtualAddress() + first as u64 * 12);
                                list.Dispatch((n as u32).div_ceil(CARD_PATCH_PER_GROUP), 1, 1);
                                first += n;
                            }
                        }
                        let Some(before) = before else {
                            continue;
                        };
                        // No twin (it cost the card the copy's size again,
                        // 399 MB in run 75): `before` holds a 128-bit hash
                        // of each 64 KiB piece. The pieces just sent up are
                        // hashed again, all of them the first time; the
                        // compare after the lists hashes the declared
                        // pieces and flags those whose hash is another.
                        if state != D3D12_RESOURCE_STATE_COMMON {
                            list.ResourceBarrier(&[transition(
                                &shadow.buffer,
                                state,
                                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                            )]);
                        }
                        list.SetComputeRootSignature(&work.root);
                        list.SetPipelineState(&work.pso);
                        list.SetComputeRootShaderResourceView(1, shadow.buffer.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                        list.SetComputeRootUnorderedAccessView(3, work.differs.0.GetGPUVirtualAddress());
                        // Hashing only: the ring is set, never written. The
                        // runs were made with the plan, outside the locks.
                        list.SetComputeRootUnorderedAccessView(4, Self::card_ring(work, false).0);
                        CARD_PASSES[0].fetch_add(rehash.len() as u64, Ordering::Relaxed);
                        CARD_PASSES[1].fetch_add(rehash.iter().map(|r| u64::from(r.pieces)).sum::<u64>(), Ordering::Relaxed);
                        for run in rehash {
                            let constants = [run.first, run.skew, size as u32, run.pieces, 0, 0, 0, 0];
                            list.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
                            list.Dispatch(run.pieces.div_ceil(CARD_COMPARE_PIECES_PER_GROUP), 1, 1);
                        }
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    Ok(())
                })()
            };
            if let Err(e) = recorded {
                fail(format_args!("host copies in the card's memory could not be brought up to date ({e})"));
            }
            used.push((false, sync));
        }
        drop(order);
        let (ring, slots) = Self::card_ring(work, flown);
        let mut plan = CardPlan {
            ranges: Vec::new(),
            check: None,
            _synced: dirty.into_iter().map(|(shadow, _)| shadow).collect(),
            lists: used,
            sent_as,
            in_ring: slots != 0,
            run: None,
            count_at: None,
            patches: patches_in,
        };
        // Each byte once, as the copy-back joins the declared ranges.
        for (shadow, ranges) in &declared {
            let mut sorted = ranges.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            let twin = shadow.card_sides().is_some_and(|(_, before, _)| before.is_some());
            for (from, to) in spans {
                let Some(span) = shadow.card_span(from, to) else {
                    continue;
                };
                plan.ranges.push(CardRange {
                    shadow: shadow.clone(),
                    declared: (from, to),
                    span,
                    at: 0,
                    compared: twin,
                });
            }
        }
        // The results of all the ranges compared (and with the ring the
        // count after them), one run behind those of plans still out
        // (`CardAreas`). With no room none is compared: every piece of
        // them is fetched.
        let pieces: Vec<u32> = plan.ranges.iter().filter(|r| r.compared).map(|r| r.span.pieces).collect();
        if pieces.iter().any(|&p| p > 0) {
            if let Some(times) = work.overdue.look(work.areas.low(), relay, work.areas.out(), late_most()) {
                tracing::warn!(
                    times,
                    runs_out = work.areas.out(),
                    most_lists = late_most(),
                    low_in_a_row = work.overdue.low,
                    free = work.areas.free(),
                    "the card compare's results look held by a plan never done with: every run after its run stays out, \
                     and submissions sent ahead wait for flights to land (\"lists in the air\"); said at the 1st, 2nd, 4th... \
                     plan in a row that finds them so"
                );
            }
            if !plan.take_run(&mut work.areas, &pieces) && relay_snapshot() {
                SNAPSHOT_STATS[5].fetch_add(1, Ordering::Relaxed);
            }
        }
        if plan.ranges.iter().any(|r| r.compared) {
            let Some(check) = work.check.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!("no host list for the compare of host copies in the card's memory"));
            };
            let (allocator, list) = &check;
            let compares: Vec<RangeCompare> = plan
                .ranges
                .iter()
                .filter(|r| r.compared)
                .filter_map(|r| {
                    let (_, before, size) = r.shadow.card_sides()?;
                    let before = before?;
                    Some(RangeCompare {
                        // SAFETY: plain queries of live buffers (the plan holds the copy, the copy its hashes).
                        copy: unsafe { r.shadow.buffer.GetGPUVirtualAddress() },
                        // SAFETY: plain queries of live buffers (the plan holds the copy, the copy its hashes).
                        hashes: unsafe { before.GetGPUVirtualAddress() },
                        size: size as u32,
                        first: r.span.first,
                        skew: r.span.skew,
                        pieces: r.span.pieces,
                        at: r.at,
                    })
                })
                .collect();
            CARD_PASSES[2].fetch_add(compares.len() as u64, Ordering::Relaxed);
            let views = CompareViews {
                root: &work.root,
                pso: &work.pso,
                // SAFETY: a plain query of a live buffer.
                results: unsafe { work.differs.0.GetGPUVirtualAddress() },
                ring,
                slots,
            };
            // SAFETY: the list's last run finished (its submission was
            // waited for, and it came back after that); live buffers; the
            // compare's own root signature.
            let recorded = unsafe {
                (|| -> windows::core::Result<ID3D12CommandList> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    record_card_compare(list, &views, &compares, if flown { 2 } else { 1 }, plan.count_at);
                    list.Close()?;
                    list.cast()
                })()
            };
            match recorded {
                Ok(list) => plan.check = Some(list),
                Err(e) => fail(format_args!("the compare of host copies in the card's memory could not be recorded ({e})")),
            }
            plan.lists.push((true, check));
        }
        PATCH_POOL.lock().unwrap_or_else(|e| e.into_inner()).plan();
        Some(plan)
    }

    /// With [`relay`]: whether an eighth or less of the compare's results is
    /// free, held by plans in the air (`CardAreas`), so the caller lets
    /// flights land first (with [`relay_exact`] only until it is not:
    /// [`results_wait_over`]): a plan that finds no room compares nothing,
    /// and its landing fetches every piece of its ranges. (A frame of the
    /// live runs held some 14,000 of the 262,144 dwords, and the lists in
    /// the air are capped first.) Not the ring of pieces: a piece with no
    /// slot is fetched, as before.
    fn card_results_low(&self) -> bool {
        card_results_low_of(&CARD_WORK)
    }

    /// The plan's submission has run and its results were read and merged
    /// ([`Host::card_back`], [`Host::relay_card_back`]): its lists are free
    /// for the next plan, and its run of the results and the ring's slots
    /// its compare took once the plans before it are done with too
    /// (`CardAreas`; plans are done with in the queue's order). The ring's
    /// limit is written either way ([`CardWork::plan_done`]).
    fn card_done(&self, plan: CardPlan) {
        let mut plan = plan;
        self.patch_back(plan.patches.take());
        if let Some(work) = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            work.plan_done(plan);
        }
    }

    /// The relay's own list, fence, event for [`Host::relay_card_back`].
    fn make_relay_fetch(&self) -> Option<RelayFetch> {
        let list = self.make_prelude()?;
        // SAFETY: plain objects of our own.
        unsafe {
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(RelayFetch {
                list,
                fence,
                event,
                sent: 0,
                buffer: None,
            })
        }
    }

    /// [`relay`], on the thread that lands flights: for the plans of the
    /// flights being landed (the card has run them), the pieces their
    /// compares flagged are fetched, once each, with a list and a wait of
    /// this thread's own, and what shaders wrote in them goes into title
    /// memory (`Shadow::card_merge_relay`). As [`Host::card_back`], but:
    /// no submission lock; a piece flagged under one range of a plan is
    /// fetched under all ([`flagged_pieces`]); the fetch runs behind
    /// whatever was sent meanwhile, so it may bring a later flight's
    /// bytes, which that flight's landing then finds merged already; and
    /// nothing is sent back up.
    fn relay_card_back(&self, plans: &[&CardPlan], fetch: &mut Option<RelayFetch>) {
        let (differs, ring) = {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            (
                work.as_ref().map(|work| (work.differs.1, (work.differs.2 / 4) as usize)),
                work.as_ref().and_then(|work| work.ring.as_ref().map(|(_, data, slots)| (*data, *slots))),
            )
        };
        // With the ring (`relay_snapshot`) a later landing may merge pieces
        // the card copied before sends that are already on the queue now:
        // their notes must stay until that landing, so notes are dropped
        // only up to the sends these flights saw. (Dropping fewer is always
        // safe: a note older than a merge's own sends skips nothing.)
        let seen_by_these = plans.iter().map(|p| p.sent_as).max();
        let drop_notes = |after: u64| match (relay_snapshot(), seen_by_these) {
            (false, _) => crate::shadow::relay_fetched(after),
            (true, Some(seen)) => crate::shadow::relay_fetched(after.min(seen)),
            (true, None) => {}
        };
        if let Some((differs, dwords)) = differs
            && relay_snapshot()
        {
            // SAFETY: as below, for the results.
            let differs = unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) };
            // Pieces the compares found differing past the ring's limit: 1,
            // with the ring.
            let unslotted: usize = plans
                .iter()
                .filter(|p| p.in_ring)
                .flat_map(|p| p.ranges.iter().filter(|r| r.compared))
                .map(|r| differs.get(r.at as usize..(r.at + r.span.pieces) as usize).map_or(0, |d| d.iter().filter(|&&d| d == 1).count()))
                .sum();
            SNAPSHOT_STATS[6].fetch_add(unslotted as u64, Ordering::Relaxed);
        }
        if let (Some((differs, dwords)), Some((ring, slots))) = (differs, ring)
            && relay_snapshot()
            // SAFETY: as below, for the results; the ring's slots that these
            // plans' compares took are given to no other plan before these
            // are done with (`card_done`: the limit stays below them).
            && unsafe { self.relay_from_ring(plans, std::slice::from_raw_parts(differs as *const u32, dwords), ring, slots) }
        {
            drop_notes(seen_by_these.unwrap_or(0));
            forget_merged_upto(seen_by_these);
            return;
        }
        let mut pieces: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        // What is fetched, by copy and piece, and the bytes of the piece
        // fetched (`MERGED_UPTO`).
        let mut keys: Vec<((usize, u32), (usize, usize))> = Vec::new();
        let mut total = 0u64;
        if let Some((differs, dwords)) = differs {
            // SAFETY: the mapping of the results. The compares of these
            // plans have run, and their dwords are given to no other plan
            // before these are done with (`card_done`).
            let differs = unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) };
            let mut seen = std::collections::HashSet::new();
            for plan in plans {
                let flags: Vec<(usize, u32, bool)> = plan
                    .ranges
                    .iter()
                    .flat_map(|r| {
                        (0..r.span.pieces).map(move |p| {
                            let flagged = !r.compared || differs.get((r.at + p) as usize).is_none_or(|&d| d != 0);
                            (std::sync::Arc::as_ptr(&r.shadow) as usize, r.span.first + p, flagged)
                        })
                    })
                    .collect();
                let fetched = flagged_pieces(&flags);
                for r in &plan.ranges {
                    let copy = std::sync::Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        if !fetched.contains(&(copy, r.span.first + p)) {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo < hi && seen.insert((copy, lo, hi)) {
                            pieces.push((r.shadow.clone(), lo, hi, total));
                            keys.push(((copy, r.span.first + p), (lo, hi)));
                            total += (hi - lo) as u64;
                        }
                    }
                    crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                }
            }
        }
        if relay_snapshot() && !plans.is_empty() {
            SNAPSHOT_STATS[1].fetch_add(1, Ordering::Relaxed);
        }
        if pieces.is_empty() {
            // Nothing to fetch: every send so far is older than the next fetch.
            drop_notes(*QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
            forget_merged_upto(seen_by_these);
            return;
        }
        if fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        let Some(f) = fetch.as_mut() else {
            fail(format_args!("no list, fence or event to fetch what shaders wrote to the card's memory (VICTORY_RELAY=1)"));
        };
        if f.buffer.as_ref().is_none_or(|b| b.2 < total) {
            let size = total.next_multiple_of(16 << 20);
            f.buffer = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
        }
        let Some((buffer, data, _)) = f.buffer.clone() else {
            fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes, VICTORY_RELAY=1)"));
        };
        f.sent += 1;
        let ((allocator, list), fence, event, sent) = (f.list.clone(), f.fence.clone(), f.event, f.sent);
        let waiting = std::time::Instant::now();
        // SAFETY: the list's last run was waited for by this thread; live
        // buffers (the copies are held by the plans); the fence and the
        // event are this thread's.
        let ran = unsafe {
            (|| -> windows::core::Result<u64> {
                allocator.Reset()?;
                list.Reset(&allocator, None)?;
                for (shadow, lo, hi, at) in &pieces {
                    list.CopyBufferRegion(&buffer, *at, &shadow.buffer, *lo as u64, (*hi - *lo) as u64);
                }
                list.Close()?;
                let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                // In the order of the queue: the sends of title bytes so
                // far are ahead of this fetch, every later one behind it.
                let order = QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner());
                self.queue.ExecuteCommandLists(&lists);
                let after = *order;
                drop(order);
                self.card_sent();
                self.queue.Signal(&fence, sent)?;
                fence.SetEventOnCompletion(sent, event)?;
                WaitForSingleObject(event, INFINITE);
                Ok(after)
            })()
        };
        let after = match ran {
            Ok(after) => after,
            Err(e) => fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e}, VICTORY_RELAY=1)")),
        };
        let us = waiting.elapsed().as_micros() as u64;
        RELAY_STATS[3].fetch_add(us, Ordering::Relaxed);
        {
            use crate::shadow::CARD_STATS;
            CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
            CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
            CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
            CARD_STATS[6].fetch_add(us, Ordering::Relaxed);
        }
        let mut title = Vec::new();
        let mut written = 0;
        for (shadow, lo, hi, at) in &pieces {
            // SAFETY: the mapping of the fetched bytes, `total` of them;
            // the copy into it has run.
            let gpu = unsafe { std::slice::from_raw_parts((data + *at as usize) as *const u8, hi - lo) };
            let merged = shadow.card_merge_relay(*lo, gpu, after, &mut title);
            shadow.walls_merged(gpu.len(), merged);
            written += merged;
        }
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        // Sends up to `after` are of no use to any later merge.
        drop_notes(after);
        // This fetch ran behind every submission queued so far, so it may
        // have brought bytes of flights still in the air: a later landing
        // must not put an older copy of these pieces over them.
        if relay_snapshot() {
            let mut upto = MERGED_UPTO.lock().unwrap_or_else(|e| e.into_inner());
            note_upto(upto.get_or_insert_with(Default::default), keys, after);
        }
        forget_merged_upto(seen_by_these);
    }

    /// [`relay_snapshot`], for [`Host::relay_card_back`]: the pieces these
    /// plans' compares flagged are merged from the ring, plan by plan in the
    /// order the flights ran, each with the sends its own flight saw (a
    /// piece's bytes are as that flight left them, before any later
    /// submission's sends or shader writes reached the copy). All or
    /// nothing: false, with nothing merged, if a plan did not use the ring,
    /// did not compare a range, or flagged a piece the ring had no slot for;
    /// the caller then fetches as before.
    ///
    /// # Safety
    /// `differs` is the compare's results and `ring` the mapping of the
    /// ring of `slots` slots; the compares of `plans` have run.
    unsafe fn relay_from_ring(&self, plans: &[&CardPlan], differs: &[u32], ring: usize, slots: u32) -> bool {
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;
        if plans.is_empty() || !plans.iter().all(|p| p.in_ring && p.ranges.iter().all(|r| r.compared)) {
            return false;
        }
        // (copy, offsets, where its bytes are, the sends its flight saw).
        let mut merges: Vec<(Arc<crate::shadow::Shadow>, usize, usize, usize, u64)> = Vec::new();
        let mut compared = 0u64;
        let mut older = 0u64;
        let upto = MERGED_UPTO.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
        for plan in plans {
            // Per piece of a copy: its slot, if the compare of any of the
            // plan's ranges copied it (a piece two ranges share is hashed
            // twice, and only the first compare finds it changed).
            let mut slot_of: HashMap<(usize, u32), Option<u32>> = HashMap::new();
            let mut flags: Vec<(usize, u32, bool)> = Vec::new();
            for r in &plan.ranges {
                let copy = Arc::as_ptr(&r.shadow) as usize;
                for p in 0..r.span.pieces {
                    let piece = r.span.first + p;
                    // A dword outside the results counts as flagged, without a slot.
                    let d = differs.get((r.at + p) as usize).copied().unwrap_or(1);
                    flags.push((copy, piece, d != 0));
                    match d {
                        0 => {}
                        1 => {
                            slot_of.entry((copy, piece)).or_insert(None);
                        }
                        _ => {
                            slot_of.insert((copy, piece), Some(d - 2));
                        }
                    }
                }
                compared += u64::from(r.span.pieces);
            }
            let flagged = flagged_pieces(&flags);
            let mut seen = HashSet::new();
            for r in &plan.ranges {
                let copy = Arc::as_ptr(&r.shadow) as usize;
                for p in 0..r.span.pieces {
                    let piece = r.span.first + p;
                    if !flagged.contains(&(copy, piece)) {
                        continue;
                    }
                    let (lo, hi) = r.shadow.card_piece(&r.span, p);
                    if lo >= hi || !seen.insert((copy, lo, hi)) {
                        continue;
                    }
                    let slot = match ring_piece(
                        slot_of.get(&(copy, piece)).copied().flatten(),
                        slots,
                        plan.sent_as,
                        upto.get(&(copy, piece)).map_or(&[][..], Vec::as_slice),
                        lo,
                        hi,
                    ) {
                        RingPiece::Fetch => return false,
                        RingPiece::Older => {
                            older += 1;
                            continue;
                        }
                        RingPiece::Merge(slot) => slot,
                    };
                    let base = card_slot_start(piece, r.span.skew);
                    if lo < base || hi - base > CARD_SLOT_BYTES {
                        return false;
                    }
                    let at = ring + CARD_SLOT_BYTES * (slot as usize + 1) + (lo - base);
                    merges.push((r.shadow.clone(), lo, hi, at, plan.sent_as));
                }
            }
        }
        crate::shadow::CARD_STATS[2].fetch_add(compared, Ordering::Relaxed);
        let mut title = Vec::new();
        let (mut written, mut bytes) = (0usize, 0u64);
        for (shadow, lo, hi, at, after) in &merges {
            // SAFETY: the slot's bytes, which the compare wrote before its
            // flight's fence (the caller's); inside the ring (checked above).
            let gpu = unsafe { std::slice::from_raw_parts(*at as *const u8, hi - lo) };
            written += shadow.card_merge_relay(*lo, gpu, *after, &mut title);
            bytes += (hi - lo) as u64;
        }
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        SNAPSHOT_STATS[0].fetch_add(1, Ordering::Relaxed);
        SNAPSHOT_STATS[2].fetch_add(merges.len() as u64, Ordering::Relaxed);
        SNAPSHOT_STATS[3].fetch_add(bytes, Ordering::Relaxed);
        SNAPSHOT_STATS[4].fetch_add(older, Ordering::Relaxed);
        true
    }

    /// [`relay`], on the thread that lands flights: `items` are the first
    /// things of the line, flights the card has run and what follows them.
    /// What their shaders wrote goes into title memory (copies in the
    /// card's memory first, then the others, each byte once whichever
    /// flights declared it); then every flight's ranges are done with;
    /// then, in the line's order, each flight's read-backs, its
    /// bottom-of-pipe writes and the fence signals behind it. Title memory
    /// is complete before the title is told anything.
    fn relay_land(&self, items: Vec<Late>, fetch: &mut Option<RelayFetch>) {
        let landing = std::time::Instant::now();
        {
            let flights: Vec<&Flight> = items
                .iter()
                .filter_map(|l| match l {
                    Late::Run(flight) => Some(flight),
                    _ => None,
                })
                .collect();
            if !flights.is_empty() {
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = flights.iter().flat_map(|f| f.order.iter().map(|&i| f.lists[i].host())).collect();
                    fail(format_args!(
                        "host GPU device removed after {} submissions sent ahead (VICTORY_RELAY=1) of {} lists{}{}",
                        flights.len(),
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                let plans: Vec<&CardPlan> = flights.iter().filter_map(|f| f.card.as_ref()).collect();
                self.relay_card_back(&plans, fetch);
                let mut back: Vec<Declared> = Vec::new();
                for flight in &flights {
                    for (shadow, ranges) in &flight.back {
                        match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                            Some((_, all)) => {
                                for range in ranges {
                                    if !all.contains(range) {
                                        all.push(*range);
                                    }
                                }
                            }
                            None => back.push((shadow.clone(), ranges.clone())),
                        }
                    }
                }
                relay_hand_back(&back);
                COPY_BACK[0].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
                RELAY_STATS[0].fetch_add(1, Ordering::Relaxed);
                RELAY_STATS[1].fetch_add(flights.len() as u64, Ordering::Relaxed);
            }
        }
        recopy_written_sources();
        // In the order given: what tells the title the card is done.
        let mut last = 0;
        let mut landed = Vec::new();
        // Keep the declarations per flight, including ranges shared by
        // flights. Each contributes one reference to the pending ranges.
        let mut completed_back = Vec::new();
        for item in items {
            match item {
                Late::Run(flight) => {
                    last = flight.number;
                    for &i in &flight.order {
                        read_backs(flight.lists[i].host());
                    }
                    if let Some(card) = flight.card {
                        self.card_done(card);
                    }
                    landed.push((flight.lists, flight.order));
                    completed_back.extend(flight.back);
                }
                Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                Late::Signal(fence, value) => crate::fence::set(fence as crate::object::This, value),
            }
        }
        if last > 0 {
            FINISHED.store(last, Ordering::Release);
            bury(last);
            copies_settled(last);
        }
        self.log_messages();
        RELAY_STATS[4].fetch_add(landing.elapsed().as_micros() as u64, Ordering::Relaxed);
        {
            let mut line = line();
            let Line {
                line: items,
                written,
                lists,
                settling,
                poke,
                landed: kept,
                in_hand,
                ..
            } = &mut *line;
            kept.extend(landed);
            in_hand.clear();
            // What is still in the air, from the flights that are.
            written.clear();
            *lists = 0;
            let mut ranges = Vec::new();
            for item in items.iter() {
                if let Late::Run(flight) = item {
                    *lists += flight.order.len().max(1);
                    ranges.extend(flight.back.iter().flat_map(|(_, r)| r.iter().copied()));
                }
            }
            join_ranges(written, ranges);
            *settling = false;
            // The landed lists are reset, and what queues hold looked at,
            // by the helper thread (they take locks a waiter may hold).
            *poke = true;
        }
        LINE_MOVED.notify_all();
        LINE_LANDED.notify_all();
        // Refreshes can see these ranges as free once the line and its
        // waiters have been told about the completed writes.
        for (shadow, ranges) in completed_back {
            shadow.copy_back_done(&ranges);
        }
    }

    /// After a submission ran, for copies in the card's memory: the pieces
    /// of the declared ranges that the compare found differing (all of a
    /// range it did not compare) are fetched, with one more wait for the
    /// card, and what shaders wrote in them goes into title memory.
    /// `submitted` is the last value signalled on the fence. `seen` holds
    /// the pieces fetched already since the card was last waited for
    /// (several submissions settled together name the same ones).
    fn card_back(
        &self,
        plan: &CardPlan,
        submitted: &mut u64,
        seen: &mut std::collections::HashSet<(usize, usize, usize)>,
    ) {
        if plan.ranges.is_empty() {
            return;
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        // SAFETY: the mapping of the results, which the compare (it has
        // run) wrote.
        let differs = unsafe { std::slice::from_raw_parts(work.differs.1 as *const u32, (work.differs.2 / 4) as usize) };
        // The pieces to fetch: the range, the offsets in its copy, and
        // where the bytes come down.
        let mut pieces: Vec<(usize, usize, usize, u64)> = Vec::new();
        let mut total = 0u64;
        for (i, r) in plan.ranges.iter().enumerate() {
            for p in 0..r.span.pieces {
                if r.compared && differs[(r.at + p) as usize] == 0 {
                    continue;
                }
                let (lo, hi) = r.shadow.card_piece(&r.span, p);
                if lo < hi && seen.insert((std::sync::Arc::as_ptr(&r.shadow) as usize, lo, hi)) {
                    pieces.push((i, lo, hi, total));
                    total += (hi - lo) as u64;
                }
            }
            crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
        }
        let mut written = vec![0usize; plan.ranges.len()];
        if !pieces.is_empty() {
            if work.fetched.as_ref().is_none_or(|f| f.2 < total) {
                let size = total.next_multiple_of(16 << 20);
                work.fetched = self.shared_buffer(size).map(|(buffer, data)| (buffer, data as usize, size));
            }
            let Some((fetched, data, _)) = &work.fetched else {
                fail(format_args!("no memory to fetch what shaders wrote to the card's memory ({total} bytes)"));
            };
            let waiting = std::time::Instant::now();
            let (allocator, list) = &work.fetch;
            // SAFETY: the list's last run was waited for; live buffers; the
            // fence and event are ours, under the submission lock.
            let ran = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for &(i, lo, hi, at) in &pieces {
                        list.CopyBufferRegion(fetched, at, &plan.ranges[i].shadow.buffer, lo as u64, (hi - lo) as u64);
                    }
                    list.Close()?;
                    self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
                    self.card_sent();
                    *submitted += 1;
                    self.queue.Signal(&self.fence, *submitted)?;
                    self.fence.SetEventOnCompletion(*submitted, self.event)?;
                    let was_working = working(false);
                    WaitForSingleObject(self.event, INFINITE);
                    stood(waiting);
                    working(was_working);
                    Ok(())
                })()
            };
            if let Err(e) = ran {
                fail(format_args!("what shaders wrote to the card's memory could not be fetched ({e})"));
            }
            {
                use crate::shadow::CARD_STATS;
                CARD_STATS[3].fetch_add(pieces.len() as u64, Ordering::Relaxed);
                CARD_STATS[4].fetch_add(total, Ordering::Relaxed);
                CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
                CARD_STATS[6].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            if wide_merge() && pieces.len() >= 8 {
                // The pieces are apart from one another (each its own bytes
                // of its copy and of title memory): several at a time.
                let fetched = *data;
                let counts: std::sync::Arc<Vec<std::sync::atomic::AtomicUsize>> =
                    std::sync::Arc::new((0..plan.ranges.len()).map(|_| std::sync::atomic::AtomicUsize::new(0)).collect());
                let each = pieces.len().div_ceil(8);
                let jobs: Vec<Box<dyn FnOnce() + Send>> = pieces
                    .chunks(each)
                    .map(|chunk| {
                        let chunk: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, usize, u64)> =
                            chunk.iter().map(|&(i, lo, hi, at)| (plan.ranges[i].shadow.clone(), i, lo, hi, at)).collect();
                        let counts = counts.clone();
                        Box::new(move || {
                            let mut title = Vec::new();
                            for (shadow, i, lo, hi, at) in chunk {
                                // SAFETY: the mapping of the fetched bytes;
                                // the copy into it has run, and the caller
                                // waits for this job.
                                let gpu = unsafe { std::slice::from_raw_parts((fetched + at as usize) as *const u8, hi - lo) };
                                let merged = shadow.card_merge(lo, gpu, &mut title);
                                shadow.walls_merged(gpu.len(), merged);
                                counts[i].fetch_add(merged, Ordering::Relaxed);
                            }
                        }) as Box<dyn FnOnce() + Send>
                    })
                    .collect();
                crate::shadow::wide_run(jobs);
                for (written, count) in written.iter_mut().zip(counts.iter()) {
                    *written += count.load(Ordering::Relaxed);
                }
            } else {
                let mut title = Vec::new();
                for &(i, lo, hi, at) in &pieces {
                    // SAFETY: the mapping of the fetched bytes, `total` of
                    // them; the copy into it has run.
                    let gpu = unsafe { std::slice::from_raw_parts((*data + at as usize) as *const u8, hi - lo) };
                    let merged = plan.ranges[i].shadow.card_merge(lo, gpu, &mut title);
                    plan.ranges[i].shadow.walls_merged(gpu.len(), merged);
                    written[i] += merged;
                }
            }
        }
        for (r, written) in plan.ranges.iter().zip(written) {
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            let (from, to) = r.declared;
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
            let _ = (from, to);
        }
    }

    /// Runs closed lists and waits for them, and with [`late`] for
    /// everything sent to the card before them, which is settled first.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        let _ = self.submit(runs, false);
    }

    /// A queue's submission with [`late`]: the host lists `lists`, taken
    /// from the title's, go to the card in `order` and are not waited for.
    /// They join the line of flights and are settled, with the others in
    /// the air, at the next point where what they do can be observed
    /// ([`late`] lists those points); their host lists are reset and kept
    /// for reuse then, as a queue does for the lists it ran.
    pub(crate) fn execute_late(&self, lists: Vec<crate::list::Detached>, order: Vec<usize>) {
        if order.is_empty() {
            crate::list::recycle(lists, &order);
            return;
        }
        late_threads();
        let sent = {
            let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
            self.submit(&runs, true)
        };
        let Some((mut submitted, sent)) = sent else {
            // (Not reached: a submission sent ahead always comes back.)
            crate::list::recycle(lists, &order);
            return;
        };
        let told = !sent.writes.is_empty();
        let most = {
            let mut line = line();
            let Line { line: items, written, lists: flying, target, .. } = &mut *line;
            join_ranges(written, sent.back.iter().flat_map(|(_, ranges)| ranges.iter().copied()));
            *flying += order.len();
            *target = *submitted;
            items.push(Late::Run(Flight {
                number: sent.number,
                fence: *target,
                lists,
                order,
                card: sent.card,
                back: sent.back,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
            *flying >= late_most()
        };
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        RELAY_MOVED.notify_one();
        if told && !late_writes() {
            self.settle(&mut submitted, WHY_WRITES);
        } else if most && relay_exact() {
            // Until there are fewer, not until there are none.
            relay_wait(WHY_MOST, |line| line.lists < late_most());
        } else if most {
            self.settle(&mut submitted, WHY_MOST);
        } else {
            LINE_MOVED.notify_one();
        }
        drop(submitted);
        land();
    }

    /// Sends `list`, a closed list of ours (the present's copy), as a
    /// flight ([`late_free_present`]): not waited for, settled with the
    /// others. The caller keeps the list and does not reset it before the
    /// submission whose number comes back has finished ([`FINISHED`]); 0 if
    /// it ran and was waited for after all.
    fn execute_own(&self, list: &HostList) -> u64 {
        late_threads();
        let Some((submitted, sent)) = self.submit(&[list], true) else {
            return 0;
        };
        let number = sent.number;
        {
            let mut line = line();
            let Line { line: items, written, lists: flying, target, .. } = &mut *line;
            join_ranges(written, sent.back.iter().flat_map(|(_, ranges)| ranges.iter().copied()));
            *flying += 1;
            *target = *submitted;
            items.push(Late::Run(Flight {
                number,
                fence: *target,
                lists: Vec::new(),
                order: Vec::new(),
                card: sent.card,
                back: sent.back,
            }));
            items.extend(sent.writes.into_iter().map(|(at, what, bytes)| Late::Write(at, what, bytes)));
        }
        LATE_STATS[0].fetch_add(1, Ordering::Relaxed);
        LINE_MOVED.notify_one();
        RELAY_MOVED.notify_one();
        drop(submitted);
        number
    }

    /// Waits for the card to run every flight in the air ([`late`]) and
    /// does what they left to do, as [`Host::execute`] does for one
    /// submission after its wait: the pieces their shaders wrote in copies
    /// in the card's memory are fetched and merged (each once), the ranges
    /// they declared in the other copies are compared and copied back
    /// (each byte once, whichever flights declared it), and then, in the
    /// order they were given: their bottom-of-pipe writes and the fence
    /// signals behind them. `submitted` is the submission lock's value;
    /// `why` one of the `WHY_` places. Nothing happens if nothing flies.
    ///
    /// Everything in the air is settled, never the first flights alone:
    /// the merge writes the copies' CPU side, which a flight still on the
    /// card may be writing too.
    fn settle(&self, submitted: &mut u64, why: usize) {
        if !late() {
            return;
        }
        if relay() {
            // Nothing is settled here: the thread that lands flights does
            // it, in order, and this waits until nothing is in the air and
            // nothing is being landed. Every point that settled before
            // comes here, so each still sees title memory, fences and
            // bottom-of-pipe values as after a settle.
            relay_wait(why, |line| line.line.is_empty() && !line.settling);
            return;
        }
        let was_working = working(true);
        let mut counted = false;
        loop {
            let items = {
                let mut line = line();
                if line.line.is_empty() {
                    line.settling = false;
                    break;
                }
                line.settling = true;
                line.written.clear();
                line.lists = 0;
                std::mem::take(&mut line.line)
            };
            let flights = items.iter().filter(|i| matches!(i, Late::Run(_))).count();
            if !counted {
                counted = true;
                LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
                LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
            }
            LATE_STATS[11].fetch_add(flights as u64, Ordering::Relaxed);
            if flights > 0 {
                submitting(2, flights);
                let waiting = std::time::Instant::now();
                // SAFETY: the fence and event are ours, under the
                // submission lock; `submitted` is the last value signalled.
                unsafe {
                    match self.fence.SetEventOnCompletion(*submitted, self.event) {
                        Ok(()) => {
                            working(false);
                            WaitForSingleObject(self.event, INFINITE);
                            stood(waiting);
                            working(true);
                        }
                        Err(e) => tracing::warn!(error = %e, "submissions sent ahead not waited for"),
                    }
                }
                let gpu = waiting.elapsed();
                LATE_STATS[12].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                LATE_WAITED[why.min(10)].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                crate::copystat::card_waited(gpu.as_micros() as u64, || {
                    format!("{flights} submissions sent ahead, settled together")
                });
                // SAFETY: a plain query.
                if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                    let runs: Vec<&HostList> = items
                        .iter()
                        .filter_map(|i| match i {
                            Late::Run(f) => Some(f),
                            _ => None,
                        })
                        .flat_map(|f| f.order.iter().map(|&i| f.lists[i].host()))
                        .collect();
                    fail(format_args!(
                        "host GPU device removed after {flights} submissions sent ahead (VICTORY_LATE_WAIT=1) of {} lists{}{}",
                        runs.len(),
                        self.removal_data(&runs),
                        last_notes(&runs)
                    ));
                }
                submitting(3, flights);
                let copying = std::time::Instant::now();
                let mut seen = std::collections::HashSet::new();
                let mut back: Vec<Declared> = Vec::new();
                for item in &items {
                    let Late::Run(flight) = item else {
                        continue;
                    };
                    if let Some(card) = &flight.card {
                        self.card_back(card, submitted, &mut seen);
                    }
                    for (shadow, ranges) in &flight.back {
                        match back.iter_mut().find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow)) {
                            Some((_, all)) => {
                                for range in ranges {
                                    if !all.contains(range) {
                                        all.push(*range);
                                    }
                                }
                            }
                            None => back.push((shadow.clone(), ranges.clone())),
                        }
                    }
                }
                hand_back(&back);
                COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            recopy_written_sources();
            // In the order given: what tells the title the card is done.
            let mut last = 0;
            let mut landed = Vec::new();
            for item in items {
                match item {
                    Late::Run(flight) => {
                        last = flight.number;
                        for &i in &flight.order {
                            read_backs(flight.lists[i].host());
                        }
                        if let Some(card) = flight.card {
                            self.card_done(card);
                        }
                        landed.push((flight.lists, flight.order));
                    }
                    Late::Write(at, what, bytes) => what.write(at as *mut u8, bytes),
                    Late::Signal(fence, value) => crate::fence::set(fence as crate::object::This, value),
                }
            }
            if last > 0 {
                FINISHED.store(last, Ordering::Release);
                bury(last);
                copies_settled(last);
            }
            line().landed.extend(landed);
        }
        if counted {
            self.log_messages();
            submitting(0, 0);
            // What was settled may be what a queue stands at, and the
            // caller may not be one that looks (a read-back, a present).
            if why != WHY_CARD_DONE {
                line().poke = true;
                LINE_MOVED.notify_one();
            }
        }
        working(was_working);
    }

    /// [`Host::execute`], or with `late` the first half of
    /// [`Host::execute_late`]: the lists are prepared and sent, and what is
    /// to be done once they have run comes back with the submission lock
    /// still held.
    fn submit<'a>(&'a self, runs: &[&HostList], late: bool) -> Option<(std::sync::MutexGuard<'a, u64>, Sent)> {
        #[cfg(feature = "diagnostics")]
        let called = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_begins();
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let was_working = working(true);
        submitting(1, runs.len());
        // Flights in the air ([`late`]): a submission that is waited for
        // settles them first, one sent ahead only if preparing it reads
        // what they write or would overwrite an in-flight texture staging
        // buffer.
        if !late {
            self.settle(&mut submitted, WHY_SYNC);
        } else if let Some(why) = late_blocked(runs) {
            if relay_exact() && why == WHY_READ {
                // Only until the flights that may write what is read have
                // landed: their copy-back's end, not the card's other work.
                relay_wait(why, |line| blocked_by(&line.written, runs, false).is_none());
            } else {
                self.settle(&mut submitted, why);
            }
        }
        // With the relay exact the wait is at the detile itself, only when a
        // staging buffer a flight still copies from is written again
        // ([`staging_wait`]); without it, flights are settled here, from
        // the submitting thread, before the detiles of this submission.
        if late && !relay_exact() && staging_in_flight(runs) {
            self.settle(&mut submitted, WHY_READ);
        }
        // The title also writes descriptors without calling a descriptor API.
        crate::descriptor::sync_heaps();
        let behind = late && flying();
        // Before this submission counts as started: what the sweep lets go
        // of is named by no list, and nothing runs now (behind flights the
        // sweep waits for a submission that finds none in the air: there is
        // one at every present).
        static SWEPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if !behind {
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        } else if late_free_present() && now_ms() > SWEPT.load(Ordering::Relaxed) + 1500 {
            // With the present sent ahead nothing else brings a moment
            // with nothing in the air: one is made for the sweep.
            self.settle(&mut submitted, WHY_SYNC);
            sweep();
            SWEPT.store(now_ms(), Ordering::Relaxed);
        }
        let behind = late && flying();
        // The slots that waited for a texture's host copy, before the
        // lists that may read them run.
        crate::descriptor::make_waiting();
        let this_submission = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        // The ranges shaders write are counted for it (with the relay).
        crate::shadow::preparing(this_submission);
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let lock = (started - called).as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        // Where each run's lists start in `lists`.
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        // The time a settle of flights takes inside this preparing: the
        // wait for the card and the copy-back have their own places on the
        // frame line (run 92 showed them twice, "preparing" 250 ms for 135).
        let mut settled = std::time::Duration::ZERO;
        // The buffer for the patches of the title's bytes put in under
        // flights, if they are put in (`stale_under_flights`).
        let mut reserve: Option<PatchBuffer> = None;
        // Each phase of the preparing, timed for the frame line
        // ([`PREPARE_SPLIT`]); a settle inside one is not its time.
        let mut phase = std::time::Instant::now();
        let split = |at: usize, since: &mut std::time::Instant, settled_in: std::time::Duration| {
            let us = since.elapsed().saturating_sub(settled_in).as_micros() as u64;
            PREPARE_SPLIT[at].fetch_add(us, Ordering::Relaxed);
            *since = std::time::Instant::now();
        };
        // One write-watch asking for everything the submission refreshes
        // (`VICTORY_ASK_ONCE`): per copy, the span of its refreshed ranges.
        let settled_before = settled;
        if crate::watch::once() {
            let mut spans: std::collections::HashMap<usize, (usize, usize)> = std::collections::HashMap::new();
            for list in runs {
                for step in &list.deferred().copies {
                    if let Step::Refresh(shadow, address, len) = step
                        && !shadow.unwatched()
                    {
                        let span = spans
                            .entry(std::sync::Arc::as_ptr(shadow) as usize)
                            .or_insert((*address, *address + *len));
                        *span = (span.0.min(*address), span.1.max(*address + *len));
                    }
                }
            }
            let ranges: Vec<(usize, usize)> = spans.into_values().collect();
            crate::watch::ask_once(&ranges);
            split(PREPARE_ASK, &mut phase, std::time::Duration::ZERO);
            // Behind flights: the title wrote memory that a flight's shaders
            // may write too, and this submission refreshes it. The flights
            // are settled, and the asking made again: the copy-back wrote
            // title memory, which other copies of it must see.
            if behind && timed(0, || stale_under_flights(self, runs, &mut reserve)) {
                let settling = std::time::Instant::now();
                self.settle(&mut submitted, WHY_STALE);
                settled += settling.elapsed();
                crate::watch::ask_once(&ranges);
            }
        } else if behind && timed(0, || stale_under_flights(self, runs, &mut reserve)) {
            let settling = std::time::Instant::now();
            self.settle(&mut submitted, WHY_STALE);
            settled += settling.elapsed();
        }
        split(PREPARE_MERGE, &mut phase, settled - settled_before);
        let stepping = std::time::Instant::now();
        let mut preludes = std::time::Duration::ZERO;
        for (i, list) in runs.iter().enumerate() {
            list.run_deferred(&mut batch);
            firsts.push(lists.len());
            let earlier = runs[..i].iter().filter(|l| std::ptr::eq(**l, *list)).count();
            let making = std::time::Instant::now();
            if let Some(prelude) = list.prelude(earlier) {
                lists.push(Some(prelude));
            }
            preludes += making.elapsed();
            lists.push(list.list.cast().ok());
        }
        PREPARING_NS[1].fetch_add(stepping.elapsed().as_nanos() as u64, Ordering::Relaxed);
        PREPARE_SPLIT[PREPARE_PRELUDES].fetch_add(preludes.as_micros() as u64, Ordering::Relaxed);
        split(PREPARE_DEFERRED, &mut phase, preludes);
        timed(2, || batch.flush());
        crate::watch::ask_once_end();
        split(PREPARE_FLUSH, &mut phase, std::time::Duration::ZERO);
        // The snapshots of the ranges shaders may write, if they were left
        // for now (`crate::shadow::wide_back`): the refreshes are done, the
        // lists have not gone to the card.
        {
            #[cfg(feature = "diagnostics")]
            let snapshots = std::time::Instant::now();
            crate::shadow::take_snapshots();
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::SnapshotUs, snapshots.elapsed().as_micros() as u64);
        }
        split(PREPARE_SNAPSHOTS, &mut phase, std::time::Duration::ZERO);
        // Copies in the card's memory: what changed on their CPU side goes
        // up ahead of the lists, and a compare of what the shaders may
        // write follows them in the same submission.
        let flown = late && relay();
        let settled_before = settled;
        if flown && self.card_results_low() {
            let settling = std::time::Instant::now();
            if relay_exact() {
                relay_wait(WHY_MOST, |line| room_wait_over(&CARD_WORK, line));
            } else {
                self.settle(&mut submitted, WHY_MOST);
            }
            settled += settling.elapsed();
        }
        let card = timed(3, || self.card_before(runs, flown, reserve, relay()));
        if let Some(check) = card.as_ref().and_then(|c| c.check.clone()) {
            lists.push(Some(check));
        }
        split(PREPARE_CARD, &mut phase, settled - settled_before);
        let prepared = std::time::Instant::now();
        if late {
            // Only the staging buffers this flight copies from on the card:
            // a list that merely samples a texture (its check at submission
            // finding it unchanged) reads no staging buffer, and marking
            // those made every later flight that samples the same textures
            // wait for this one.
            for list in runs {
                for staging in &list.deferred().staging_copies {
                    staging.mark_in_flight(this_submission);
                }
            }
            *submitted += 1;
            // SAFETY: closed lists; the fence is ours.
            let signalled = unsafe {
                self.queue.ExecuteCommandLists(&lists);
                self.queue.Signal(&self.fence, *submitted)
            };
            self.card_sent();
            if let Err(e) = signalled {
                fail(format_args!("a submission sent ahead could not be followed by a signal ({e})"));
            }
            let back = declared_writes(runs);
            let writes = runs.iter().flat_map(|l| l.deferred().writes.clone()).collect();
            SUBMISSION_US[0].fetch_add((prepared - started).saturating_sub(settled).as_micros() as u64, Ordering::Relaxed);
            submitting(0, 0);
            #[cfg(feature = "diagnostics")]
            crate::copystat::submission_ends(lock);
            let sent = Sent {
                number: this_submission,
                card,
                back,
                writes,
            };
            working(was_working);
            return Some((submitted, sent));
        }
        // Diagnostics: each list on its own, timed (`VICTORY_GPU_TIMING`).
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        submitting(2, runs.len());
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            // SAFETY: closed lists; the fence and event are ours.
            unsafe {
                self.queue.ExecuteCommandLists(group);
                self.card_sent();
                let waited = self
                    .queue
                    .Signal(&self.fence, *submitted)
                    .and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
                match waited {
                    Ok(()) => {
                        working(false);
                        WaitForSingleObject(self.event, INFINITE);
                        stood(started);
                        working(true);
                    }
                    Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
                }
            }
            // With timing on each group is one of `runs` (and the compare
            // of copies in the card's memory, with the last): every
            // command's time on the card goes to the sums by pipeline.
            #[cfg(feature = "diagnostics")]
            if timing() {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                for (ms, op) in runs[i].slowest(frequency, usize::MAX) {
                    crate::copystat::command_took(&op, ms);
                }
            }
            // A submission of one list was never named here (`groups.len()
            // > 1` was the test): the two 50 ms lists of the gameplay
            // frame, each a submission of its own, printed nothing.
            #[cfg(feature = "diagnostics")]
            if timing() && started.elapsed() > std::time::Duration::from_millis(50) {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i].notes.lock().unwrap_or_else(|e| e.into_inner()).len();
                tracing::warn!(ms = started.elapsed().as_millis(), commands, "slow host list");
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        FINISHED.store(this_submission, Ordering::Release);
        bury(this_submission);
        let gpu = prepared.elapsed();
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        crate::copystat::card_waited(gpu.as_micros() as u64, || {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            let text = format!("{} lists, pipelines {pipelines:?}", runs.len());
            text.chars().take(1200).collect()
        });
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| l.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .collect();
            tracing::warn!(ms = gpu.as_millis(), lists = runs.len(), ?pipelines, "slow host submission");
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail(format_args!(
                "host GPU device removed after a submission of {} lists{}{}",
                runs.len(),
                self.removal_data(runs),
                last_notes(runs)
            ));
        }
        submitting(3, runs.len());
        // What shaders wrote goes back before the bottom-of-pipe writes,
        // which tell the title the GPU is done; each range once per
        // submission, however many dispatches declared it (the world's
        // passes declare the same big ranges dozens of times).
        let copying = std::time::Instant::now();
        // For copies in the card's memory the compare has run there: the
        // pieces that differ are fetched and merged.
        if let Some(card) = &card {
            self.card_back(card, &mut submitted, &mut std::collections::HashSet::new());
        }
        let back = declared_writes(runs);
        hand_back(&back);
        if let Some(card) = card {
            self.card_done(card);
        }
        #[cfg(feature = "diagnostics")]
        if !back.is_empty() {
            crate::copystat::add(crate::copystat::C::BackSubmissions, 1);
        }
        // Nothing of this submission goes back to the title: no range of
        // shader writes, no value written when it is done. The wait for
        // such a one is only for order and for what the card still holds.
        #[cfg(feature = "diagnostics")]
        if back.is_empty() && runs.iter().all(|l| l.deferred().writes.is_empty()) {
            use crate::copystat::{C, add};
            add(C::NothingBack, 1);
            add(C::NothingBackUs, gpu.as_micros() as u64);
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        for list in runs {
            // Read backs first: a value written at the bottom of the pipe
            // may be what tells the title that they are there.
            read_backs(list);
            for &(at, what, bytes) in &list.deferred().writes {
                what.write(at as *mut u8, bytes);
            }
        }
        self.log_messages();
        submitting(0, 0);
        #[cfg(feature = "diagnostics")]
        crate::copystat::submission_ends(lock);
        // With the relay: nothing is in the air now, so no fetch still to
        // be merged is older than the sends to card copies so far, and
        // their notes can go.
        if relay() {
            crate::shadow::relay_fetched(*QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        }
        working(was_working);
        None
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    fn removal_data(&self, runs: &[&HostList]) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // Every list's progress (done/count), in the runtime's order.
            let mut progress = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                let ours = p.pCommandList.as_ref().map(|l| l.as_raw());
                let index = runs.iter().position(|r| Some(r.list.as_raw()) == ours);
                progress.push(format!("{done}/{} {index:?}", p.BreadcrumbCount));
                node = p.pNext;
            }
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            let mut plain = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = plain.as_ref() {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let list = p.pCommandList.as_ref().map(|l| l.as_raw());
                    let run = runs.iter().find(|r| Some(r.list.as_raw()) == list);
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
                plain = p.pNext;
            }
        }
        text
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// Whether the runtime accepts anisotropic filtering with point mipmaps.
    pub(crate) fn aniso_point_mip(&self) -> bool {
        static SUPPORTED: OnceLock<bool> = OnceLock::new();
        *SUPPORTED.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
            // SAFETY: the feature matches the output structure and its size.
            unsafe {
                self.device.CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS19,
                    (&raw mut options).cast(),
                    std::mem::size_of_val(&options) as u32,
                )
            }
            .is_ok()
                && options.AnisoFilterWithPointMipSupported.as_bool()
        })
    }

    /// A CPU-only descriptor held exclusively until recording finishes.
    pub(crate) fn with_cpu_view<T>(&self, record: impl FnOnce(D3D12_CPU_DESCRIPTOR_HANDLE) -> T) -> T {
        let heap = self.cpu_view.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the heap is live and cannot be reused before the call returns.
        record(unsafe { heap.GetCPUDescriptorHandleForHeapStart() })
    }

    /// A host render target view of `resource`, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: Option<&ID3D12Resource>,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource, or none with a
        // description (a typed null view).
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: STAMPS, NodeMask: 0 },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (count + 1) * 8 };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (ticks as f64 * 1e3 / frequency.max(1) as f64, notes[i].0.chars().take(300).collect())
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    pub(crate) fn close(&self) {
        self.twins_to_depth();
        let scratch = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((scratch, _)) = scratch {
            self.indirect_common(&scratch);
        }
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count = (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    /// Only after the list's last submission finished (every one is waited
    /// for).
    pub(crate) fn reset(&self) {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self._alive.used();
        self.deferred().clear();
        self.states.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.twins.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
            // Every submission of the list has run: nothing reads its
            // scratch memory any more, and the next list to ask takes it.
            if scratch_pool() {
                give_chunks(std::mem::take(&mut scratch.chunks));
            }
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::ResetNs, started.elapsed().as_nanos() as u64);
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let texture = crate::resource::host_resource(resource)?;
        let copy = crate::resource::host_copy(resource)?;
        let wanted = legal_state(texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.entry(std::sync::Arc::as_ptr(&copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some((texture, before))
            }
        }
    }

    /// Moves host texture `copy` to `wanted` (a host state) in this list,
    /// as [`HostList::transition`] does a title resource's host copy, and
    /// records the barrier: for textures that are not a title resource's
    /// host copy (colour twins) and for the copies between the two.
    fn settle(&self, copy: &std::sync::Arc<crate::resource::HostTexture>, wanted: u32) {
        let wanted = legal_state(&copy.texture, wanted);
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let before = match states.entry(std::sync::Arc::as_ptr(copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy.clone()));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some(before)
            }
        };
        drop(states);
        if let Some(before) = before {
            let barrier = transition(
                &copy.texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(wanted as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { self.list.ResourceBarrier(&[barrier]) };
        }
    }

    /// A use of title texture `resource` about to be recorded, if it is a
    /// depth texture with a colour twin (false if it is not): of the twin
    /// (`colour`: unordered access) or of the depth texture (depth target,
    /// clear, shader read, copy, resolve), writing it or only reading. The
    /// side used is first brought up to date: by a copy recorded here if
    /// this list wrote the other side since, by the list's prelude if the
    /// list uses the side as it finds it. The twin is left in the
    /// unordered access state; the caller moves the depth texture to the
    /// state its use needs, as for any texture. A twin still ahead when the
    /// list closes is copied then (`twins_to_depth`).
    /// shortcut: every depth target bound for a draw counts as written
    /// (also when the draw only tests), and every unordered access view a
    /// shader names as written; views a shader picks from the heap by a
    /// computed index are not seen here at all.
    pub(crate) fn twin_use(
        &self,
        resource: crate::object::This,
        colour: bool,
        write: bool,
    ) -> bool {
        self.twin_use_inner(resource, colour, write, false).0
    }

    /// For the depth target of a draw about to be bound (before
    /// `State::bind`): [`HostList::twin_use`] as a write of the depth side,
    /// and if the depth plane's twin holds what the plane lacks, the pass
    /// that draws it into the plane is recorded here, before the draw that
    /// tests the plane. True if it was: the list's heap, root signature,
    /// pipeline, targets, viewport, scissor and topology are then the
    /// pass's, and the caller sets its own again (`State::replay`).
    pub(crate) fn twin_use_now(&self, resource: crate::object::This) -> bool {
        self.twin_use_inner(resource, false, true, true).1
    }

    /// [`HostList::twin_use`], and with `now` the depth plane's pass
    /// recorded at once ([`HostList::twin_use_now`]): whether the texture
    /// has a twin, and whether the pass was recorded.
    fn twin_use_inner(
        &self,
        resource: crate::object::This,
        colour: bool,
        write: bool,
        now: bool,
    ) -> (bool, bool) {
        const UNORDERED_ACCESS: u32 = 0x8;
        // Asked for every texture a draw reads: most have no twin.
        let Some(twin) = crate::resource::twin(resource) else {
            return (false, false);
        };
        let Some(depth) = crate::resource::host_copy(resource) else {
            return (false, false);
        };
        let side = usize::from(colour);
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let u = twins
            .entry(std::sync::Arc::as_ptr(&twin) as usize)
            .or_insert_with(|| TwinUse {
                depth: depth.clone(),
                twin: twin.clone(),
                needs: [false; 2],
                behind: [None; 2],
                writing: false,
                late: false,
            });
        let copy = match u.behind[side] {
            Some(behind) => behind,
            None => {
                u.needs[side] = true;
                false
            }
        };
        u.behind[side] = Some(false);
        match (write, colour) {
            // Shaders write the twin when the draw or dispatch runs: what
            // the same one reads of the depth texture is prepared before
            // (`twins_written`).
            (true, true) => u.writing = true,
            (true, false) => u.behind[1] = Some(true),
            (false, _) => {}
        }
        // The depth plane's twin drawn into the plane: before a draw binds
        // the depth texture (`now`), so that the title's class draws, which
        // test the plane with "equal", see this frame's classes. Elsewhere
        // (copies, clears, shader reads) it is not: the pass sets a pipeline
        // and a target of ours, which the title's state there would not get
        // back. That use sees the plane as it was; the pass runs at the next
        // such bind or when the list closes (`twins_to_depth`). Counted.
        let behind_plane = !colour && twin.plane.is_some() && (copy || u.late);
        let pass_now = now && behind_plane;
        if pass_now {
            u.late = false;
        } else if copy && !colour && twin.plane.is_some() {
            u.late = true;
            crate::resource::PLANES[2].fetch_add(1, Ordering::Relaxed);
        }
        drop(twins);
        if copy {
            self.twin_copy(&depth, &twin, colour);
        } else if colour {
            self.settle(&twin.colour, UNORDERED_ACCESS);
        }
        // The depth plane's twin is written through unordered access too.
        if colour && let Some(plane) = &twin.plane {
            self.settle(plane, UNORDERED_ACCESS);
        }
        if pass_now {
            self.pass_plane(&depth, &twin);
        }
        (true, pass_now)
    }

    /// Records the pass that draws the twin of the depth plane of `depth`
    /// into the plane, if `twin` has one, with the moves to the states the
    /// pass needs; the twin is left in the unordered access state. The
    /// pass leaves its own state set (`record_plane_pass`): the title's is
    /// set again after it (`State::replay`), or nothing of the title's
    /// follows (the list closes).
    fn pass_plane(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const DEPTH_WRITE: u32 = 0x10;
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        let Some(plane) = &twin.plane else { return };
        self.settle(depth, DEPTH_WRITE);
        self.settle(plane, PIXEL_SHADER_RESOURCE);
        record_plane_pass(&self.list, &depth.texture, &plane.texture);
        self.settle(plane, UNORDERED_ACCESS);
    }

    /// Records the copy of a depth texture into its colour twin (`colour`)
    /// or of the twin into the depth texture, with the moves to the copy
    /// states. The twin is left in the unordered access state.
    fn twin_copy(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
        colour: bool,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let (from, to) = if colour {
            (depth, &twin.colour)
        } else {
            (&twin.colour, depth)
        };
        self.settle(from, COPY_SOURCE);
        self.settle(to, COPY_DEST);
        record_twin_copy(&self.list, &depth.texture, twin, colour);
        self.settle(&twin.colour, UNORDERED_ACCESS);
    }

    /// Before the list closes: the depth textures whose twins it wrote last
    /// are given what was written, and left readable by shaders. Shaders
    /// that pick a view from the heap by a computed index are not seen by
    /// `twin_use` and read the depth texture in lists that never name it
    /// (in a captured frame of the title no table names a shadow
    /// map for reading but the pass that fills the next one).
    fn twins_to_depth(&self) {
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let written: Vec<_> = twins
            .values_mut()
            .filter(|u| u.behind[0] == Some(true) || u.late)
            .map(|u| {
                // Not copied again if the list copied since the last write
                // and only the depth plane's pass is left.
                let copy = u.behind[0] == Some(true);
                u.behind[0] = Some(false);
                u.late = false;
                (u.depth.clone(), u.twin.clone(), copy)
            })
            .collect();
        drop(twins);
        for (depth, twin, copy) in &written {
            if *copy {
                self.twin_copy(depth, twin, false);
            }
            // Nothing of the title's is recorded after this: the pass may
            // leave its own pipeline and target set.
            self.pass_plane(depth, twin);
            self.settle(depth, SHADER_RESOURCE);
        }
    }

    /// The draw or dispatch just prepared is recorded: the twins it writes
    /// now hold what their depth textures lack.
    pub(crate) fn twins_written(&self) {
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values_mut().filter(|u| u.writing) {
            u.writing = false;
            u.behind[0] = Some(true);
        }
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with; the textures' real
    /// states become those the list ends with. For depth textures with a
    /// colour twin it first copies the side the list uses as it finds it
    /// from the other, if a list submitted earlier left it behind.
    /// `earlier`: how many times the submission names this list before
    /// this one; those preludes are queued, so this one is another list.
    fn prelude(&self, earlier: usize) -> Option<ID3D12CommandList> {
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let host_state = |state: u32| D3D12_RESOURCE_STATES(state as i32);
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        // The copies (depth texture, twin, whether into the twin); the
        // sides are then as this list leaves them.
        let mut syncs = Vec::new();
        let twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values() {
            let real = u.twin.behind.load(Ordering::Relaxed);
            let mut behind = [real & 1 != 0, real & 2 != 0];
            for side in [0, 1] {
                if u.needs[side] && behind[side] {
                    syncs.push((u.depth.clone(), u.twin.clone(), side == 1));
                    behind = [false; 2];
                }
            }
            let left = [0, 1].map(|side| u.behind[side].unwrap_or(behind[side]));
            let left = u32::from(left[0]) | (u32::from(left[1]) << 1);
            u.twin.behind.store(left, Ordering::Relaxed);
        }
        drop(twins);
        // A copy runs with its textures in the copy states: each moves
        // there from its real state, and from there to the state the list
        // starts with (back to its real state if the list does not use it).
        let mut to_copy = Vec::new();
        let mut barriers = Vec::new();
        let mut copying = std::collections::HashMap::new();
        for (depth, twin, colour) in &syncs {
            let (from, to) = if *colour {
                (depth, &twin.colour)
            } else {
                (&twin.colour, depth)
            };
            for (texture, state) in [(from, COPY_SOURCE), (to, COPY_DEST)] {
                let key = std::sync::Arc::as_ptr(texture) as usize;
                let real = texture.state.load(Ordering::Relaxed);
                copying.insert(key, state);
                if real != state {
                    to_copy.push(transition(
                        &texture.texture,
                        host_state(real),
                        host_state(state),
                    ));
                    if !states.contains_key(&key) {
                        barriers.push(transition(
                            &texture.texture,
                            host_state(state),
                            host_state(real),
                        ));
                    }
                }
            }
        }
        barriers.extend(states.iter().filter_map(|(key, (first, last, copy))| {
            let real = copy.state.swap(*last, Ordering::Relaxed);
            let real = copying.get(key).copied().unwrap_or(real);
            (real != *first)
                .then(|| transition(&copy.texture, host_state(real), host_state(*first)))
        }));
        if barriers.is_empty() && syncs.is_empty() {
            return None;
        }
        let own = if earlier == 0 {
            self.prelude.get_or_init(|| host().and_then(|h| h.make_prelude())).clone()
        } else {
            let mut repeats = self.repeats.lock().unwrap_or_else(|e| e.into_inner());
            while repeats.len() < earlier
                && let Some(made) = host().and_then(|h| h.make_prelude())
            {
                repeats.push(made);
            }
            repeats.get(earlier - 1).cloned()
        };
        let Some((allocator, list)) = own.as_ref() else {
            fail(format_args!("no host list for a state prelude"));
        };
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !to_copy.is_empty() {
                    list.ResourceBarrier(&to_copy);
                }
                for (depth, twin, colour) in &syncs {
                    record_twin_copy(list, &depth.texture, twin, *colour);
                }
                if !barriers.is_empty() {
                    list.ResourceBarrier(&barriers);
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    /// As [`HostList::note_op`], for a copy that reads the host GPU range
    /// `(address, bytes)`: the report places the faulting address in it.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        let shaders = traced_shaders();
        if !dred_on() {
            if shaders.is_empty() {
                crate::capture::op(std::ptr::from_ref(self) as usize, what);
            } else {
                let what = what();
                trace_shader_command(&what, shaders);
                crate::capture::op(std::ptr::from_ref(self) as usize, || what);
            }
            return;
        }
        {
            let started = std::time::Instant::now();
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps() && at < STAMPS - 1 {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            trace_shader_command(&what, shaders);
            crate::capture::op(std::ptr::from_ref(self) as usize, || what.clone());
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list.SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
            crate::copystat::add(crate::copystat::C::NoteNs, started.elapsed().as_nanos() as u64);
        }
    }

    /// Whether nothing recorded since the list was opened does any work:
    /// no draw or dispatch (the first keeps its pipeline), no copy, clear,
    /// upload or resolve (they defer a step or move a texture's state), no
    /// bottom-of-pipe write. State set on the list does not count.
    pub(crate) fn untouched(&self) -> bool {
        {
            let deferred = self.deferred();
            if !(deferred.copies.is_empty()
                && deferred.uploads.is_empty()
                && deferred.kept.is_empty()
                && deferred.textures.is_empty()
                && deferred.writes.is_empty()
                && deferred.backs.is_empty())
            {
                return false;
            }
        }
        self.states.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
            && self.twins.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Records a binding and retains the pipeline and its root signature
    /// once until reset. A title can release either before submitting.
    pub(crate) fn keep_pipeline(&self, pipeline: &crate::pipeline::host::HostPipeline) {
        let mut deferred = self.deferred();
        let key = (pipeline.pso.as_raw() as usize, pipeline.root.as_raw() as usize);
        if !deferred.bound_pipelines.insert(key) {
            return;
        }
        deferred.kept.push(pipeline.pso.clone().into());
        deferred.kept.push(pipeline.root.clone().into());
        drop(deferred);
        self.pipelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pipeline.name.clone());
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copies `len` bytes from `src` to `dst` when the list is submitted.
    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        let mut deferred = self.deferred();
        COPIES_JOINED[0].fetch_add(1, Ordering::Relaxed);
        // A copy that goes on where the one recorded just before it ended,
        // source and destination both: with `join_copies` the two are one
        // step.
        if let Some(Step::Copy(d, s, n)) = deferred.copies.last_mut()
            && continues((*d, *s, *n), (dst as usize, src as usize, len))
        {
            COPIES_JOINED[1].fetch_add(1, Ordering::Relaxed);
            if join_copies() {
                *n += len;
                return;
            }
        }
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.refreshed.clear();
    }

    /// Copies `len` bytes of title memory at `src` into the list's upload
    /// memory at `dst` when the list is submitted.
    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len));
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run.
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        self.deferred()
            .copies
            .push(Step::GpuWrite(shadow.clone(), address, len));
    }

    /// A read back: once the list has run, `rows` rows of `row` bytes go
    /// from `buffer` (rows 256-byte pitched, see `PlacedFootprint::host_rows`)
    /// to title memory at `dst`, pitched `pitch`. The buffer lives until
    /// the list is reset.
    pub(crate) fn defer_read_back(&self, buffer: ID3D12Resource, dst: *mut u8, [row, rows, pitch]: [usize; 3]) {
        self.deferred().backs.push((buffer, dst as usize, [row, rows, pitch]));
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    /// Mapped upload memory for a placed texture footprint. The resource,
    /// 512-byte-aligned offset and CPU pointer stay valid until reset.
    pub(crate) fn upload(&self, size: u64) -> Option<(ID3D12Resource, u64, *mut u8)> {
        let alignment = u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT);
        self.scratch_in(size, alignment, |buffer, offset, _, data| (buffer.clone(), offset, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        self.scratch_in(len, 256, |_, _, gpu, data| (gpu, data))
    }

    fn scratch_in<R>(
        &self,
        len: u64,
        alignment: u64,
        use_range: impl FnOnce(&ID3D12Resource, u64, u64, *mut u8) -> R,
    ) -> Option<R> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).checked_next_multiple_of(256)?;
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            let result = use_range(&buffer, 0, gpu, data);
            self.deferred().hold_upload(buffer, len);
            return Some(result);
        }
        // The chunk in use while `len` fits in what is left of it, else the
        // next one (a list's first chunk may be a small one).
        while s.current < s.chunks.len() && s.used.next_multiple_of(alignment) + len > s.chunks[s.current].1 {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let size = if scratch_pool() && s.chunks.is_empty() && len <= SCRATCH_FIRST {
                SCRATCH_FIRST
            } else {
                SCRATCH_CHUNK
            };
            let chunk = take_chunk(host, size)?;
            s.chunks.push((chunk, size));
            s.used = 0;
        }
        let chunk = &s.chunks[s.current].0;
        let at = s.used.next_multiple_of(alignment);
        let data = (chunk.data + at as usize) as *mut u8;
        let result = use_range(&chunk.buffer, at, chunk.gpu + at, data);
        s.used = at + len;
        Some(result)
    }

    /// The bytes of scratch chunks the list holds now (after a reset:
    /// none while lists share their chunks).
    pub(crate) fn scratch_kept(&self) -> u64 {
        let scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        scratch.chunks.iter().map(|c| c.1).sum()
    }

    /// The host address of the list's indirect-work budget, zeroed at each
    /// submission of the list from the first use after a reset on.
    pub(crate) fn budget(&self) -> Option<u64> {
        let (buffer, data) = self
            .budget
            .get_or_init(|| {
                let (b, d) = host()?.shared_buffer(256)?;
                Some((b, d as usize))
            })
            .as_ref()?;
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            deferred.budget_zeroed = true;
            let at = *data;
            // SAFETY: the list's own mapped words, written before it runs.
            // The words after the budget hold what the clamp noted about the
            // list's last run (mesh dispatches: the largest group count
            // asked for, the groups asked in all): taken, then zeroed.
            deferred.copies.push(Step::Host(Box::new(move || unsafe {
                let words = at as *mut u32;
                let (largest, all) = (words.add(1).read_volatile(), words.add(2).read_volatile());
                MESH_GROUPS[0].fetch_max(u64::from(largest), Ordering::Relaxed);
                MESH_GROUPS[1].fetch_add(u64::from(all), Ordering::Relaxed);
                for k in 0..3 {
                    words.add(k).write_volatile(0);
                }
            })));
        }
        // SAFETY: a live buffer.
        Some(unsafe { buffer.GetGPUVirtualAddress() })
    }

    /// A device-local unordered-access buffer of at least `size` bytes for
    /// clamped indirect arguments, reused across executions and resets of the
    /// list. Smaller buffers replaced mid-list stay alive until
    /// [`HostList::reset`].
    pub(crate) fn indirect_scratch(&self, size: u64) -> Option<ID3D12Resource> {
        let needed = size.max(64 << 10);
        let mut slot = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((buffer, cap)) = &*slot
            && *cap >= needed
        {
            return Some(buffer.clone());
        }
        let prev = slot.as_ref().map_or(0, |(_, cap)| *cap);
        let alloc = prev
            .saturating_mul(2)
            .max(needed)
            .next_multiple_of(64 << 10);
        let buffer = host()?.uav_buffer(alloc)?;
        name(&buffer, || format!("indirect arguments {alloc:#x}"));
        let old = slot.replace((buffer.clone(), alloc));
        drop(slot);
        if let Some((old_buf, _)) = old {
            // It ends the list common, as every scratch buffer does.
            self.indirect_common(&old_buf);
            self.keep(&old_buf);
        }
        Some(buffer)
    }

    /// Moves `scratch` back to the common state if a clamp left it readable
    /// as indirect arguments ([`HostList::indirect_readable`]): where the
    /// list's first clamp expects it. Done before the list closes and before
    /// a bigger scratch buffer replaces it: one barrier per list (where
    /// there was one per indirect execution), and the list ends as it
    /// begins however it is submitted.
    fn indirect_common(&self, scratch: &ID3D12Resource) {
        {
            let mut deferred = self.deferred();
            if deferred.indirect_readable != scratch.as_raw() as usize {
                return;
            }
            deferred.indirect_readable = 0;
        }
        let barrier = transition(
            scratch,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COMMON,
        );
        // SAFETY: an open list and a live buffer this recording left in
        // `INDIRECT_ARGUMENT`.
        unsafe { self.list.ResourceBarrier(&[barrier]) };
    }

    /// The state the list's indirect scratch buffer `scratch` is in at this
    /// point of the recording: indirect arguments if a clamp in this
    /// recording left it so ([`HostList::indirect_readable`]), otherwise
    /// common: the state a buffer is made in, and the one a recording
    /// leaves it in when it closes or replaces it ([`HostList::indirect_common`]).
    pub(crate) fn indirect_state(&self, scratch: &ID3D12Resource) -> D3D12_RESOURCE_STATES {
        if self.deferred().indirect_readable == scratch.as_raw() as usize {
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT
        } else {
            D3D12_RESOURCE_STATE_COMMON
        }
    }

    /// A clamp recorded in this list left `scratch` readable as indirect
    /// arguments. It stays so for every execution that reads it, until the
    /// next clamp moves it back to unordered access.
    pub(crate) fn indirect_readable(&self, scratch: &ID3D12Resource) {
        self.deferred().indirect_readable = scratch.as_raw() as usize;
    }

    /// Keeps `object` alive until the list is reset.
    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(object);
        }
    }

    /// Whether this is the first time since the reset that the list asks
    /// about `key` (a texture to bring up to date).
    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Run(Box::new(work)));
        self.deferred().refreshed.clear();
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    /// Brings a texture's staging buffer up to date when the list is
    /// submitted, keeping it alive until the list is reset.
    pub(crate) fn defer_texture(&self, staging: std::sync::Arc<crate::texture::Staging>) {
        self.deferred().copies.push(Step::Texture(staging));
    }

    /// The list's commands copy from `staging` on the card: its flight is
    /// noted in it at submission ([`staging_wait`]).
    pub(crate) fn note_staging_copy(&self, staging: std::sync::Arc<crate::texture::Staging>) {
        self.deferred().staging_copies.push(staging);
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges).
    fn run_deferred(&self, batch: &mut Batch) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        for step in &self.deferred().copies {
            let mut started = std::time::Instant::now();
            let copied = match step {
                &Step::Copy(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    // The pending refreshes this flushes count their own
                    // time (`Batch::update`): the copy's starts after them.
                    batch.title_written();
                    started = std::time::Instant::now();
                    copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    copy_ran(dst, src, len);
                    // With the submission's one asking: the journal learns it.
                    crate::watch::wrote(dst, len);
                    len
                }
                &Step::Fill(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    fill_ran(src, len);
                    len
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Refreshes, 1);
                        add(C::RefreshBytes, *len as u64);
                    }
                    batch.add(shadow, *address, *len);
                    continue;
                }
                Step::Run(work) | Step::Host(work) => {
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    work();
                    // It wrote title memory, where is not known here: the
                    // submission's one asking ends, later refreshes ask anew.
                    if matches!(step, Step::Run(_)) {
                        crate::watch::ask_once_end();
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::GpuWrite(shadow, address, len) => {
                    // The shaders start from the title's data: this copy's
                    // pending refresh first. The other copies' wait for
                    // the next flush of all (flushing all of them at every
                    // range a dispatch writes made thousands of write-watch
                    // askings a frame).
                    batch.flush_one(shadow);
                    #[cfg(feature = "diagnostics")]
                    let snapshot = std::time::Instant::now();
                    shadow.writing(*address, *len);
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::GpuWrites, 1);
                        add(C::SnapshotUs, snapshot.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::Texture(staging) => {
                    if batch.texture_due(staging) {
                        staging.refresh();
                    }
                    continue;
                }
            };
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
        }
    }
}

/// Whether a submission's refreshes of host copies run on several threads,
/// a copy to a thread (`VICTORY_WIDE_REFRESH`, any value but `0`; off by
/// default).
///
/// A submission brings some sixty copies up to date, one after the other
/// on the submitting thread: for each, the pages the title wrote are asked
/// for and read through the kernel into the copy. That is the frame line's
/// "copies", 45-50 ms a frame, and the copies have nothing to do with one
/// another: each refresh writes its own copy and its own notes, and reads
/// title memory. With the switch the copies of a flush (four or more) are
/// spread over the threads kept for the snapshots (`crate::shadow::wide`).
/// The write-watch journal and the guards are behind their own locks, as
/// they are for the title's threads.
fn wide_refresh() -> bool {
    // Live ([`crate::live`]).
    static ON: crate::live::Live = crate::live::Live::new("VICTORY_WIDE_REFRESH", false);
    ON.on()
}

/// A submission's refreshes not done yet, merged per buffer copy into the
/// range that covers them: done before a step that writes title memory or
/// gives a range to shaders, and before the lists run.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
#[derive(Default)]
struct Batch {
    refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    /// Where each copy (by address) is in `refreshes`: the list that makes
    /// the heap's buffers ready adds some 4,100 copies, and searching them
    /// for every refresh of a submission was tens of millions of
    /// comparisons a frame.
    index: std::collections::HashMap<usize, usize>,
    /// Staging buffers (by address) refreshed since the last title write.
    textures: std::collections::HashSet<usize>,
}

impl Batch {
    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        match self.index.get(&key) {
            Some(&at) => {
                let r = &mut self.refreshes[at];
                (r.1, r.2) = (r.1.min(address), r.2.max(end));
            }
            None => {
                self.index.insert(key, self.refreshes.len());
                self.refreshes.push((shadow.clone(), address, end));
            }
        }
    }

    /// Before shaders are given a range of `shadow`'s copy: its pending
    /// refresh is done, the others stay pending.
    fn flush_one(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>) {
        let Some(at) = self.index.remove(&(std::sync::Arc::as_ptr(shadow) as usize)) else {
            return;
        };
        let (shadow, from, to) = self.refreshes.swap_remove(at);
        // The last entry took its place.
        if let Some(moved) = self.refreshes.get(at) {
            self.index.insert(std::sync::Arc::as_ptr(&moved.0) as usize, at);
        }
        Self::update(&shadow, from, to);
    }

    /// Brings `[from, to)` of a copy up to date, timed.
    fn update(shadow: &crate::shadow::Shadow, from: usize, to: usize) {
        let us = Self::update_counting_bytes(shadow, from, to);
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
    }

    /// [`Self::update`] without adding its time to the frame line (the
    /// caller counts the time once, for refreshes run side by side); its
    /// microseconds come back.
    fn update_counting_bytes(shadow: &crate::shadow::Shadow, from: usize, to: usize) -> u64 {
        let started = std::time::Instant::now();
        let copied = shadow.update(from, to - from);
        let us = started.elapsed().as_micros() as u64;
        #[cfg(feature = "diagnostics")]
        crate::copystat::range(from, to - from, us, 0);
        PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
        us
    }

    /// Whether `staging` is still to be refreshed in this submission (and
    /// notes it refreshed).
    fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures.insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    fn title_written(&mut self) {
        self.flush();
        self.textures.clear();
    }

    fn flush(&mut self) {
        self.index.clear();
        // One write-watch asking for the copies that lie close together,
        // not one per copy; none while the submission's one asking stands,
        // where it would ask nothing (the ranges were gathered and sorted
        // for every flush only to be dropped).
        let ahead = self.refreshes.len() > 1 && !crate::watch::asking_stands();
        if ahead {
            let started = std::time::Instant::now();
            let ranges: Vec<(usize, usize)> = self
                .refreshes
                .iter()
                .filter(|r| !r.0.unwatched())
                .map(|r| (r.1, r.2))
                .collect();
            crate::watch::ask_ahead(&ranges);
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        if wide_refresh() && self.refreshes.len() >= 4 {
            // Each copy on its own, several at a time: a copy's refresh
            // writes that copy and reads title memory, nothing of another's.
            // The time counted is the time it took, not the threads' sum:
            // the jobs count bytes only, and the time is added once. (It
            // was the threads' sum less the time taken, subtracted from the
            // counter afterwards, which the frame line may have taken to 0
            // meanwhile: the counter then wrapped.)
            let started = std::time::Instant::now();
            let jobs: Vec<Box<dyn FnOnce() + Send>> = self
                .refreshes
                .drain(..)
                .map(|(shadow, from, to)| {
                    Box::new(move || {
                        Self::update_counting_bytes(&shadow, from, to);
                    }) as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::wide_run(jobs);
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        } else {
            for (shadow, from, to) in self.refreshes.drain(..) {
                Self::update(&shadow, from, to);
            }
        }
        if ahead {
            crate::watch::ask_ahead_done();
        }
    }
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped is left alone.
///
/// The source is read through the kernel (`ReadProcessMemory`), which
/// reports an unmapped page instead of faulting. Asking `VirtualQuery` first
/// cost about 2 ms per copy in the title's big pools (it walks the page
/// tables to the region's end), which made copies run at 100-200 MB/s.
pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    // A title destination that is not all mapped is left alone. Asking
    // Windows first (`VirtualQuery`) walks the page tables to the end of
    // the destination's region: in the title's big blocks that is
    // milliseconds a copy, and it was a seventh of the submitting thread's
    // time in gameplay (run 60: 71 of 500 looks). The kernel's copy below
    // says the same when it fails, so the question is only asked then.
    // `VICTORY_COPY_ASK=1` asks first, as before.
    static ASK: OnceLock<bool> = OnceLock::new();
    let ask_first = title_dst
        && *ASK.get_or_init(|| std::env::var_os("VICTORY_COPY_ASK").is_some_and(|v| v == "1"));
    if ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    if title_dst {
        // The kernel writes the destination: it must not be guarded.
        crate::watch::writing(dst as usize, len);
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let whole = read(src, dst, len);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::MoveUs, moved.elapsed().as_micros() as u64);
    if whole {
        return;
    }
    // The copy failed: a source page is not there, or the destination is
    // not all mapped. Then the destination stays as it is (the kernel may
    // have written its first pages).
    if title_dst && !ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    #[cfg(feature = "diagnostics")]
    let sparse = std::time::Instant::now();
    if !title_dst && len >= 64 << 10 {
        read_sparse(src, dst, len);
    } else {
        read_pieces(src, dst, len);
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::SparseUs, sparse.elapsed().as_micros() as u64);
}

/// After a failed bulk read into host memory, skip contiguous holes. Never
/// cache the query across copies: the title may remap the same address.
fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    let mut offset = 0;
    while offset < len {
        let at = src.wrapping_add(offset);
        let to = dst.wrapping_add(offset);
        let remaining = len - offset;
        // SAFETY: the query inspects an address and fills a local structure.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the query inspects an address and fills a local structure.
        let size = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
        let n = (info.BaseAddress as usize).saturating_add(info.RegionSize)
            .saturating_sub(at as usize).min(remaining);
        if size != size_of_val(&info) || n == 0 {
            read_pieces(at, to, remaining);
            break;
        }
        if info.State != MEM_COMMIT || info.Protect == PAGE_NOACCESS {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: this path is only used with a writable host destination.
            unsafe { to.write_bytes(0, n) };
        } else if !read(at, to, n) {
            // A mapping may change between query and read.
            read_pieces(at, to, n);
        }
        offset += n;
    }
}

fn read_pieces(src: *const u8, dst: *mut u8, len: usize) {
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

/// Reads title memory at `src` into `dst`; false if a page of it is not
/// readable.
pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

/// Whether reads and kernel-style write-backs copy in-process guarded by a
/// vectored exception handler (`VICTORY_DIRECT_COPY=0` restores
/// `ReadProcessMemory` / `NtWriteVirtualMemory`).
fn direct_copy_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_DIRECT_COPY").is_none_or(|v| v != "0")
    })
}

#[cfg(target_arch = "x86_64")]
mod direct_copy {
    use std::sync::Once;

    use windows_sys::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, EXCEPTION_GUARD_PAGE, EXCEPTION_IN_PAGE_ERROR, NTSTATUS,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };

    std::arch::global_asm!(
        ".global victory_try_copy",
        ".global victory_try_copy_op",
        ".global victory_try_copy_end",
        ".global victory_try_copy_fault",
        "victory_try_copy:",
        ".seh_proc victory_try_copy",
        "push rsi",
        ".seh_pushreg rsi",
        "push rdi",
        ".seh_pushreg rdi",
        ".seh_endprologue",
        "mov rdi, rcx",
        "mov rsi, rdx",
        "mov rcx, r8",
        "cld",
        "victory_try_copy_op:",
        "rep movsb",
        "victory_try_copy_end:",
        "mov eax, 1",
        "jmp victory_try_copy_done",
        "victory_try_copy_fault:",
        "xor eax, eax",
        "victory_try_copy_done:",
        "pop rdi",
        "pop rsi",
        "ret",
        ".seh_endproc",
    );

    unsafe extern "C" {
        fn victory_try_copy(dst: *mut u8, src: *const u8, len: usize) -> u8;
        static victory_try_copy_op: u8;
        static victory_try_copy_end: u8;
        static victory_try_copy_fault: u8;
    }

    unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: Windows passes valid exception pointers during VEH dispatch.
        let record = unsafe { &*(*info).ExceptionRecord };
        let code = record.ExceptionCode;
        // A guard page too (`PAGE_GUARD`, one-shot): the kernel's copy this
        // replaces reported it as a page it could not read, and left alone
        // it would end the process.
        if code != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            && code != EXCEPTION_IN_PAGE_ERROR as NTSTATUS
            && code != EXCEPTION_GUARD_PAGE as NTSTATUS
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // SAFETY: `ContextRecord` is valid during VEH dispatch.
        let ctx = unsafe { &mut *(*info).ContextRecord };
        let rip = ctx.Rip as usize;
        let start = (&raw const victory_try_copy_op) as usize;
        let end = (&raw const victory_try_copy_end) as usize;
        if rip < start || rip >= end {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // A write fault may race with `watch` re-protecting a block; let
        // `watch` open writable blocks before reporting failure.
        const WRITE: usize = 1;
        if code == EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            && record.NumberParameters >= 2
            && record.ExceptionInformation[0] == WRITE
            && crate::watch::handle_write_fault(record.ExceptionInformation[1])
        {
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        ctx.Rip = (&raw const victory_try_copy_fault) as u64;
        EXCEPTION_CONTINUE_EXECUTION
    }

    fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: `on_fault` only intercepts faults inside `victory_try_copy_op`.
            unsafe {
                AddVectoredExceptionHandler(1, Some(on_fault));
            }
        });
    }

    /// Copies `len` bytes from `src` to `dst` in-process, returning `false` on
    /// an access violation or in-page error at either pointer.
    ///
    /// # Safety
    ///
    /// Any valid mapped bytes covered by `dst[..len]` and `src[..len]` must be
    /// safe for unsynchronized read/write of disjoint buffers.
    pub(super) unsafe fn try_copy(dst: *mut u8, src: *const u8, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        install();
        // SAFETY: `victory_try_copy` preserves callee-saved registers and any
        // fault inside `victory_try_copy_op` jumps to `victory_try_copy_fault`.
        unsafe { victory_try_copy(dst, src, len) != 0 }
    }
}

/// Copies `len` bytes from `src` to `dst` unless a source page is not
/// readable.
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    if direct_copy_enabled() {
        // SAFETY: `dst` is caller-owned writable memory of `len` bytes; faults on
        // `src` are intercepted by `direct_copy::try_copy`.
        return unsafe { direct_copy::try_copy(dst, src, len) };
    }
    read_kernel(src, dst, len)
}

fn read_kernel(src: *const u8, dst: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the destination is mapped writable memory of `len` bytes; the
    // kernel checks the source.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// The kernel's write into a process's memory, `(process, address,
    /// bytes, length, written out)`: an NTSTATUS, negative when a page of
    /// the range is not there or not writable. It changes no protection.
    fn NtWriteVirtualMemory(
        process: *mut std::ffi::c_void,
        base: *mut std::ffi::c_void,
        buffer: *const std::ffi::c_void,
        size: usize,
        written: *mut usize,
    ) -> i32;
}

/// Whether title memory is written with the kernel's own call
/// (`NtWriteVirtualMemory`) and not `WriteProcessMemory`, which first asks
/// what the pages are (as `VirtualQuery` does: about 2 ms a call in the
/// title's big pools, see [`copy_mapped`]): putting back what shaders wrote
/// cost 1.2-1.9 s a frame for 430-800 pieces, 2 ms each whatever their size
/// (run 24). On by default; `VICTORY_DIRECT_WRITES=0` writes as before.
fn direct_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DIRECT_WRITES").is_none_or(|v| v != "0"))
}

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let direct = direct_writes();
    #[cfg(target_arch = "x86_64")]
    let in_process = direct && direct_copy_enabled();
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        #[cfg(target_arch = "x86_64")]
        if in_process {
            // SAFETY: `from` is our mapped host copy of `n` bytes; guards were
            // opened by the caller (`crate::watch::writing`) or by `on_fault`.
            return unsafe { direct_copy::try_copy(at as *mut u8, from, n) };
        }
        let mut done = 0;
        if direct {
            // SAFETY: as below; the kernel checks the destination and
            // writes nothing it cannot (the caller opened its guards:
            // `crate::watch::writing`).
            return unsafe {
                NtWriteVirtualMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) >= 0
            };
        }
        // SAFETY: the source is our mapped host copy; the kernel checks the
        // destination.
        unsafe { WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0 }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

impl Host {
    /// A buffer in the upload heap, mapped for writing.
    /// A buffer of `size` bytes the card writes and we read (a read back),
    /// in the state copies into it need.
    pub(crate) fn read_back_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a read-back buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no read-back buffer");
                None
            }
        }
    }

    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[0].fetch_add(1, Ordering::Relaxed);
        MADE[1].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no upload buffer");
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's.
fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

const BUFFERS: u32 = 2;

#[derive(Default)]
struct Presentation {
    presenter: Option<Presenter>,
    next_attempt: Option<std::time::Instant>,
}

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    /// With presents sent ahead ([`late_free_present`]): a list for each
    /// present in the air, and the submission it last flew as (it is used
    /// again once that one has finished).
    ahead: Vec<(HostList, u64)>,
    hwnd: HWND,
    meter: Meter,
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds.
/// How often the frame lines are logged (each holds the last second's
/// numbers): `VICTORY_METER_MS`, 5 s by default; 1 s logs every second's,
/// for comparisons over short stretches.
fn meter_period() -> std::time::Duration {
    static PERIOD: OnceLock<std::time::Duration> = OnceLock::new();
    *PERIOD.get_or_init(|| {
        let ms = std::env::var("VICTORY_METER_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(5000u64);
        std::time::Duration::from_millis(ms.max(1000))
    })
}

struct Meter {
    since: std::time::Instant,
    frames: u32,
    #[cfg(feature = "diagnostics")]
    copy_stats_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            #[cfg(feature = "diagnostics")]
            copy_stats_frames: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            caption: None,
        }
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        #[cfg(feature = "diagnostics")]
        crate::copystat::presented();
        self.frames += 1;
        #[cfg(feature = "diagnostics")]
        {
            self.copy_stats_frames += 1;
        }
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        // Of a frame, ms preparing submissions and ms waiting for the GPU.
        let [cpu, gpu] = [0, 1].map(|i| {
            SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
        });
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] = [0, 1].map(|i| {
            COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames
        });
        {
            let p = PREPARE_SPLIT.each_ref().map(|a| a.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
            let rest = cpu - p.iter().sum::<f64>();
            tracing::info!(
                "preparing per frame by phase: write-watch asking {:.1} ms, title bytes put in under flights {:.1} ms, \
                 the lists' deferred steps {:.1} ms (copies and refreshes, here and in the last refreshes, {copy_ms:.1} ms), state preludes {:.1} ms, last refreshes \
                 {:.1} ms, snapshots {:.1} ms, copies in the card's memory {:.1} ms; the rest {rest:.1} ms",
                p[PREPARE_ASK],
                p[PREPARE_MERGE],
                p[PREPARE_DEFERRED],
                p[PREPARE_PRELUDES],
                p[PREPARE_FLUSH],
                p[PREPARE_SNAPSHOTS],
                p[PREPARE_CARD],
            );
        }
        let dropped = crate::draw::take_dropped() as f64 / frames;
        {
            // The same drops by reason (the frame line's one number mixed them).
            let why = crate::draw::take_dropped_why();
            if why.iter().any(|&n| n > 0) {
                let parts: Vec<String> = why
                    .iter()
                    .zip(crate::draw::DROP_REASONS)
                    .filter(|(n, _)| **n > 0)
                    .map(|(&n, what)| format!("{:.1} {what} ({n} in all)", n as f64 / frames))
                    .collect();
                tracing::info!("draws and dispatches dropped per frame, by reason: {}", parts.join(", "));
            }
        }
        let indirect = crate::list::take_indirect() as f64 / frames;
        let mesh_largest = MESH_GROUPS[0].swap(0, Ordering::Relaxed);
        let mesh_groups = MESH_GROUPS[1].swap(0, Ordering::Relaxed) as f64 / frames;
        tracing::info!(
            "indirect mesh dispatches: the largest group count asked for {mesh_largest} (run: at most 65535), {mesh_groups:.0} groups asked per frame"
        );
        late_line(frames);
        card_clock_line(frames, ms);
        crate::shadow::back_line(frames);
        crate::descriptor::sync_line(frames);
        {
            let [passes, pieces, compares] = [0, 1, 2].map(|i| CARD_PASSES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if passes + compares > 0.0 {
                // What joins the runs hashed again (`rehash_joined`), as it
                // stands: the patch, the switch, or the count of ranges.
                let joined = if CARD_PATCHES.load(Ordering::Relaxed) {
                    "always: the patch of title bytes compiled".to_string()
                } else if rehash_runs() {
                    "always: VICTORY_CARD_REHASH_RUNS=1".to_string()
                } else {
                    format!("past {REHASH_JOIN_FROM} ranges sent to a copy")
                };
                tracing::info!(
                    "our passes over copies in the card's memory per frame: {passes:.0} hash passes over {pieces:.0} pieces after \
                     ranges were sent up (their runs joined {joined}), {compares:.0} compare passes"
                );
            }
        }
        {
            let [own, asking, still, all] =
                [0, 1, 2, 3].map(|i| crate::texture::REFRESH_WAITS[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
            if all >= 1.0 {
                tracing::info!(
                    "texture refreshes per frame, all threads: {all:.1} ms, of them {own:.1} ms waiting for a texture another thread \
                     is refreshing, {asking:.1} ms asking the write watch and the guards, {still:.1} ms waiting for title memory to \
                     hold still (the runtime remapping it); the rest is reading, hashing and detiling"
                );
            }
            let [through, left] = [0, 1].map(|i| crate::texture::UNLOCKED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if through + left > 0.0 {
                tracing::info!(
                    "texture refreshes that did not wait for title memory to hold still, per frame (VICTORY_TEXTURE_NO_WAIT=1): \
                     {through:.1} read through the kernel, {left:.1} left for the next refresh (a page was not there)"
                );
            }
        }
        {
            let [copies, joined] = [0, 1].map(|i| COPIES_JOINED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if copies > 0.0 {
                tracing::info!(
                    "title buffer copies recorded per frame: {copies:.0}, of them {joined:.0} continue the one before \
                     (VICTORY_JOIN_COPIES=1 makes those one copy with it: now {})",
                    if join_copies() { "on" } else { "off" }
                );
            }
            let [under, steps, flush, card] = [0, 1, 2, 3].map(|i| PREPARING_NS[i].swap(0, Ordering::Relaxed) as f64 / 1e6 / frames);
            tracing::info!(
                "preparing per frame, by part: {under:.1} ms putting title bytes in under flights, {steps:.1} the lists' steps, {flush:.1} flushing their refreshes, {card:.1} the card copies' sends and compare"
            );
            let [unseen, seen, not_view, remapping, fault] = [0, 1, 2, 3, 4].map(|i| BACK_WRITES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if back_alias() {
                tracing::info!(
                    "copy-back writes per frame (VICTORY_BACK_ALIAS=1): {unseen:.0} through our own view of the section, {seen:.0} through the title's address ({not_view:.0} not view memory, {remapping:.0} while the runtime remapped, {fault:.1} faulted)"
                );
            }
            let [through, again, fills, stale] = [0, 1, 2, 3].map(|i| COPY_NO_WAIT[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if through + again + fills + stale > 0.0 {
                tracing::info!(
                    "reads of memory that flights may write, per frame (VICTORY_COPY_NO_WAIT=1): {through:.1} title buffer copies ran without waiting, {again:.2} of them made again after a copy-back wrote the source; {fills:.1} fills of upload memory ran without waiting, {stale:.2} had a copy-back write their source after"
                );
            }
        }
        {
            let p = crate::draw::HEAP_PHASES.each_ref().map(|a| a.swap(0, Ordering::Relaxed) as f64);
            let ms = |i: usize| p[i] / 1e3 / frames;
            tracing::info!(
                "heap pass per frame: buffer slots {:.1} ms, buffer refreshes {:.1} ms, texture queue {:.1} ms, \
                 mapped bytes {:.1} ms, views and host textures made {:.1} ms, uploads recorded {:.1} ms, rest {:.1} ms; \
                 {:.0} texture slots looked at, {:.1} uploaded; passes ended by the time {:.2}, by the bytes {:.2}",
                ms(0),
                ms(1),
                ms(2),
                ms(3),
                ms(4),
                ms(5),
                ms(6),
                p[7] / frames,
                p[8] / frames,
                p[9] / frames,
                p[10] / frames
            );
        }
        // Passes over the heap's buffer views for shaders that pick them by index.
        let picked = crate::draw::HEAP_BUFFER_PASSES.swap(0, Ordering::Relaxed) as f64 / frames;
        let picked_ms =
            crate::draw::HEAP_BUFFER_MICROS.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
        // Copies between depth textures and their colour twins, each way.
        let [twin_to_depth, twin_to_colour] =
            [2, 3].map(|i| crate::resource::TWINS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        // Depth planes drawn from their twins.
        let planes = crate::resource::PLANES[0].swap(0, Ordering::Relaxed) as f64 / frames;
        // Copies in the card's memory (`crate::shadow::cards`).
        let card = crate::shadow::card_stats();
        #[cfg(feature = "diagnostics")]
        let slow = crate::watch::slow_asks();
        // Over the same frames as the counts above.
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::take_drops();
        if self.logged.elapsed() >= meter_period() {
            self.logged = std::time::Instant::now();
            tracing::info!(
                "placed texture views: {} writes of another view of the texture a slot showed (it kept the old view until the new was made)",
                crate::descriptor::host::KEPT_VIEWS.load(Ordering::Relaxed)
            );
            {
                let [queued, due, away, partly, oldest] =
                    [0, 1, 2, 3, 4].map(|i| crate::draw::HEAP_TEXTURES_QUEUE[i].load(Ordering::Relaxed));
                let [same, parts, tails] =
                    [0, 1, 2].map(|i| crate::draw::HEAP_TEXTURES_PARTLY[i].load(Ordering::Relaxed));
                let unannounced = crate::draw::HEAP_TEXTURES_UNANNOUNCED.load(Ordering::Relaxed);
                let [unchanged, reuploaded] =
                    crate::draw::HEAP_TEXTURES_RECHECKED.each_ref().map(|n| n.load(Ordering::Relaxed));
                let [early, bake] = crate::draw::EARLY_TEXTURE_PASSES.each_ref().map(|n| n.load(Ordering::Relaxed));
                tracing::info!(
                    "textures picked by index, the queue: {queued} slots, {due} of them due (the oldest written {oldest} frames ago), \
                     {away} waiting for their memory, {partly} uploaded in part; since the start: {} slots found uploaded already by \
                     the draws that read them through tables, {} writes of tables' slots (due some frames later), {} slots taken out \
                     as read through a table, {parts} uploads of textures not all mapped ({tails} of them without their first page), \
                     {same} looks that found no more mapped, {unannounced} that found more arrived unannounced; \
                     {} slots made stale by a newer view of other memory there, \
                     {} ranges of memory mapped, unmapped or written under texture views ({} dropped), {} slots made due by them, \
                     {} made slots found unmapped and made null; {unchanged} content checks unchanged, \
                     {reuploaded} requested an upload after a change, {:.1} ms checking since start; \
                     {early} early passes and {bake} before decoration bakes",
                    crate::draw::HEAP_TEXTURES_TABLED.load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[0].load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[0].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[2].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[3].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[4].load(Ordering::Relaxed),
                    crate::draw::HEAP_TEXTURE_RECHECK_US.load(Ordering::Relaxed) as f64 / 1e3
                );
            }
            #[cfg(feature = "diagnostics")]
            tracing::info!(
                "write-watch askings of a millisecond or more: {:.1} a frame, {:.1} ms a frame; the longest asking {:.1} ms",
                slow[0] as f64 / frames,
                slow[1] as f64 / 1e3 / frames,
                slow[2] as f64 / 1e3
            );
            if crate::shadow::cards() {
                let per_frame = |i: usize| card[i] as f64 / frames;
                tracing::info!(
                    "host copies in the card's memory: {} copies {} MB, their bytes from before {} MB; per frame: \
                     {:.1} MB sent up in {:.0} ranges, {:.0} pieces compared on the card, {:.0} of them fetched \
                     ({:.1} MB) with {:.1} more waits for the card ({:.1} ms)",
                    card[7],
                    card[8] >> 20,
                    card[9] >> 20,
                    per_frame(0) / 1e6,
                    per_frame(1),
                    per_frame(2),
                    per_frame(3),
                    per_frame(4) / 1e6,
                    per_frame(5),
                    per_frame(6) / 1e3
                );
            }
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} (reasons on their own line; {making} pipelines being made) and \
                 {indirect:.0} indirect; heap buffers {picked:.1} passes {picked_ms:.1} ms;                  texture slots written so far: {placed} placed, {objects} of objects; \
                 textures picked by index: {picked_textures} uploaded so far ({picked_mb} MB), \
                 {picked_waiting} waiting, {picked_away} passed over unmapped; depth textures with                  unordered access: {du0} made, bound as depth {du1}x, cleared as depth {du2}x,                  unordered views refused {du3}x; buffer views left null as the host would refuse them: {refused}",
                picked_textures = crate::draw::HEAP_TEXTURES.load(Ordering::Relaxed),
                picked_mb = crate::draw::HEAP_TEXTURE_BYTES.load(Ordering::Relaxed) >> 20,
                picked_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Ordering::Relaxed),
                picked_away = crate::draw::HEAP_TEXTURES_AWAY.load(Ordering::Relaxed),
                du0 = crate::resource::DEPTH_UNORDERED[0].load(Ordering::Relaxed),
                du1 = crate::resource::DEPTH_UNORDERED[1].load(Ordering::Relaxed),
                du2 = crate::resource::DEPTH_UNORDERED[2].load(Ordering::Relaxed),
                du3 = crate::resource::DEPTH_UNORDERED[3].load(Ordering::Relaxed),
                refused = format!(
                    "{}; depth twins: {} alive, {} depth textures left without one; twin copies per frame: {twin_to_depth:.1} into depth textures, {twin_to_colour:.1} into twins; depth planes: {planes:.1} drawn from their twins per frame, {} times not given back to a twin, {} times used before the pass",
                    {
                        // the old total (those the host would refuse), then every reason
                        use crate::descriptor::host::{NULL_BUFFER_VIEWS, NULL_VIEW_KINDS};
                        let n: [u64; NULL_VIEW_KINDS] = NULL_BUFFER_VIEWS.each_ref().map(|c| c.load(Ordering::Relaxed));
                        format!(
                            "{} (empty {}, raw off 16 bytes {}, raw of another format {}, structured {}, typed {}, past the end {}); \
                             also left null: not on an element {}, no host copy {}, unordered and unwritable {}",
                            n[..6].iter().sum::<u64>(),
                            n[0],
                            n[1],
                            n[2],
                            n[3],
                            n[4],
                            n[5],
                            n[6],
                            n[7],
                            n[8]
                        )
                    },
                    crate::resource::TWINS[0].load(Ordering::Relaxed),
                    crate::resource::TWINS[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[2].load(Ordering::Relaxed)
                ),
                making = crate::pipeline::making(),
                placed = crate::descriptor::host::PLACED_TEXTURE_SLOTS.load(Ordering::Relaxed),
                objects = crate::descriptor::host::OBJECT_TEXTURE_SLOTS.load(Ordering::Relaxed)
            );
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::log(std::mem::take(&mut self.copy_stats_frames) as f64);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        // `VICTORY_STATIC_TITLE=1`: a caption that does not change, so that
        // window captures by title work while the game runs (the rate is in
        // the log).
        static STATIC_TITLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let text = if *STATIC_TITLE.get_or_init(|| std::env::var_os("VICTORY_STATIC_TITLE").is_some_and(|v| v == "1")) {
            " | VIctory medindo".to_string()
        } else {
            format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame")
        };
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows `texture`, which is in `state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on). `None` if nothing was
    /// shown; else, in diagnostics builds, whether the frame was black.
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let Some(format) = swap_format(desc.Format) else {
            tracing::warn!(format = desc.Format.0, "cannot present this format");
            return None;
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presentation = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        let Presentation { presenter, next_attempt } = &mut *presentation;
        let should_make = presenter.is_none();
        let should_resize = presenter.as_ref().is_some_and(|p| {
            (p.width, p.height, p.format) != (width, height, format)
        });
        if (should_make || should_resize)
            && next_attempt.is_some_and(|at| std::time::Instant::now() < at)
        {
            return None;
        }
        if should_make {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    *next_attempt = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                    tracing::warn!(error = %e, width, height, format = format.0, "no swap chain");
                    return None;
                }
            };
            *next_attempt = None;
        }
        let p = presenter.as_mut()?;
        if (p.width, p.height, p.format) != (width, height, format) {
            // Presents sent ahead hold back buffers until they have run.
            settle_now(WHY_SYNC);
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    *next_attempt = None;
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    *next_attempt = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
                    tracing::warn!(error = %e, width, height, format = format.0, "swap chain not resized");
                    return None;
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state as i32);
        // SAFETY: our swap chain, list and a live source texture.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            // Sent ahead ([`late_free_present`]) unless the frame is to be
            // read back at once (a saved or listed frame): then a list of
            // its own, one whose last flight has finished or a new one.
            #[cfg(feature = "diagnostics")]
            let read_now = crate::capture::wanted(frame) || crate::capture::listed(frame);
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead.iter().position(|(_, flown)| *flown <= finished).or_else(|| {
                    let list = self.new_list().ok()?;
                    list.close();
                    p.ahead.push((list, 0));
                    Some(p.ahead.len() - 1)
                })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            let list = &copy.list;
            // A texture that already is a copy source stays one: a
            // transition between equal states is not a barrier the host
            // accepts.
            let first = usize::from(state == D3D12_RESOURCE_STATE_COPY_SOURCE);
            list.ResourceBarrier(
                &[
                    transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE),
                    transition(
                        &back,
                        D3D12_RESOURCE_STATE_PRESENT,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    ),
                ][first..],
            );
            list.CopyResource(&back, texture);
            #[cfg(feature = "diagnostics")]
            let capture = crate::capture::wanted(frame)
                .then(|| crate::capture::read_back(&self.device, list, texture))
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = if slot.is_some() { None } else { crate::capture::probe(&self.device, list, texture) };
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            list.ResourceBarrier(
                &[
                    transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state),
                    transition(
                        &back,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        D3D12_RESOURCE_STATE_PRESENT,
                    ),
                ][first..],
            );
            copy.close();
            match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                }
                None => self.execute(&[copy]),
            }
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            p.meter.frame(p.hwnd);
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height)?,
        };
        self.log_video_memory("right before the swap chain is made");
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?
                .cast::<IDXGISwapChain3>()?;
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            self.log_video_memory("right after the swap chain is made");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                ahead: Vec::new(),
                hwnd,
                meter: Meter::new(),
            })
        }
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

impl Host {
    /// The card's memory as the system accounts it to this process, in
    /// bytes: the budget and the use of the card's own memory, then of the
    /// PC's memory shared with the card (where the system moves what does
    /// not fit on the card).
    pub(crate) fn video_memory(&self) -> Option<[u64; 4]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        // The card's budget moved by 256 MB or more since the last look:
        // said at once, with what was in use at the last look, so the log
        // has the moment. (It fell from 11,242 MB to some 10,300 MB with
        // the first world frame in runs 179 to 185, and the memory line,
        // seconds later, was the first to show it. We reserve nothing
        // and make the swap chain once, at the start.)
        static LAST: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];
        let now = [own.Budget, own.CurrentUsage, shared.CurrentUsage];
        let last = [0, 1, 2].map(|i| LAST[i].swap(now[i], Ordering::Relaxed));
        if last[0] != 0 && last[0].abs_diff(own.Budget) >= (256u64 << 20) {
            tracing::info!(
                "the system changed the budget of the card's memory from {} MB to {} MB (frame {}); at the last look {} MB of the card's and {} MB of the PC's shared memory were in use",
                last[0] >> 20,
                own.Budget >> 20,
                crate::frame::current(),
                last[1] >> 20,
                last[2] >> 20
            );
            self.log_video_memory("right after that change");
        }
        Some([own.Budget, own.CurrentUsage, shared.Budget, shared.CurrentUsage])
    }

    /// Both of the system's memory groups for this process, in bytes: for
    /// the card's own memory and then for the PC's memory shared with the
    /// card, the budget, the use, what could still be reserved and what
    /// is reserved (the layer reserves none).
    pub(crate) fn video_memory_groups(&self) -> Option<[[u64; 4]; 2]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        let all = |m: &DXGI_QUERY_VIDEO_MEMORY_INFO| {
            [m.Budget, m.CurrentUsage, m.AvailableForReservation, m.CurrentReservation]
        };
        Some([all(&own), all(&shared)])
    }

    /// Logs [`Self::video_memory_groups`], with `when` it is said.
    pub(crate) fn log_video_memory(&self, when: &str) {
        let Some([own, shared]) = self.video_memory_groups() else {
            return;
        };
        tracing::info!(
            "video memory as the system counts it, {}: the card's own: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved; the PC's shared with the card: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved (the layer reserves none)",
            when,
            own[0] >> 20,
            own[1] >> 20,
            own[2] >> 20,
            own[3] >> 20,
            shared[0] >> 20,
            shared[1] >> 20,
            shared[2] >> 20,
            shared[3] >> 20
        );
    }

    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        MADE[2].fetch_add(1, Ordering::Relaxed);
        MADE[3].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }

    /// A device-local buffer shaders may write (unordered access) and indirect
    /// execution may read, starting in the common state.
    pub(crate) fn uav_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        MADE[4].fetch_add(1, Ordering::Relaxed);
        MADE[5].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a device-local buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no unordered-access buffer");
                None
            }
        }
    }
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!("; list {i} of {}, of its {}: {n}", runs.len(), notes.len()));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
fn in_flight(history: &[D3D12_AUTO_BREADCRUMB_OP], markers: &[(u32, String)], done: usize) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done].iter().rposition(barrier).map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    // From just before the command the GPU had not finished: the first
    // twelve after the barrier named draws long done when a list of 79
    // mesh draws stopped at its 45th (run 41).
    for i in from.max(done.saturating_sub(4))..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod trail {
    /// The trail keeps the newest things, and the report made when the
    /// host device is removed says them and the pipeline library's state
    /// (no card is asked for any of it).
    #[test]
    fn the_removal_report_says_the_newest_things_done() {
        for n in 0..super::TRAIL_MOST + 5 {
            super::trail(|| format!("thing {n} done"));
        }
        let text = super::removal_context();
        assert!(text.contains("the submitting thread was"));
        assert!(text.contains("pipeline library: no call running"));
        assert!(text.contains(&format!("thing {} done", super::TRAIL_MOST + 4)));
        assert!(!text.contains("thing 4 done"), "the oldest entries are dropped");
    }
}

#[cfg(test)]
mod spare {
    use super::Spare;

    /// Kept things come back by size, the last given first, and nothing is
    /// kept past the limit.
    #[test]
    fn kept_things_come_back_by_size_within_the_limit() {
        let mut spare: Spare<u32> = Spare::new();
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.give(1, 4, 10), None);
        assert_eq!(spare.give(2, 1, 10), None);
        assert_eq!(spare.give(3, 4, 10), None);
        assert_eq!(spare.bytes, 9);
        // Past the limit: handed back, nothing changes.
        assert_eq!(spare.give(4, 4, 10), Some(4));
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.take(2), None, "no thing of that size");
        assert_eq!(spare.take(4), Some(3));
        assert_eq!(spare.take(4), Some(1));
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.take(1), Some(2));
        assert_eq!(spare.bytes, 0);
        assert!(spare.kept.is_empty());
    }
}

#[cfg(test)]
mod batch {   // Batch::texture_due lives here; `mod tests` is already taken at the top of this file
    use super::Batch;
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }
}
