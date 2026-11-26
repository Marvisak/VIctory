//! Fences: a 64-bit value and the events and threads waiting for it.
//! The title's Signals, its queues' (once the work before them has run)
//! and bottom-of-pipe writes to a fence's address (see [`written`]) move
//! one.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::object::{self, FENCE, HRESULT, Iid, Object, S_OK, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

pub(crate) const FENCE_IIDS: [Iid; 5] = [
    guid(
        0x0a753dcf,
        0xc4d8,
        0x4b91,
        [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
    ),
    // ID3D12Fence1.
    guid(
        0x433685fe,
        0xe22b,
        0x4ca0,
        [0xa8, 0xdb, 0xb5, 0xb4, 0xf4, 0xdd, 0x0e, 0x4a],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
struct Fence {
    object: Object,
    /// Titles also read it straight from memory, at the fence's GPU
    /// address.
    value: AtomicU64,
    flags: u32,
    /// Events to set once the value reaches theirs (handles as integers),
    /// in the order they were asked for, and since when. Each is set once,
    /// then dropped.
    waiters: Mutex<Vec<(u64, usize, std::time::Instant)>>,
    /// Notified, with `waiters` locked, whenever the value changes: threads
    /// blocked in a wait without an event look at it again.
    changed: Condvar,
}

/// Every live fence by the address of its value, for bottom-of-pipe writes
/// that land on one. Locked before a fence's `waiters`, never after.
static FENCES: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

fn fences() -> MutexGuard<'static, BTreeMap<usize, usize>> {
    FENCES.lock().unwrap_or_else(|e| e.into_inner())
}

impl Fence {
    fn waiters(&self) -> MutexGuard<'_, Vec<(u64, usize, std::time::Instant)>> {
        self.waiters.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// With `waiters` locked, after the value changed: sets (once) the
    /// events whose value it now reaches and wakes blocked waits.
    fn reached(&self, waiters: &mut Vec<(u64, usize, std::time::Instant)>) {
        let value = self.value.load(Ordering::Acquire);
        waiters.retain(|&(wanted, event, since)| {
            let reached = wanted <= value;
            if reached {
                set_event(event);
                WAITED.0.fetch_add(1, Ordering::Relaxed);
                WAITED.1.fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
            !reached
        });
        self.changed.notify_all();
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        fences().remove(&((&raw const self.value) as usize));
    }
}

fn fence<'a>(this: This) -> &'a Fence {
    // SAFETY: called from fence methods only.
    unsafe { &*this.cast::<Fence>() }
}

pub(crate) extern "system" fn create(
    _: This,
    initial: u64,
    flags: u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::debug!(initial, flags, "fence");
    let fence = Box::new(Fence {
        object: Object::new(&FUNCTIONS),
        value: AtomicU64::new(initial),
        flags,
        waiters: Mutex::new(Vec::new()),
        changed: Condvar::new(),
    });
    // Registered before it is handed out; if it is not, `hand_out` drops
    // it and dropping unregisters it.
    let at = (&raw const fence.value) as usize;
    fences().insert(at, (&raw const *fence) as usize);
    object::hand_out(FENCE, fence, iid, &FENCE_IIDS, out)
}

/// After `bytes` bytes were stored at `at` behind the fences' back (a
/// list's bottom-of-pipe write, which titles point at a fence's GPU
/// address): sets the events and wakes the waits of any fence whose value
/// that touched.
pub(crate) fn written(at: usize, bytes: usize) {
    let fences = fences();
    if fences.is_empty() {
        return;
    }
    let first = at.saturating_sub(size_of::<u64>() - 1);
    for (_, &fence) in fences.range(first..at.saturating_add(bytes)) {
        // SAFETY: registered fences are alive: dropping one unregisters it,
        // under the lock held here.
        let fence = unsafe { &*(fence as *const Fence) };
        let mut waiters = fence.waiters();
        fence.reached(&mut waiters);
    }
}

fn set_event(event: usize) {
    #[cfg(windows)]
    // SAFETY: the title passed an event handle.
    unsafe {
        windows_sys::Win32::System::Threading::SetEvent(event as *mut c_void)
    };
    #[cfg(not(windows))]
    let _ = event;
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        FENCE,
        iid,
        object::is(iid, &FENCE_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Fence>(this)
}
extern "system" fn completed(this: This) -> u64 {
    // A title polling for a value: held work that may run now does first.
    crate::queue::pump_held_if_free(crate::queue::Why::Fence);
    fence(this).value.load(Ordering::Acquire)
}

/// How often a blocked wait looks at the value again without being woken:
/// the title may store it from the CPU at the fence's address.
const RECHECK: Duration = Duration::from_millis(1);

/// Sets `event` once the value reaches `value`; a null event blocks the
/// calling thread until then. A wait never gives up: a removed host device
/// ends the process (`host::fail`) and our submissions are done when they
/// return, so, as on the console, only another thread's signal (the
/// title's or a queue's) or a bottom-of-pipe write ends a wait for a value
/// not yet reached.
extern "system" fn set_event_on_completion(this: This, value: u64, event: *mut c_void) -> HRESULT {
    crate::queue::pump_held_if_free(crate::queue::Why::Fence);
    let fence = fence(this);
    let mut waiters = fence.waiters();
    if event.is_null() {
        // The lock is let go while waiting, so signals get through.
        while fence.value.load(Ordering::Acquire) < value {
            waiters = fence
                .changed
                .wait_timeout(waiters, RECHECK)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        return S_OK;
    }
    if fence.value.load(Ordering::Acquire) >= value {
        set_event(event as usize);
    } else {
        waiters.push((value, event as usize, std::time::Instant::now()));
    }
    S_OK
}
/// The title's Signal from the CPU: what queues hold behind a Wait for it
/// may run now (`crate::hold`).
extern "system" fn signal(this: This, value: u64) -> HRESULT {
    let hr = set(this, value);
    crate::queue::pump_held(crate::queue::Why::Fence);
    hr
}

/// Moves a fence to `value` and wakes the events waiting for it (a queue's
/// Signal, when the work before it has run).
pub(crate) fn set(this: This, value: u64) -> HRESULT {
    let fence = fence(this);
    let mut waiters = fence.waiters();
    fence.value.store(value, Ordering::Release);
    fence.reached(&mut waiters);
    S_OK
}

/// Events the title waited on for a fence value, and their summed waits in
/// microseconds (the held-work frame line: work held back shows here as the
/// title's own waits).
static WAITED: (AtomicU64, AtomicU64) = (AtomicU64::new(0), AtomicU64::new(0));

/// Events set since the last call and how long they waited, summed.
pub(crate) fn take_waited() -> (u64, std::time::Duration) {
    let n = WAITED.0.swap(0, Ordering::Relaxed);
    (n, std::time::Duration::from_micros(WAITED.1.swap(0, Ordering::Relaxed)))
}

/// A fence's value.
pub(crate) fn value(this: This) -> u64 {
    fence(this).value.load(Ordering::Acquire)
}

/// Keeps a fence alive while a queue holds a Signal of it, until
/// [`let_go`].
pub(crate) fn keep(this: This) {
    object::add_ref(this);
}
pub(crate) fn let_go(this: This) {
    release(this);
}

extern "system" fn creation_flags(this: This) -> u32 {
    fence(this).flags
}
/// Where the value lives (GPU addresses are CPU addresses).
extern "system" fn gpu_address(this: This) -> u64 {
    (&raw const fence(this).value) as u64
}

/// IUnknown 3, object 4, device child 1, fence 3, fence 1 (2, a console
/// one among them).
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<FENCE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = completed as *const (); // 0x40
    f[9] = set_event_on_completion as *const (); // 0x48
    f[10] = signal as *const (); // 0x50
    f[11] = creation_flags as *const (); // 0x58
    f[12] = gpu_address as *const (); // 0x60
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;
    use std::sync::mpsc::{self, Receiver};

    type Wait = extern "system" fn(This, u64, *mut c_void) -> HRESULT;
    type Signal = extern "system" fn(This, u64) -> HRESULT;

    /// Long enough for a woken wait to return; a broken one fails the test
    /// after it instead of hanging it.
    const WAKES: Duration = Duration::from_secs(10);
    /// How long a wait that should still block is watched.
    const BLOCKS: Duration = Duration::from_millis(50);

    fn new_fence(initial: u64) -> This {
        let mut f = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), initial, 0, &FENCE_IIDS[0], &mut f),
            S_OK
        );
        f
    }

    fn release_fence(f: This) {
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }

    /// `SetEventOnCompletion(value, null)` on another thread; its result
    /// arrives once it returns.
    fn blocked_wait(f: This, value: u64) -> Receiver<HRESULT> {
        let (done, result) = mpsc::channel();
        let f = f as usize;
        std::thread::spawn(move || {
            let f = f as This;
            let _ = done.send(slot::<Wait>(f, 0x48)(f, value, std::ptr::null_mut()));
        });
        result
    }

    #[cfg(windows)]
    fn event() -> *mut c_void {
        use windows_sys::Win32::System::Threading::CreateEventW;
        // SAFETY: a plain auto-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null());
        event
    }

    /// Whether an auto-reset event is set (which resets it).
    #[cfg(windows)]
    fn is_set(event: *mut c_void) -> bool {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        // SAFETY: a valid event.
        unsafe { WaitForSingleObject(event, 0) == WAIT_OBJECT_0 }
    }

    #[cfg(windows)]
    fn close(event: *mut c_void) {
        // SAFETY: ours.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(event) };
    }

    #[cfg(windows)]
    #[test]
    fn signals_set_waiting_events() {
        let f = new_fence(1);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(f, 0x40)(f), 1);
        let event = event();
        let wait = slot::<Wait>(f, 0x48);
        let signal = slot::<Signal>(f, 0x50);
        assert_eq!(wait(f, 5, event), S_OK);
        assert!(!is_set(event));
        assert_eq!(signal(f, 5), S_OK);
        assert!(is_set(event));
        assert_eq!(wait(f, 5, event), S_OK, "already reached");
        assert!(is_set(event));
        // Grand Theft Auto VI reads the value at the fence's address.
        let at = slot::<extern "system" fn(This) -> u64>(f, 0x60)(f);
        // SAFETY: the fence's value.
        assert_eq!(unsafe { *(at as *const u64) }, 5);
        // A new wait after a rewind uses the current fence value.
        assert_eq!(signal(f, 2), S_OK);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(f, 0x40)(f), 2);
        assert_eq!(wait(f, 4, event), S_OK);
        assert!(!is_set(event));
        assert_eq!(signal(f, 4), S_OK);
        assert!(is_set(event));
        close(event);
        release_fence(f);
    }

    /// Each event is set once, when the first signal reaches its value, in
    /// the order asked for; a later signal does not set it again.
    #[cfg(windows)]
    #[test]
    fn events_are_set_once_at_their_value() {
        let f = new_fence(0);
        let wait = slot::<Wait>(f, 0x48);
        let signal = slot::<Signal>(f, 0x50);
        let (three, five, again) = (event(), event(), event());
        assert_eq!(wait(f, 5, five), S_OK);
        assert_eq!(wait(f, 3, three), S_OK);
        assert_eq!(wait(f, 5, again), S_OK);
        assert_eq!(signal(f, 2), S_OK);
        assert!(!is_set(three) && !is_set(five) && !is_set(again));
        assert_eq!(signal(f, 4), S_OK);
        assert!(is_set(three));
        assert!(!is_set(five) && !is_set(again));
        assert_eq!(signal(f, 6), S_OK);
        assert!(is_set(five) && is_set(again));
        assert!(!is_set(three), "set again by a later signal");
        assert_eq!(signal(f, 9), S_OK);
        assert!(!is_set(three) && !is_set(five) && !is_set(again));
        assert!(fence(f).waiters().is_empty());
        for e in [three, five, again] {
            close(e);
        }
        release_fence(f);
    }

    #[test]
    fn waits_without_an_event_return_at_once_when_reached() {
        let f = new_fence(7);
        let wait = slot::<Wait>(f, 0x48);
        assert_eq!(wait(f, 0, std::ptr::null_mut()), S_OK);
        assert_eq!(wait(f, 7, std::ptr::null_mut()), S_OK);
        assert_eq!(blocked_wait(f, 6).recv_timeout(WAKES), Ok(S_OK));
        release_fence(f);
    }

    /// Threads blocked for different values each wake at the first signal
    /// that reaches theirs, and not before.
    #[test]
    fn blocked_waits_wake_when_another_thread_signals() {
        let f = new_fence(1);
        let signal = slot::<Signal>(f, 0x50);
        let five = blocked_wait(f, 5);
        let eight = blocked_wait(f, 8);
        let also_eight = blocked_wait(f, 8);
        assert!(five.recv_timeout(BLOCKS).is_err(), "returned before 5");
        assert_eq!(signal(f, 4), S_OK);
        assert!(five.recv_timeout(BLOCKS).is_err(), "returned at 4");
        let f_ = f as usize;
        // From yet another thread, as the title's render thread would.
        std::thread::spawn(move || signal(f_ as This, 5))
            .join()
            .unwrap();
        assert_eq!(five.recv_timeout(WAKES), Ok(S_OK));
        assert!(eight.recv_timeout(BLOCKS).is_err(), "returned at 5");
        assert_eq!(signal(f, 9), S_OK);
        assert_eq!(eight.recv_timeout(WAKES), Ok(S_OK));
        assert_eq!(also_eight.recv_timeout(WAKES), Ok(S_OK));
        release_fence(f);
    }

    /// A queue's Signal ends a blocked wait as the title's does.
    #[test]
    fn queue_signals_end_blocked_waits() {
        let f = new_fence(0);
        let woken = blocked_wait(f, 2);
        assert!(woken.recv_timeout(BLOCKS).is_err());
        assert_eq!(crate::fence::signal(f, 2), S_OK);
        assert_eq!(woken.recv_timeout(WAKES), Ok(S_OK));
        release_fence(f);
    }

    /// Bottom-of-pipe writes to the fence's address (64 or 32 bits) set
    /// its events and wake its waits; a write next to it does not.
    #[cfg(windows)]
    #[test]
    fn bottom_of_pipe_writes_reach_fence_waiters() {
        use crate::queue::Written;
        let f = new_fence(0);
        let at = slot::<extern "system" fn(This) -> u64>(f, 0x60)(f) as *mut u8;
        let event = event();
        assert_eq!(slot::<Wait>(f, 0x48)(f, 3, event), S_OK);
        let woken = blocked_wait(f, 3);
        assert!(woken.recv_timeout(BLOCKS).is_err());
        let mut elsewhere = [0u64; 2];
        Written::Value(3).write(elsewhere.as_mut_ptr().cast(), 8);
        assert!(!is_set(event));
        Written::Value(3).write(at, 8);
        assert!(is_set(event));
        assert_eq!(woken.recv_timeout(WAKES), Ok(S_OK));

        assert_eq!(slot::<Wait>(f, 0x48)(f, 4, event), S_OK);
        let woken = blocked_wait(f, 4);
        Written::Value(4).write(at, 4);
        assert!(is_set(event));
        assert_eq!(woken.recv_timeout(WAKES), Ok(S_OK));
        assert_eq!(completed(f), 4);
        close(event);
        release_fence(f);
    }

    /// A store from the CPU straight to the value (no signal at all) still
    /// ends a blocked wait.
    #[test]
    fn blocked_waits_see_stores_to_the_value() {
        let f = new_fence(0);
        let woken = blocked_wait(f, 1);
        assert!(woken.recv_timeout(BLOCKS).is_err());
        fence(f).value.store(1, Ordering::Release);
        assert_eq!(woken.recv_timeout(WAKES), Ok(S_OK));
        release_fence(f);
    }

    #[test]
    fn released_fences_are_forgotten() {
        let f = new_fence(0);
        let at = slot::<extern "system" fn(This) -> u64>(f, 0x60)(f) as usize;
        assert!(fences().contains_key(&at));
        release_fence(f);
        assert!(!fences().contains_key(&at));
    }

    #[test]
    fn fence1_interface_is_created_and_queried() {
        const FENCE1_IID: Iid = guid(
            0x433685fe,
            0xe22b,
            0x4ca0,
            [0xa8, 0xdb, 0xb5, 0xb4, 0xf4, 0xdd, 0x0e, 0x4a],
        );
        type Query = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
        type Flags = extern "system" fn(This) -> u32;
        type Release = extern "system" fn(This) -> u32;

        // Direct creation as ID3D12Fence1, and GetCreationFlags at slot 0x58.
        let mut f1 = std::ptr::null();
        assert_eq!(create(std::ptr::null(), 3, 0x5, &FENCE1_IID, &mut f1), S_OK);
        assert!(!f1.is_null());
        assert_eq!(slot::<Flags>(f1, 0x58)(f1), 0x5);
        assert_eq!(slot::<Release>(f1, 0x10)(f1), 0);

        // QueryInterface from ID3D12Fence to ID3D12Fence1 returns the same object.
        let mut f = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), 0, 0x2, &FENCE_IIDS[0], &mut f),
            S_OK
        );
        let mut queried = std::ptr::null();
        assert_eq!(slot::<Query>(f, 0x00)(f, &FENCE1_IID, &mut queried), S_OK);
        assert_eq!(queried, f);
        assert_eq!(slot::<Flags>(queried, 0x58)(queried), 0x2);
        assert_eq!(slot::<Release>(queried, 0x10)(queried), 1);
        assert_eq!(slot::<Release>(f, 0x10)(f), 0);
    }
}
