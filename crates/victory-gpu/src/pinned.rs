//! Title memory that host objects hold in place (#880, decision 0005's
//! unified-memory item), and letting it go before the runtime changes it.
//!
//! A GPU heap opened over title memory (`OpenExistingHeapFromAddress1`)
//! locks its pages: while it lives the memory cannot be protected, unmapped,
//! decommitted or freed (each fails with `ERROR_INVALID_PARAMETER`, on
//! NVIDIA, AMD and WARP alike: the system's rule, not a driver's). The
//! title frees and remaps such memory, and the runtime's own write guards
//! protect it, so whatever holds a heap there registers how to let it go
//! ([`pin`]), and the runtime unmaps, frees, decommits or protects title
//! memory through [`title_memory_change`]: every pin overlapping the range
//! is released first, and the change then succeeds.
//!
//! Releasing is the holder's work: drop the heap and every resource in it
//! once no submission still uses them (a submission running when the title
//! frees memory it reads is the title's race, on the console too; here it
//! waits). The system unlocks the pages when the last reference goes, but
//! not always at once: on an RTX 5070 Ti, 32 of 40 releases were unlocked
//! within 0.01 ms and the slowest after 5.9 ms. So a change refused with
//! `ERROR_INVALID_PARAMETER` after a release is tried again, for up to
//! [`SETTLE`].
//!
//! A pin counts as held until its release has returned: another thread
//! changing an overlapping range meanwhile waits for that release, then
//! retries as above, instead of meeting the memory still locked.
//!
//! Never change title memory through here from the guard handler or under
//! `crate::watch`'s lock: a release may wait for a submission to finish.
//!
//! Cost without pins: one atomic load per change.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError};

/// How long a change refused after a release is tried again: the system
/// unlocks released pages within milliseconds.
const SETTLE: Duration = Duration::from_millis(100);

/// How a holder lets its range go; called once, outside the registry's
/// lock, on the thread about to change the memory.
pub(crate) type Release = Box<dyn FnOnce() + Send>;

/// A registered range, given back with [`Pin::forget`] when its holder lets
/// it go on its own (the range is then no longer released for it).
#[must_use = "a pin forgotten by dropping stays registered until released"]
pub(crate) struct Pin(u64);

impl Pin {
    /// The holder let its range go by itself: nothing to release any more.
    pub(crate) fn forget(self) {
        let mut pins = pins();
        if let Some(key) = pins.by_id.remove(&self.0) {
            pins.ranges.remove(&key);
            PINS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
struct Pins {
    /// (start, id) → (end, release).
    ranges: BTreeMap<(usize, u64), (usize, Release)>,
    /// id → its key in `ranges`.
    by_id: BTreeMap<u64, (usize, u64)>,
    /// The longest range ever registered: how far before a range's start a
    /// pin overlapping it may begin.
    longest: usize,
    /// Pins whose release is running, by id: (start, end, the thread
    /// running it).
    releasing: BTreeMap<u64, (usize, usize, std::thread::ThreadId)>,
}

static REGISTRY: Mutex<Option<Pins>> = Mutex::new(None);
/// Signalled when releases return.
static RELEASED: Condvar = Condvar::new();
/// Pins registered now, those being released among them (the fast path's
/// test).
static PINS: AtomicUsize = AtomicUsize::new(0);
static NEXT: AtomicU64 = AtomicU64::new(1);

struct Guard(MutexGuard<'static, Option<Pins>>);

impl std::ops::Deref for Guard {
    type Target = Pins;
    fn deref(&self) -> &Pins {
        self.0.as_ref().expect("made by pins()")
    }
}

impl std::ops::DerefMut for Guard {
    fn deref_mut(&mut self) -> &mut Pins {
        self.0.as_mut().expect("made by pins()")
    }
}

fn pins() -> Guard {
    let mut g = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Pins::default);
    Guard(g)
}

/// Registers that `[at, at + len)` is held in place, and how to let it go.
pub(crate) fn pin(at: usize, len: usize, release: Release) -> Pin {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let end = at.saturating_add(len);
    let mut pins = pins();
    pins.longest = pins.longest.max(len);
    pins.ranges.insert((at, id), (end, release));
    pins.by_id.insert(id, (at, id));
    PINS.fetch_add(1, Ordering::Relaxed);
    Pin(id)
}

/// Whether anything holds title memory in place now: the runtime's callers
/// skip working out what they change when nothing does.
pub fn title_memory_held() -> bool {
    PINS.load(Ordering::Relaxed) > 0
}

/// The runtime changes title memory at `[at, at + len)` with `change`, a
/// host call that unmaps, frees, decommits or protects it and returns
/// nonzero when it worked (the title's own `VirtualFree` and the like, or
/// the runtime's unmapping of physical pages): whatever holds part of the
/// range in place lets it go first. `change`'s answer, with its last error.
pub fn title_memory_change(at: usize, len: usize, mut change: impl FnMut() -> i32) -> i32 {
    let released = release(at, len);
    let mut worked = change();
    if worked != 0 || released == 0 {
        return worked;
    }
    let start = Instant::now();
    let mut tries = 1;
    // SAFETY: plain reads of this thread's last error.
    while unsafe { GetLastError() } == ERROR_INVALID_PARAMETER && start.elapsed() < SETTLE {
        std::thread::yield_now();
        worked = change();
        tries += 1;
        if worked != 0 {
            tracing::debug!(
                at = format_args!("{at:#x}"),
                len,
                tries,
                ms = start.elapsed().as_secs_f64() * 1e3,
                "released title memory unlocked late"
            );
            break;
        }
    }
    worked
}

/// Lets every holder of part of `[at, at + len)` go, after waiting for
/// other threads' releases there to return; how many holders there were,
/// those waited for among them.
fn release(at: usize, len: usize) -> usize {
    if PINS.load(Ordering::Relaxed) == 0 || len == 0 {
        return 0;
    }
    let end = at.saturating_add(len);
    let me = std::thread::current().id();
    let overlaps = |&(s, e, _): &(usize, usize, std::thread::ThreadId)| s < end && e > at;
    let (released, waited): (Vec<(u64, Release)>, usize) = {
        let mut g = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(Pins::default);
        // Releases running on other threads there: wait for them (a release
        // running on this thread is the caller's own).
        let mut waited = 0;
        while let Some(pins) = g.as_ref()
            && pins.releasing.values().any(|r| overlaps(r) && r.2 != me)
        {
            waited = 1;
            g = RELEASED.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        let pins = g.as_mut().expect("made above");
        let from = at.saturating_sub(pins.longest);
        let hit: Vec<(usize, u64)> = pins
            .ranges
            .range((from, 0)..(end, 0))
            .filter(|(_, (pin_end, _))| *pin_end > at)
            .map(|(key, _)| *key)
            .collect();
        let released = hit
            .into_iter()
            .filter_map(|key| {
                pins.by_id.remove(&key.1);
                let (pin_end, release) = pins.ranges.remove(&key)?;
                pins.releasing.insert(key.1, (key.0, pin_end, me));
                Some((key.1, release))
            })
            .collect();
        (released, waited)
    };
    let n = released.len();
    let ids: Vec<u64> = released.iter().map(|(id, _)| *id).collect();
    for (_, release) in released {
        release();
    }
    if n > 0 {
        let mut g = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pins) = g.as_mut() {
            for id in &ids {
                pins.releasing.remove(id);
            }
        }
        // Held until here: a change on another thread sees them.
        PINS.fetch_sub(n, Ordering::Relaxed);
        drop(g);
        RELEASED.notify_all();
        tracing::debug!(
            at = format_args!("{at:#x}"),
            len,
            n,
            "title memory let go before the runtime changes it"
        );
    }
    n + waited
}

/// Pins registered over part of `[at, at + len)` (tests elsewhere).
#[cfg(test)]
pub(crate) fn held_over(at: usize, len: usize) -> usize {
    let end = at.saturating_add(len);
    pins()
        .ranges
        .iter()
        .filter(|((start, _), (pin_end, _))| *start < end && *pin_end > at)
        .count()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::core::Interface;
    use windows_sys::Win32::System::Memory as mem;

    use super::*;

    /// Tests share the registry: each uses addresses of its own.
    fn counting(n: &Arc<AtomicUsize>) -> Release {
        let n = n.clone();
        Box::new(move || {
            n.fetch_add(1, Ordering::Relaxed);
        })
    }

    #[test]
    fn overlapping_pins_are_released_once_and_others_kept() {
        let base = 0x7000_0000_0000;
        let n = Arc::new(AtomicUsize::new(0));
        let _a = pin(base, 0x1_0000, counting(&n));
        let _b = pin(base + 0x8000, 0x10_0000, counting(&n));
        let after = pin(base + 0x20_0000, 0x1_0000, counting(&n));
        let before = pin(base - 0x1_0000, 0x1_0000, counting(&n));
        // Ends where `before` ends and starts where `after` starts: neither.
        assert_eq!(release(base + 0xF000, 0x1000), 2);
        assert_eq!(n.load(Ordering::Relaxed), 2);
        assert_eq!(release(base, 0x20_0000), 0, "released once");
        // A pin let go by its holder is not released again.
        after.forget();
        assert_eq!(release(base + 0x20_0000, 1), 0);
        assert_eq!(release(base - 1, 1), 1);
        assert_eq!(n.load(Ordering::Relaxed), 3);
        before.forget(); // already released: nothing to do
    }

    /// The long pin starts far before the range: still found.
    #[test]
    fn a_long_pin_starting_far_before_is_found() {
        let base = 0x7100_0000_0000;
        let n = Arc::new(AtomicUsize::new(0));
        let _long = pin(base, 1 << 30, counting(&n));
        let _short = pin(base + (1 << 29), 0x1000, counting(&n));
        assert_eq!(release(base + (1 << 30) - 1, 1), 1);
        assert_eq!(release(base + (1 << 29), 1), 1);
        assert_eq!(n.load(Ordering::Relaxed), 2);
    }

    /// A change refused as memory still locked is tried again only after a
    /// release, and only for that refusal.
    #[test]
    fn a_change_is_tried_again_only_after_a_release() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, SetLastError};
        let base = 0x7200_0000_0000;
        let calls = AtomicUsize::new(0);
        // Refused twice as locked, then done.
        let late = || {
            let k = calls.fetch_add(1, Ordering::Relaxed);
            // SAFETY: plain write of this thread's last error.
            unsafe { SetLastError(ERROR_INVALID_PARAMETER) };
            i32::from(k >= 2)
        };
        assert_eq!(
            title_memory_change(base, 0x1000, late),
            0,
            "nothing released"
        );
        assert_eq!(calls.swap(0, Ordering::Relaxed), 1);
        let n = Arc::new(AtomicUsize::new(0));
        let _p = pin(base, 0x1000, counting(&n));
        assert_eq!(title_memory_change(base, 0x1000, late), 1);
        assert_eq!(calls.swap(0, Ordering::Relaxed), 3);
        // Another refusal is the answer at once.
        let _p = pin(base, 0x1000, counting(&n));
        let denied = || {
            calls.fetch_add(1, Ordering::Relaxed);
            // SAFETY: plain write of this thread's last error.
            unsafe { SetLastError(ERROR_ACCESS_DENIED) };
            0
        };
        assert_eq!(title_memory_change(base, 0x1000, denied), 0);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(n.load(Ordering::Relaxed), 2);
    }

    /// A second thread changing the range while the first is still
    /// releasing waits for that release to return.
    #[test]
    fn a_change_waits_for_a_release_running_on_another_thread() {
        let base = 0x7300_0000_0000;
        let (started, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (s, d) = (started.clone(), done.clone());
        let _p = pin(
            base,
            0x1_0000,
            Box::new(move || {
                s.store(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(50));
                d.store(1, Ordering::SeqCst);
            }),
        );
        let first = std::thread::spawn(move || release(base, 0x1000));
        while started.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        assert!(title_memory_held(), "held until the release returns");
        // Overlapping, not the same range.
        let waited = release(base + 0x8000, 0x1_0000);
        assert_eq!(
            done.load(Ordering::SeqCst),
            1,
            "the change came after the release"
        );
        assert_eq!(waited, 1);
        assert_eq!(first.join().unwrap(), 1);
    }

    /// What the registry is for: memory a heap is opened over cannot be
    /// protected or freed until its holder lets it go, which
    /// [`title_memory_change`] makes it do.
    #[test]
    fn title_memory_held_by_a_heap_frees_after_the_call() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let Ok(device13) = host.device.cast::<ID3D12Device13>() else {
            return;
        };
        const BYTES: usize = 4 << 20;
        // SAFETY: plain allocation.
        let at = unsafe {
            mem::VirtualAlloc(
                std::ptr::null(),
                BYTES,
                mem::MEM_RESERVE | mem::MEM_COMMIT | 0x20_0000, // write-watched
                mem::PAGE_READWRITE,
            )
        };
        assert!(!at.is_null());
        // SAFETY: our memory, outliving the heap (released below).
        let Ok(heap) = (unsafe { device13.OpenExistingHeapFromAddress1::<ID3D12Heap>(at, BYTES) })
        else {
            // SAFETY: ours.
            unsafe { mem::VirtualFree(at, 0, mem::MEM_RELEASE) };
            return;
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: BYTES as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation in a live heap.
        unsafe {
            host.device
                .CreatePlacedResource(
                    &heap,
                    0,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .unwrap();
        }
        let held = Arc::new(Mutex::new(Some((heap, buffer))));
        let holder = held.clone();
        let _pin = pin(
            at as usize,
            BYTES,
            Box::new(move || {
                holder.lock().unwrap().take();
            }),
        );
        let mut old = 0;
        // SAFETY: our memory.
        let protected =
            unsafe { mem::VirtualProtect(at, 0x1_0000, mem::PAGE_READONLY, &mut old) } != 0;
        assert!(!protected, "a heap over the memory refuses protection");
        // The runtime is about to free a page in the middle.
        // SAFETY: an address inside our memory.
        let page = unsafe { at.cast::<u8>().add(BYTES / 2) } as usize;
        // SAFETY: our memory.
        let protect = || unsafe { mem::VirtualProtect(at, 0x1_0000, mem::PAGE_READONLY, &mut old) };
        assert_ne!(title_memory_change(page, 0x1000, protect), 0);
        assert!(held.lock().unwrap().is_none(), "the holder let go");
        // SAFETY: our memory, no heap on it any more.
        assert_ne!(unsafe { mem::VirtualFree(at, 0, mem::MEM_RELEASE) }, 0);
    }
}
