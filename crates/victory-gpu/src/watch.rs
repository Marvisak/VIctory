//! Which title memory changed since a host copy took it (decision 0007,
//! "Copies of title memory" and "Change tracking"). Each host copy of title
//! memory (`crate::shadow`) and each texture's staging (`crate::texture`)
//! tracks its memory ([`Tracked`]): the 64 KiB pages it has not taken since
//! they last changed. Taking pages makes their blocks read-only (guards);
//! the first write to a guarded block faults into our vectored handler,
//! which gives write access back and marks the block changed in every copy
//! holding it. Writers that do not fault say so: our own and the kernel's
//! writes into title memory ([`writing`], before they write), and the
//! runtime mapping or unmapping memory ([`title_memory_remapped`]). So
//! bringing a copy up to date costs what changed since, not how much memory
//! it covers: no system call for memory nobody wrote.
//!
//! Only the runtime's GPU memory is tracked: its write-watched pools, its
//! physical-page views and their placeholders. Other memory a copy takes
//! (the C heap, where the kernel writes system call results into buffers a
//! guard would make fail) is copied whole every time.
//!
//! Blocks that cannot be made read-only (placeholders, nothing mapped)
//! read as zeros and count as taken: only a mapping changes them, which the
//! runtime reports.
//!
//! Physical pages may be mapped at several addresses (the title fills
//! pages through a pool view and maps them into its heaps), so a write at
//! one is a change at all of them: the runtime says where else memory is
//! mapped whenever it maps it ([`title_memory_aliased`]); taking a page
//! guards every address showing it, and a write at any of them marks the
//! trackers at all. (Asking the runtime at each take instead waited for
//! its lock, which the title's page mappings hold: 4 s per world frame.)
//! DirectStorage writing pages through views of the runtime's own reports
//! the addresses showing them as written.
//!
//! shortcut: a system call writing into a guarded block fails; the known
//! ones (file reads, DirectStorage) open their blocks first. Memory freed
//! and allocated again while a tracker holds it is not seen.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The unit tracked and copied: 64 KiB, the console's page. Finer units
/// made the checks of the world's big buffers cost more than the copies
/// they saved.
pub(crate) const PAGE: usize = 64 << 10;

/// Hardware page size used to size stack buffers for `GetWriteWatch`.
const HW_PAGE: usize = 4096;

/// Consecutive write faults on a guarded `MEM_WRITE_WATCH` page before it is
/// promoted to hardware write-watch tracking (no `VirtualProtect` or VEH
/// faults while it stays hot).
const HOT_THRESHOLD: u8 = 3;

/// Consecutive takes with no CPU writes before a hot write-watched page cools
/// down and returns to a `PAGE_READONLY` guard (so cold trackers go back to
/// zero-syscall [`Tracked::clean`]).
const COOL_TAKES: u8 = 2;

/// The pages a tracker has not taken since they changed, what it saves of
/// pages it armed, its bit in [`DIRTY`] (none: always taken as dirty),
/// whether it is a host buffer copy's (the only trackers [`retake`] marks),
/// and how many of its pages are currently in [`Guards::watched`].
struct Changed(Mutex<BTreeSet<usize>>, Mutex<Saved>, Option<u32>, bool, AtomicU32);

/// A bit per tracker, set while it holds changed pages: set and cleared
/// under the tracker's lock, read without one ([`Tracked::clean`]). Dense,
/// so that a submission asking about every copy its heaps reach (~14,000)
/// reads a few KB instead of a cache line per copy.
static DIRTY: [AtomicU64; DIRTY_WORDS] = [const { AtomicU64::new(0) }; DIRTY_WORDS];
const DIRTY_WORDS: usize = 1 << 14;

/// Bits of [`DIRTY`] given back, and the first never handed out.
static FREE_BITS: Mutex<(Vec<u32>, u32)> = Mutex::new((Vec::new(), 0));

fn dirty_bit() -> Option<u32> {
    let mut free = FREE_BITS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(bit) = free.0.pop() {
        return Some(bit);
    }
    let bit = free.1;
    (bit < (DIRTY_WORDS * 64) as u32).then(|| {
        free.1 += 1;
        bit
    })
}

fn set_dirty(bit: Option<u32>, dirty: bool) {
    if let Some(bit) = bit {
        let (word, mask) = (&DIRTY[bit as usize / 64], 1u64 << (bit % 64));
        if dirty {
            word.fetch_or(mask, Ordering::Release);
        } else {
            word.fetch_and(!mask, Ordering::Release);
        }
    }
}

/// Whether the tracker with bit `bit` holds no changed page (see
/// [`Tracked::clean`]).
pub(crate) fn clean_bit(bit: Option<u32>) -> bool {
    bit.is_some_and(|bit| DIRTY[bit as usize / 64].load(Ordering::Acquire) & 1 << (bit % 64) == 0)
}

impl Drop for Changed {
    fn drop(&mut self) {
        if let Some(bit) = self.2 {
            set_dirty(Some(bit), false);
            FREE_BITS.lock().unwrap_or_else(|e| e.into_inner()).0.push(bit);
        }
    }
}

/// Pages (page numbers) whose bytes a tracker keeps when they are first
/// written after it took them, and those bytes: while shaders may be
/// writing its copy of them, what it held before (`crate::shadow`'s
/// copy-back of marked pages), with what the copy took of the page since
/// laid over them ([`Tracked::refreshed`]).
#[derive(Default)]
struct Saved {
    /// The armed pages as runs `[first, last)`, sorted, apart and not
    /// touching (runs armed that meet merge): a root view declares its
    /// copy from its address to the end, the world's 174 MB heap many
    /// times a submission, and a set entry per page cost 30 microseconds a
    /// declaration; descriptor tables arm many sub-ranges end to end, and
    /// the write handler checks a page under the guards' lock.
    armed: Vec<(usize, usize)>,
    /// Pages of those runs that are not armed any more.
    unarmed: BTreeSet<usize>,
    bytes: std::collections::HashMap<usize, Box<[u8]>>,
}

impl Saved {
    /// Whether page `page` is armed.
    fn armed(&self, page: usize) -> bool {
        // The first run ending after `page` is the only one that can hold it.
        let i = self.armed.partition_point(|&(_, last)| last <= page);
        self.armed.get(i).is_some_and(|&(first, _)| first <= page) && !self.unarmed.contains(&page)
    }

    /// Arms pages `[first, last)`.
    fn arm(&mut self, first: usize, last: usize) {
        if first >= last {
            return;
        }
        if !self.unarmed.is_empty() {
            let pages: Vec<usize> = self.unarmed.range(first..last).copied().collect();
            for page in pages {
                self.unarmed.remove(&page);
            }
        }
        // Runs `[lo, hi)` overlap or touch the new one: it replaces them.
        let lo = self.armed.partition_point(|&(_, b)| b < first);
        let hi = self.armed.partition_point(|&(a, _)| a <= last);
        if lo + 1 == hi && self.armed[lo].0 <= first && last <= self.armed[lo].1 {
            return;
        }
        let run = if lo < hi {
            (self.armed[lo].0.min(first), self.armed[hi - 1].1.max(last))
        } else {
            (first, last)
        };
        self.armed.splice(lo..hi, [run]);
    }
}

impl Changed {
    /// Page `page` was taken: what it saved of it is stale.
    fn taken(&self, page: usize) {
        self.1.lock().unwrap_or_else(|e| e.into_inner()).bytes.remove(&page);
    }

    /// Whether `page` is currently armed for pre-write saving.
    fn is_armed(&self, page: usize) -> bool {
        self.1.lock().unwrap_or_else(|e| e.into_inner()).armed(page)
    }

    /// Updates this tracker's [`DIRTY`] bit: stays set while either changed
    /// pages remain or any of its pages are polled via hardware write-watch.
    fn update_dirty(&self, changed_empty: bool) {
        set_dirty(self.2, !changed_empty || self.4.load(Ordering::Acquire) > 0);
    }

    /// Page `page` is about to change for the first time since it was
    /// taken: its bytes now, if armed for it.
    fn save(&self, page: usize) {
        let mut saved = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if !saved.armed(page) || saved.bytes.contains_key(&page) {
            return;
        }
        let mut bytes = vec![0u8; PAGE].into_boxed_slice();
        if crate::host::read_into((page * PAGE) as *const u8, &mut bytes) {
            saved.bytes.insert(page, bytes);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::SavedPages, 1);
        }
    }
}

/// Byte runs `[start, end)` in order.
pub(crate) type Runs = Vec<(usize, usize)>;

/// Title memory a host copy or texture staging takes: bytes `[start, end)`.
pub(crate) struct Tracked {
    start: usize,
    end: usize,
    changed: Arc<Changed>,
    /// Parts of the range on pages that are not the runtime's memory (in
    /// order): not tracked, copied whole at each use ([`Tracked::untracked`]).
    untracked: Vec<(usize, usize)>,
}

struct Guards {
    /// Blocks (pages) we made read-only.
    protected: BTreeSet<usize>,
    /// Hot `MEM_WRITE_WATCH` pages left read-write and polled through
    /// `GetWriteWatch` instead of faulting into [`install`]'s handler.
    watched: BTreeSet<usize>,
    /// Consecutive write faults per guarded page toward [`HOT_THRESHOLD`].
    hot_streak: BTreeMap<usize, u8>,
    /// Consecutive clean `GetWriteWatch` polls per watched page toward
    /// [`COOL_TAKES`].
    quiet_takes: BTreeMap<usize, u8>,
    /// The trackers holding each page.
    holders: BTreeMap<usize, Vec<Arc<Changed>>>,
    /// Pages `[first, last)` of writes the guards do not see that are under
    /// way ([`Writing`]): not guarded until they end, or the write fails.
    busy: Vec<(usize, usize)>,
    /// Pages showing the same physical pages elsewhere, both ways, as the
    /// runtime told when it mapped them: a write at one marks the others,
    /// with no question to the runtime (whose lock its page mappings hold
    /// for long).
    alias_of: BTreeMap<usize, Vec<usize>>,
    /// Tracked pages written this frame and the frame before (diagnostics).
    #[cfg(feature = "diagnostics")]
    written: BTreeSet<usize>,
    #[cfg(feature = "diagnostics")]
    written_before: BTreeSet<usize>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    watched: BTreeSet::new(),
    hot_streak: BTreeMap::new(),
    quiet_takes: BTreeMap::new(),
    holders: BTreeMap::new(),
    busy: Vec::new(),
    alias_of: BTreeMap::new(),
    #[cfg(feature = "diagnostics")]
    written: BTreeSet::new(),
    #[cfg(feature = "diagnostics")]
    written_before: BTreeSet::new(),
});

#[cfg(not(feature = "diagnostics"))]
fn guards() -> std::sync::MutexGuard<'static, Guards> {
    GUARDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The guard lock, timed while held by the line that took it
/// (diagnostics: [`held_times`]).
#[cfg(feature = "diagnostics")]
#[track_caller]
fn guards() -> Held {
    let line = std::panic::Location::caller().line();
    let guard = GUARDS.lock().unwrap_or_else(|e| e.into_inner());
    Held(Some(guard), std::time::Instant::now(), line)
}

#[cfg(feature = "diagnostics")]
struct Held(Option<std::sync::MutexGuard<'static, Guards>>, std::time::Instant, u32);

#[cfg(feature = "diagnostics")]
impl std::ops::Deref for Held {
    type Target = Guards;
    fn deref(&self) -> &Guards {
        self.0.as_ref().expect("held until dropped")
    }
}

#[cfg(feature = "diagnostics")]
impl std::ops::DerefMut for Held {
    fn deref_mut(&mut self) -> &mut Guards {
        self.0.as_mut().expect("held until dropped")
    }
}

/// Per line taking the guard lock: the line, microseconds held in all, the
/// longest hold (no allocation: the fault handler takes the lock).
#[cfg(feature = "diagnostics")]
static HELD: [[std::sync::atomic::AtomicU64; 3]; 16] =
    [const { [const { std::sync::atomic::AtomicU64::new(0) }; 3] }; 16];

#[cfg(feature = "diagnostics")]
impl Drop for Held {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let us = self.1.elapsed().as_micros() as u64;
        drop(self.0.take());
        let line = u64::from(self.2);
        if let Some(slot) = HELD
            .iter()
            .find(|s| s[0].load(Relaxed) == line || s[0].compare_exchange(0, line, Relaxed, Relaxed).is_ok())
        {
            slot[1].fetch_add(us, Relaxed);
            slot[2].fetch_max(us, Relaxed);
        }
    }
}

/// Since the last call, per line of this file taking the guard lock: (line,
/// milliseconds held, longest hold in milliseconds), most held first.
#[cfg(feature = "diagnostics")]
pub(crate) fn held_times() -> Vec<(u64, f64, f64)> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut out: Vec<(u64, f64, f64)> = HELD
        .iter()
        .filter(|s| s[0].load(Relaxed) != 0)
        .map(|s| (s[0].load(Relaxed), s[1].swap(0, Relaxed) as f64 / 1e3, s[2].swap(0, Relaxed) as f64 / 1e3))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// (page, other page) for the pages (page numbers) showing the same memory
/// as `pages` elsewhere.
fn aliases(g: &Guards, pages: &[usize]) -> Vec<(usize, usize)> {
    pages
        .iter()
        .flat_map(|&p| g.alias_of.get(&p).into_iter().flatten().map(move |&a| (p, a)))
        .collect()
}

/// Guards the other mappings of taken pages, returning those that still
/// have an unguarded mapping (cannot be taken yet).
fn guard_aliases(g: &mut Guards, pairs: &[(usize, usize)]) -> BTreeSet<usize> {
    if pairs.is_empty() {
        return BTreeSet::new();
    }
    let others: Vec<usize> = pairs.iter().map(|&(_, a)| a).collect();
    let guarded: BTreeSet<usize> = protect(g, &others).into_iter().collect();
    pairs
        .iter()
        .filter(|(_, a)| !guarded.contains(a))
        .map(|&(p, _)| p)
        .collect()
}

/// Tracks `[start, start + len)` if it is the runtime's GPU memory (every
/// page changed at first); `None` for other memory, and with `views` false
/// for anything but its write-watched pools (physical-page views are
/// written through other views showing the same pages: the title fills
/// streamed textures and video planes through a pool view and maps the
/// pages into its tiled heap, where a guard sees none of it).
///
/// Only the pages of the range that are such memory are tracked; the rest is
/// never guarded and is copied whole at each use, as memory with no tracker.
pub(crate) fn track(start: usize, len: usize, views: bool) -> Option<Tracked> {
    track_as(start, len, views, false)
}

/// [`track`] for a host buffer copy: what shaders write lands in it, so a
/// [`retake`] of its memory marks it.
pub(crate) fn track_copy(start: usize, len: usize, views: bool) -> Option<Tracked> {
    track_as(start, len, views, true)
}

fn track_as(start: usize, len: usize, views: bool, copy: bool) -> Option<Tracked> {
    if len == 0 {
        return None;
    }
    let end = start + len;
    let pages = runtime_pages(start, end, views);
    if pages.is_empty() {
        return None;
    }
    install();
    // The rest of the range, as byte runs.
    let mut untracked: Vec<(usize, usize)> = Vec::new();
    let mut at = start;
    for &page in pages.iter().chain([&usize::MAX]) {
        let (a, b) = ((page.saturating_mul(PAGE)).max(start).min(end), (page.saturating_add(1).saturating_mul(PAGE)).min(end));
        if at < a {
            untracked.push((at, a));
        }
        at = at.max(b);
    }
    // A tracker with untracked parts is never clean: no bit.
    let bit = if untracked.is_empty() { dirty_bit() } else { None };
    set_dirty(bit, true);
    let changed = Arc::new(Changed(
        Mutex::new(pages.iter().copied().collect()),
        Mutex::default(),
        bit,
        copy,
        AtomicU32::new(0),
    ));
    let mut g = guards();
    for &page in &pages {
        if g.watched.contains(&page) {
            if copy {
                changed.4.fetch_add(1, Ordering::AcqRel);
            } else {
                // Texture stagings and descriptor heaps rely on guards rather
                // than per-submission write-watch polling.
                demote_watched(&mut g, page, true);
            }
        }
        g.holders.entry(page).or_default().push(changed.clone());
    }
    drop(g);
    Some(Tracked {
        start,
        end,
        changed,
        untracked,
    })
}

/// The pages of `[start, end)` that are the runtime's GPU memory: a
/// write-watched pool, or (with `views`) anything but plain committed memory
/// and images (views, placeholders, nothing mapped yet). Decided for every
/// region the range covers, and a page touching any other memory is left
/// out: a range can run past its allocation (a 4 MiB window of memory
/// outside buffers, `crate::shadow`), and when the first page alone decided,
/// a window starting in free memory had the process heap's pages after it
/// guarded; the heap's own code then faulted while holding its lock, and the
/// guard handler waited for the guard lock, whose holder waited for the heap
/// lock (the crate's tests hung).
///
/// shortcut: memory free or reserved when the range is tracked counts as the
/// runtime's; another allocator committing it before the first take would
/// have it guarded.
fn runtime_pages(start: usize, end: usize, views: bool) -> Vec<usize> {
    use windows_sys::Win32::System::Memory::{
        GetWriteWatch, MEM_COMMIT, MEM_IMAGE, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    let watched = |at: usize, len: usize| {
        let (mut found, mut count, mut granularity) = (0usize, 1usize, 0u32);
        // SAFETY: room for one answer; no reset, so nothing changes.
        unsafe { GetWriteWatch(0, at as *const _, len, (&raw mut found).cast(), &mut count, &mut granularity) == 0 }
    };
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    // One pool holds it all (the common case; asking `VirtualQuery` costs
    // milliseconds in the runtime's big pools).
    if watched(start & !(4096 - 1), end - (start & !(4096 - 1))) {
        return (first..last).collect();
    }
    let (mut runtime, mut other) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
    let mut at = start & !(4096 - 1);
    while at < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let next = (info.BaseAddress as usize + info.RegionSize).max(at + 1).min(end);
        let ours = watched(at, next - at)
            || views && info.Type != MEM_IMAGE && !(info.State == MEM_COMMIT && info.Type == MEM_PRIVATE);
        (if ours { &mut runtime } else { &mut other }).extend(at / PAGE..next.div_ceil(PAGE));
        at = next;
    }
    runtime.difference(&other).copied().collect()
}

impl Drop for Tracked {
    /// Pages no tracker holds any more get write access back, so that the
    /// guard set never names memory freed and allocated again; unless another
    /// mapping of the page is held: the guard here is that tracker's too (it
    /// was set when that tracker took its page), and with it opened a write
    /// here went unseen there.
    fn drop(&mut self) {
        let mut g = guards();
        let mut free = Vec::new();
        for page in self.start / PAGE..self.end.div_ceil(PAGE) {
            let watched = g.watched.contains(&page);
            if let Some(holders) = g.holders.get_mut(&page) {
                let before = holders.len();
                holders.retain(|c| !Arc::ptr_eq(c, &self.changed));
                if watched && holders.len() < before {
                    self.changed.4.fetch_sub(1, Ordering::AcqRel);
                }
                if holders.is_empty() {
                    g.holders.remove(&page);
                    g.hot_streak.remove(&page);
                    g.quiet_takes.remove(&page);
                    g.watched.remove(&page);
                    free.push(page);
                }
            }
        }
        for page in free {
            let watched_elsewhere = g.alias_of.get(&page).is_some_and(|o| o.iter().any(|p| g.holders.contains_key(p)));
            if !watched_elsewhere {
                open(&mut g, page);
            }
        }
    }
}

/// Gives write access back to page `page` if we guarded it.
fn open(g: &mut Guards, page: usize) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    if !g.protected.remove(&page) {
        return false;
    }
    let mut old = 0;
    // SAFETY: a block we made read-only (failing harmlessly if it is gone).
    unsafe { VirtualProtect((page * PAGE) as *const _, PAGE, PAGE_READWRITE, &mut old) };
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::OpenPages, 1);
    true
}

/// Resets Windows hardware write-watch bits for `page`. Returns `true` only if
/// `page` is a valid `MEM_WRITE_WATCH` allocation.
fn reset_write_watch(page: usize) -> bool {
    use windows_sys::Win32::System::Memory::ResetWriteWatch;
    // SAFETY: queries and clears write-watch bits without dereferencing.
    unsafe { ResetWriteWatch((page * PAGE) as *const _, PAGE) == 0 }
}

/// Non-resetting check whether any 4 KiB subpage of `page` has its Windows
/// write-watch dirty bit set. Conservatively returns `true` if `GetWriteWatch`
/// fails (e.g. remapped/unmapped).
fn peek_write_watch(page: usize) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    let mut found = [0usize; 1];
    let mut count = found.len();
    let mut granularity = 0u32;
    // SAFETY: stack buffer of `count` entries; flags = 0 performs no reset.
    let rc = unsafe {
        GetWriteWatch(
            0,
            (page * PAGE) as *const _,
            PAGE,
            found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    };
    rc != 0 || count > 0
}

/// Maximum contiguous 64 KiB pages polled in a single `GetWriteWatch` call
/// (64 × 16 = 1,024 hardware 4 KiB pages = 8 KiB stack buffer, covering 4 MiB
/// per syscall).
const WATCH_BATCH_PAGES: usize = 64;

/// Queries and atomically resets Windows hardware write-watch bits across a
/// contiguous run of `count_pages` (`1..=WATCH_BATCH_PAGES`) 64 KiB pages
/// starting at `first_page`. Returns a `u64` bitmask of which pages in the run
/// were written (`bit k` = `first_page + k`), or `Err(())` if any page in the
/// run is no longer a valid `MEM_WRITE_WATCH` region.
fn poll_write_watch_run(first_page: usize, count_pages: usize) -> Result<u64, ()> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    debug_assert!((1..=WATCH_BATCH_PAGES).contains(&count_pages));
    let mut found = [0usize; WATCH_BATCH_PAGES * (PAGE / HW_PAGE)];
    let mut count = found.len();
    let mut granularity = 0u32;
    // SAFETY: stack buffer holds every 4 KiB subpage in `count_pages * PAGE`,
    // guaranteeing `WRITE_WATCH_FLAG_RESET` resets the entire run in one call.
    let rc = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            (first_page * PAGE) as *const _,
            count_pages * PAGE,
            found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    };
    if rc != 0 {
        return Err(());
    }
    let mut mask = 0u64;
    for &addr in &found[..count] {
        let idx = (addr / PAGE).wrapping_sub(first_page);
        if idx < count_pages {
            mask |= 1u64 << idx;
        }
    }
    Ok(mask)
}

/// Queries and atomically resets Windows hardware write-watch bits across all
/// 4 KiB subpages of a single `page`. Returns `Ok(dirty)` or `Err(())` if the
/// page is no longer a valid `MEM_WRITE_WATCH` region.
fn poll_write_watch(page: usize) -> Result<bool, ()> {
    poll_write_watch_run(page, 1).map(|mask| mask != 0)
}

/// Whether `page` is eligible to be tracked through hardware `MEM_WRITE_WATCH`
/// instead of a `PAGE_READONLY` guard: reached [`HOT_THRESHOLD`] faults, has
/// no physical aliases, and every holder is an unarmed host buffer copy.
fn can_watch(g: &Guards, page: usize) -> bool {
    if g.hot_streak.get(&page).copied().unwrap_or(0) < HOT_THRESHOLD {
        return false;
    }
    if g.alias_of.contains_key(&page) {
        return false;
    }
    let Some(holders) = g.holders.get(&page) else {
        return false;
    };
    !holders.is_empty() && holders.iter().all(|c| c.3 && !c.is_armed(page))
}

/// Moves `page` into [`Guards::watched`] after resetting its write-watch bits.
/// Returns `false` if `page` is not a `MEM_WRITE_WATCH` allocation (e.g. a
/// section view), clearing its hot streak so it falls back to normal guards.
fn promote_watched(g: &mut Guards, page: usize) -> bool {
    if !reset_write_watch(page) {
        g.hot_streak.remove(&page);
        return false;
    }
    if g.watched.insert(page)
        && let Some(holders) = g.holders.get(&page)
    {
        for c in holders {
            c.4.fetch_add(1, Ordering::AcqRel);
            set_dirty(c.2, true);
        }
    }
    g.quiet_takes.remove(&page);
    true
}

/// Removes `page` from [`Guards::watched`] and updates every holder's watched
/// count and dirty bit. Does not poll or re-guard `page`.
fn remove_watched(g: &mut Guards, page: usize, skip_dirty_update: Option<&Arc<Changed>>) {
    if !g.watched.remove(&page) {
        return;
    }
    g.quiet_takes.remove(&page);
    g.hot_streak.remove(&page);
    if let Some(holders) = g.holders.get(&page) {
        for c in holders {
            c.4.fetch_sub(1, Ordering::AcqRel);
            if skip_dirty_update.is_none_or(|skip| !Arc::ptr_eq(c, skip)) {
                let empty = c.0.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
                c.update_dirty(empty);
            }
        }
    }
}

/// Demotes `page` out of [`Guards::watched`]. When `re_guard` is true, installs
/// the `PAGE_READONLY` guard *before* the final `GetWriteWatch` poll so no CPU
/// write can slip through the window between polling and protecting: any write
/// before `VirtualProtect` is caught by the final poll, and any write after
/// faults into `on_write` (which waits on `GUARDS`).
fn demote_watched(g: &mut Guards, page: usize, re_guard: bool) {
    if !g.watched.contains(&page) {
        return;
    }
    remove_watched(g, page, None);
    if re_guard {
        let _ = protect(g, &[page]);
    }
    let dirty = poll_write_watch(page).unwrap_or(true);
    if dirty && let Some(holders) = g.holders.get(&page) {
        for c in holders {
            let mut set = c.0.lock().unwrap_or_else(|e| e.into_inner());
            set.insert(page);
            set_dirty(c.2, true);
        }
    }
}

impl Tracked {
    /// Takes the changed pages of `[from, to)`: guards them (writes from
    /// now on mark them again) and returns the byte ranges to copy, cut to
    /// `[from, to)`, in order. A page whose part in the tracked memory lies
    /// only partly in `[from, to)` is copied in part and stays changed.
    pub(crate) fn take(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let (copy, zero) = self.take_split(from, to);
        let mut runs: Vec<(usize, usize)> = copy.into_iter().chain(zero).collect();
        runs.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(runs.len());
        for (a, b) in runs {
            match merged.last_mut() {
                Some((_, end)) if *end == a => *end = b,
                _ => merged.push((a, b)),
            }
        }
        merged
    }

    /// [`Tracked::take`], the runs on pages with nothing mapped apart: they
    /// read as zeros, with no copy to try (each such page cost a dozen
    /// failing kernel reads, 7-30 ms a frame in run perf4).
    pub(crate) fn take_split(&self, from: usize, to: usize) -> (Runs, Runs) {
        let (from, to) = (from.max(self.start), to.min(self.end));
        if from >= to {
            return (Vec::new(), Vec::new());
        }
        let first_page = from / PAGE;
        let last_page = to.div_ceil(PAGE);
        let mut pages: Vec<usize> = {
            #[cfg(feature = "diagnostics")]
            let waited = Timed(std::time::Instant::now(), crate::copystat::C::GuardWaitUs);
            let changed = self.changed.0.lock().unwrap_or_else(|e| e.into_inner());
            #[cfg(feature = "diagnostics")]
            drop(waited);
            changed.range(first_page..last_page).copied().collect()
        };
        let has_watched = self.changed.4.load(Ordering::Acquire) > 0;
        if pages.is_empty() && !has_watched {
            return (Vec::new(), Vec::new());
        }
        let is_whole = |p: usize| (p * PAGE).max(self.start) >= from && ((p + 1) * PAGE).min(self.end) <= to;
        // Every address showing the same pages is guarded too. A write in
        // between marks them changed again; the copy, made after, has it.
        #[cfg(feature = "diagnostics")]
        let waited = Timed(std::time::Instant::now(), crate::copystat::C::GuardWaitUs);
        let mut g = guards();
        let mut changed = self.changed.0.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "diagnostics")]
        drop(waited);
        let mut whole: Vec<usize> = Vec::with_capacity(pages.len());
        let mut cooled: Vec<usize> = Vec::new();
        // Poll any hot `MEM_WRITE_WATCH` pages in `[first_page, last_page)`
        // before guarding normal pages: dirty ones are returned directly and
        // reset atomically without `VirtualProtect` or a VEH fault; pages that
        // stay quiet cool back down into `PAGE_READONLY` guards.
        if has_watched {
            let watched_in_range: Vec<usize> = g
                .watched
                .range(first_page..last_page)
                .copied()
                .filter(|p| {
                    g.holders
                        .get(p)
                        .is_some_and(|h| h.iter().any(|c| Arc::ptr_eq(c, &self.changed)))
                })
                .collect();
            let mut i = 0;
            while i < watched_in_range.len() {
                let run_first = watched_in_range[i];
                let mut run_end = i;
                while run_end + 1 < watched_in_range.len()
                    && watched_in_range[run_end + 1] == watched_in_range[run_end] + 1
                    && (run_end + 1 - i) < WATCH_BATCH_PAGES
                {
                    run_end += 1;
                }
                let run = &watched_in_range[i..=run_end];
                i = run_end + 1;
                let batch_mask = poll_write_watch_run(run_first, run.len());
                for (offset, &p) in run.iter().enumerate() {
                    let whole_page = is_whole(p);
                    let polled = match batch_mask {
                        Ok(mask) => Ok((mask >> offset) & 1 != 0),
                        Err(()) => poll_write_watch(p),
                    };
                    match polled {
                        Ok(true) => {
                            g.quiet_takes.remove(&p);
                            if let Some(holders) = g.holders.get(&p) {
                                for c in holders {
                                    if !Arc::ptr_eq(c, &self.changed) {
                                        let mut set = c.0.lock().unwrap_or_else(|e| e.into_inner());
                                        set.insert(p);
                                        set_dirty(c.2, true);
                                    }
                                }
                            }
                            pages.push(p);
                            if whole_page {
                                changed.remove(&p);
                                self.changed.taken(p);
                            } else {
                                changed.insert(p);
                            }
                        }
                        Ok(false) => {
                            if changed.contains(&p) {
                                if whole_page {
                                    changed.remove(&p);
                                    self.changed.taken(p);
                                }
                            } else {
                                let quiet = g.quiet_takes.entry(p).or_insert(0);
                                *quiet = quiet.saturating_add(1);
                                if *quiet >= COOL_TAKES {
                                    // The poll covered the whole page, even for a clipped take.
                                    whole.push(p);
                                    cooled.push(p);
                                }
                            }
                        }
                        Err(()) => {
                            remove_watched(&mut g, p, Some(&self.changed));
                            pages.push(p);
                            if whole_page {
                                whole.push(p);
                            }
                        }
                    }
                }
            }
            pages.sort_unstable();
            pages.dedup();
        }
        for &p in &pages {
            if is_whole(p) && !g.watched.contains(&p) {
                whole.push(p);
            }
        }
        whole.sort_unstable();
        whole.dedup();
        let mut unmapped = Vec::new();
        if !whole.is_empty() {
            #[cfg(feature = "diagnostics")]
            let asked = Timed(std::time::Instant::now(), crate::copystat::C::AliasUs);
            let others = aliases(&g, &whole);
            let unguarded = guard_aliases(&mut g, &others);
            #[cfg(feature = "diagnostics")]
            drop(asked);
            whole.retain(|p| !unguarded.contains(p));
            for page in protect_noting(&mut g, &whole, &cooled, &mut unmapped) {
                changed.remove(&page);
                self.changed.taken(page);
            }
        }
        // Final write-watch poll for pages cooled to `PAGE_READONLY` above:
        // catches any CPU write that landed in the window between the earlier
        // `poll_write_watch_run` and `VirtualProtect(PAGE_READONLY)`.
        if !cooled.is_empty() {
            let mut i = 0;
            while i < cooled.len() {
                let run_first = cooled[i];
                let mut run_end = i;
                while run_end + 1 < cooled.len()
                    && cooled[run_end + 1] == cooled[run_end] + 1
                    && (run_end + 1 - i) < WATCH_BATCH_PAGES
                {
                    run_end += 1;
                }
                let run = &cooled[i..=run_end];
                i = run_end + 1;
                let batch_mask = poll_write_watch_run(run_first, run.len());
                for (offset, &p) in run.iter().enumerate() {
                    let dirty = match batch_mask {
                        Ok(mask) => (mask >> offset) & 1 != 0,
                        Err(()) => poll_write_watch(p).unwrap_or(true),
                    };
                    if dirty {
                        // A clipped copy cannot acknowledge the rest of a late write.
                        if !is_whole(p) {
                            changed.insert(p);
                        }
                        if let Some(holders) = g.holders.get(&p) {
                            for c in holders {
                                if !Arc::ptr_eq(c, &self.changed) {
                                    let mut set = c.0.lock().unwrap_or_else(|e| e.into_inner());
                                    set.insert(p);
                                    set_dirty(c.2, true);
                                }
                            }
                        }
                        pages.push(p);
                    }
                    // A busy or otherwise unguardable page still needs hardware polling.
                    // Publish the handoff only after delivering writes from the final poll.
                    if g.protected.contains(&p) || unmapped.binary_search(&p).is_ok() {
                        remove_watched(&mut g, p, Some(&self.changed));
                    }
                }
            }
            pages.sort_unstable();
            pages.dedup();
        }
        self.changed.update_dirty(changed.is_empty());
        drop(changed);
        drop(g);
        let (mut copy, mut zero): (Runs, Runs) = (Vec::new(), Vec::new());
        for p in pages {
            let (a, b) = ((p * PAGE).max(from), ((p + 1) * PAGE).min(to));
            let runs = if unmapped.contains(&p) { &mut zero } else { &mut copy };
            match runs.last_mut() {
                Some((_, end)) if *end == a => *end = b,
                _ => runs.push((a, b)),
            }
        }
        (copy, zero)
    }

    /// Whether page `page` (a page number) changed since it was taken.
    pub(crate) fn changed(&self, page: usize) -> bool {
        if self
            .changed
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&page)
        {
            return true;
        }
        if self.changed.4.load(Ordering::Acquire) > 0 {
            let g = guards();
            if g.watched.contains(&page) {
                return peek_write_watch(page);
            }
        }
        false
    }

    /// The caller made pages (page numbers) of its copy equal to title
    /// memory itself (what shaders wrote went back): taken and guarded.
    pub(crate) fn taken(&self, pages: &[usize]) {
        let mut g = guards();
        let mut changed = self.changed.0.lock().unwrap_or_else(|e| e.into_inner());
        for &page in pages {
            // A page written back by the GPU is not CPU-only hot: demote it
            // back to a guard so future writes are caught by the handler.
            remove_watched(&mut g, page, Some(&self.changed));
            g.hot_streak.remove(&page);
        }
        let others = aliases(&g, pages);
        let unguarded = guard_aliases(&mut g, &others);
        let pages: Vec<usize> = pages
            .iter()
            .copied()
            .filter(|p| !unguarded.contains(p))
            .collect();
        for page in protect(&mut g, &pages) {
            changed.remove(&page);
            self.changed.taken(page);
        }
        self.changed.update_dirty(changed.is_empty());
    }

    /// Whether no page changed since it was taken: then [`Tracked::take`]
    /// finds nothing, and asking costs no lock. A write racing this answer
    /// is found by the next asking, as one racing a take.
    pub(crate) fn clean(&self) -> bool {
        clean_bit(self.changed.2)
    }

    /// The tracker's bit for [`clean_bit`], for callers that ask about
    /// many trackers at once without touching each (none: never clean).
    pub(crate) fn bit(&self) -> Option<u32> {
        self.changed.2
    }

    /// The parts of `[from, to)` that are not tracked (see
    /// [`Tracked::untracked`] field): to be copied whole at each use.
    pub(crate) fn untracked(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        self.untracked
            .iter()
            .map(|&(a, b)| (a.max(from), b.min(to)))
            .filter(|&(a, b)| a < b)
            .collect()
    }

    /// Shaders may write the copy's bytes `[from, to)` until [`Tracked::disarm`]:
    /// pages there written next after they were taken have their bytes kept
    /// first ([`Tracked::saved`]).
    pub(crate) fn arm(&self, from: usize, to: usize) {
        let (from, to) = (from.max(self.start), to.min(self.end));
        if from >= to {
            return;
        }
        let (first, last) = (from / PAGE, to.div_ceil(PAGE));
        // Always serialize with `protect_noting` / `can_watch` under `GUARDS`
        // (preserving global `GUARDS` -> `Saved` lock order) so a concurrent
        // first-time promotion cannot leave an armed page read-write without
        // demoting it.
        let mut g = guards();
        self.changed.1.lock().unwrap_or_else(|e| e.into_inner()).arm(first, last);
        if !g.watched.is_empty() {
            let to_demote: Vec<usize> = g.watched.range(first..last).copied().collect();
            for page in to_demote {
                demote_watched(&mut g, page, true);
            }
        }
    }

    /// Page `page` (a page number) needs no saving any more: its copy-back
    /// is under way.
    pub(crate) fn unarm(&self, page: usize) {
        let mut saved = self.changed.1.lock().unwrap_or_else(|e| e.into_inner());
        if saved.armed(page) {
            saved.unarmed.insert(page);
        }
    }

    /// No shader writes the copy any more: nothing is armed or kept.
    pub(crate) fn disarm(&self) {
        let mut saved = self.changed.1.lock().unwrap_or_else(|e| e.into_inner());
        saved.armed.clear();
        saved.unarmed.clear();
        saved.bytes.clear();
    }

    /// The bytes page `page` (a page number) held when it was first written
    /// since it was taken, if it was armed then (and so changed), with what
    /// the copy took of it since: the copy's bytes before the submission.
    pub(crate) fn saved(&self, page: usize) -> Option<Box<[u8]>> {
        self.changed.1.lock().unwrap_or_else(|e| e.into_inner()).bytes.remove(&page)
    }

    /// The copy took `bytes` at title address `from`: any saved bytes of pages
    /// it covers take them too, so marked copy-back neither takes a partial
    /// refresh for the GPU's nor drops a shader write back to the pre-fault byte.
    pub(crate) fn refreshed(&self, from: usize, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut saved = self.changed.1.lock().unwrap_or_else(|e| e.into_inner());
        if saved.bytes.is_empty() {
            return;
        }
        let to = from + bytes.len();
        for page in from / PAGE..to.div_ceil(PAGE) {
            if let Some(before) = saved.bytes.get_mut(&page) {
                let (a, b) = (from.max(page * PAGE), to.min((page + 1) * PAGE));
                before[a - page * PAGE..b - page * PAGE]
                    .copy_from_slice(&bytes[a - from..b - from]);
            }
        }
    }
}

/// Makes `pages` read-only where not already (in runs), and returns those
/// now guarded or with nothing mapped (taken either way); a page that is
/// mapped but cannot be guarded is left out (it stays changed).
fn protect(g: &mut Guards, pages: &[usize]) -> Vec<usize> {
    protect_noting(g, pages, &[], &mut Vec::new())
}

/// [`protect`], also listing in `unmapped` the pages taken because nothing
/// is mapped there. Sorted hot pages in `cooling` need a real read-only guard
/// while their write-watch stays active through the final poll.
fn protect_noting(g: &mut Guards, pages: &[usize], cooling: &[usize], unmapped_pages: &mut Vec<usize>) -> Vec<usize> {
    #[cfg(feature = "diagnostics")]
    let counted = |calls: u64, pages: u64| {
        crate::copystat::add(crate::copystat::C::ProtectCalls, calls);
        crate::copystat::add(crate::copystat::C::ProtectPages, pages);
    };
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READONLY, VirtualProtect, VirtualQuery,
    };
    #[cfg(feature = "diagnostics")]
    let _timed = Timed(std::time::Instant::now(), crate::copystat::C::ProtectUs);
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title memory; only its protection changes.
        unsafe { VirtualProtect((from * PAGE) as *const _, (to - from) * PAGE, PAGE_READONLY, &mut old) != 0 }
    };
    let unmapped = |page: usize| {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery((page * PAGE) as *const _, &mut info, size_of_val(&info)) };
        n == 0 || info.State != MEM_COMMIT
    };
    let mut taken = Vec::with_capacity(pages.len());
    let mut to_guard = Vec::with_capacity(pages.len());
    for &p in pages {
        if g.busy.iter().any(|&(a, b)| (a..b).contains(&p)) {
            continue;
        }
        if g.protected.contains(&p) {
            taken.push(p);
            continue;
        }
        if g.watched.contains(&p) {
            if cooling.binary_search(&p).is_ok() {
                to_guard.push(p);
            } else {
                taken.push(p);
            }
            continue;
        }
        if can_watch(g, p) && promote_watched(g, p) {
            taken.push(p);
            continue;
        }
        to_guard.push(p);
    }
    let pages = to_guard;
    let mut i = 0;
    while i < pages.len() {
        // A run of consecutive pages not guarded yet.
        let first = i;
        while i + 1 < pages.len() && pages[i + 1] == pages[i] + 1 {
            i += 1;
        }
        let run = &pages[first..=i];
        i += 1;
        #[cfg(feature = "diagnostics")]
        counted(1, run.len() as u64);
        if set(run[0], run[run.len() - 1] + 1) {
            g.protected.extend(run);
            taken.extend(run);
            continue;
        }
        for &page in run {
            if set(page, page + 1) {
                g.protected.insert(page);
                taken.push(page);
            } else if unmapped(page) {
                taken.push(page);
                unmapped_pages.push(page);
            }
        }
    }
    taken
}

/// Pages `[first, last)` changed, and so the pages showing the same memory
/// elsewhere: gives write access back where we guarded them and marks them
/// in every tracker holding them. True if one of `[first, last)` was
/// guarded.
fn changed(g: &mut Guards, first: usize, last: usize, save: bool) -> bool {
    let guarded = g.protected.range(first..last).next().is_some();
    let others: Vec<usize> = g.alias_of.range(first..last).flat_map(|(_, o)| o.iter().copied()).collect();
    #[cfg(feature = "diagnostics")]
    {
        let held = others.iter().filter(|p| g.holders.contains_key(p)).count();
        crate::copystat::add(crate::copystat::C::AliasMarks, held as u64);
    }
    // Every page's bytes are saved before any is opened: once a page is
    // open, a write by another thread lands without a fault, and a save
    // after it would keep that write as the page's old bytes.
    mark_changed(g, first, last, save);
    for &page in &others {
        mark_changed(g, page, page + 1, save);
    }
    open_guarded(g, first, last);
    for page in others {
        open_guarded(g, page, page + 1);
    }
    guarded
}

/// Gives write access back to the pages of `[first, last)` we guarded.
fn open_guarded(g: &mut Guards, first: usize, last: usize) {
    let guarded: Vec<usize> = g.protected.range(first..last).copied().collect();
    for page in guarded {
        open(g, page);
    }
}

/// Marks `[first, last)` changed in the trackers holding its pages. With
/// `save`, before the write lands: trackers armed for a page they had taken
/// keep its bytes.
fn mark_changed(g: &mut Guards, first: usize, last: usize, save: bool) {
    for (&page, holders) in g.holders.range(first..last) {
        for c in holders {
            let first_write = {
                let mut set = c.0.lock().unwrap_or_else(|e| e.into_inner());
                set_dirty(c.2, true);
                set.insert(page)
            };
            if first_write && save {
                c.save(page);
            }
        }
        #[cfg(feature = "diagnostics")]
        g.written.insert(page);
    }
}

/// At each presented frame (diagnostics): counts the tracked pages written
/// in it, and those written the frame before too.
#[cfg(feature = "diagnostics")]
pub(crate) fn frame_done() {
    let mut g = guards();
    let written = std::mem::take(&mut g.written);
    let again = written.intersection(&g.written_before).count();
    crate::copystat::add(crate::copystat::C::WrittenPages, written.len() as u64);
    crate::copystat::add(crate::copystat::C::WrittenAgain, again as u64);
    g.written_before = written;
}

/// A write into title memory the guards do not see, under way: its pages
/// stay open (a tracker taking them meanwhile would guard them, and the
/// kernel's write would fail) until it is dropped, which marks them changed
/// once more.
#[must_use = "the write must happen while this lives"]
pub struct Writing {
    first: usize,
    last: usize,
}

impl Drop for Writing {
    fn drop(&mut self) {
        let mut g = guards();
        if let Some(i) = g.busy.iter().position(|&r| r == (self.first, self.last)) {
            g.busy.swap_remove(i);
        }
        changed(&mut g, self.first, self.last, false);
    }
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler) or another write the
/// guards do not see: opens its guarded blocks, marks them changed and
/// keeps them open while the result lives. The runtime calls it (as
/// `kernel_will_write`) around the title's kernel reads into its own
/// memory and DirectStorage's writes to addresses.
pub fn writing(at: usize, len: usize) -> Writing {
    // The length is the title's (`fread`'s size times count, saturated).
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    let mut g = guards();
    g.busy.push((first, last));
    changed(&mut g, first, last, true);
    Writing { first, last }
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new, and read-write.
pub fn title_memory_remapped(at: usize, len: usize) {
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    let mut g = guards();
    // Remapped memory may no longer be a `MEM_WRITE_WATCH` pool: clear any
    // hot write-watch state for `[first, last)` before marking it changed.
    let watched: Vec<usize> = g.watched.range(first..last).copied().collect();
    for page in watched {
        remove_watched(&mut g, page, None);
    }
    let streaks: Vec<usize> = g.hot_streak.range(first..last).map(|(&p, _)| p).collect();
    for page in streaks {
        g.hot_streak.remove(&page);
    }
    // What it showed is gone already: nothing to keep.
    changed(&mut g, first, last, false);
    // What it showed is not what it shows now: its links go, both ways.
    let stale: Vec<usize> = g.alias_of.range(first..last).map(|(&p, _)| p).collect();
    for page in stale {
        for other in g.alias_of.remove(&page).unwrap_or_default() {
            if let Some(links) = g.alias_of.get_mut(&other) {
                links.retain(|&p| p != page);
                if links.is_empty() {
                    g.alias_of.remove(&other);
                }
            }
        }
    }
}

/// What shaders wrote into host copies of `[at, at + len)` is dropped:
/// every host buffer copy holding the memory takes it from title memory
/// again at its next refresh (the pages count as changed in each, aliases
/// too). Title memory itself did not change, so its guards stay (opened page
/// by page, they cost ~15 % of the submitting thread at each frame end, run
/// perf15), and other trackers (textures' staging, heaps) are left alone:
/// shaders' writes never reach them (marked, textures over withheld ranges
/// were uploaded again for nothing: 19 % of the uploads, run evict3).
pub(crate) fn retake(at: usize, len: usize) {
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    let g = guards();
    let others: Vec<usize> = g.alias_of.range(first..last).flat_map(|(_, o)| o.iter().copied()).collect();
    mark(&g, first, last);
    for page in others {
        mark(&g, page, page + 1);
    }
}

/// Pages `[first, last)` count as changed in every host buffer copy's
/// tracker holding them.
fn mark(g: &Guards, first: usize, last: usize) {
    for (&page, holders) in g.holders.range(first..last) {
        for c in holders.iter().filter(|c| c.3) {
            let mut set = c.0.lock().unwrap_or_else(|e| e.into_inner());
            set_dirty(c.2, true);
            set.insert(page);
        }
    }
}

/// The runtime mapped the page at `at` where `others` show it already: the
/// pages are linked both ways, and if we guard it there, it is guarded here
/// too (a write through the new mapping must not go unseen by the trackers
/// there).
pub fn title_memory_aliased(at: usize, others: &[usize]) {
    let mut g = guards();
    let page = at / PAGE;
    // Aliased pages can be written through any mapping; demote any watched
    // page involved back to `PAGE_READONLY` guards so writes through any alias
    // mark every mapping.
    demote_watched(&mut g, page, true);
    g.hot_streak.remove(&page);
    for &other in others {
        let op = other / PAGE;
        demote_watched(&mut g, op, true);
        g.hot_streak.remove(&op);
    }
    for &other in others {
        for (x, y) in [(other / PAGE, page), (page, other / PAGE)] {
            let links = g.alias_of.entry(x).or_default();
            if !links.contains(&y) {
                links.push(y);
            }
        }
    }
    // If the new mapping cannot be guarded (a write the guards do not see is
    // under way through it), the page is changed; its bytes now may hold that
    // write already, so nothing is saved of them.
    if others.iter().any(|&o| g.protected.contains(&(o / PAGE)))
        && protect(&mut g, &[page]).is_empty()
    {
        changed(&mut g, page, page + 1, false);
    }
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS
            || r.NumberParameters < 2
            || r.ExceptionInformation[0] != WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        #[cfg(feature = "diagnostics")]
        let _timed = Timed(std::time::Instant::now(), crate::copystat::C::HandlerUs);
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        let mut g = guards();
        let page = at / PAGE;
        if changed(&mut g, page, page + 1, true) {
            let streak = g.hot_streak.entry(page).or_insert(0);
            *streak = streak.saturating_add(1).min(HOT_THRESHOLD);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if writable(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_write));
    });
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

/// Adds the time since it was made to a counter when dropped (diagnostics).
#[cfg(feature = "diagnostics")]
pub(crate) struct Timed(pub(crate) std::time::Instant, pub(crate) crate::copystat::C);

#[cfg(feature = "diagnostics")]
impl Drop for Timed {
    fn drop(&mut self) {
        let c = std::mem::replace(&mut self.1, crate::copystat::C::Submissions);
        crate::copystat::add(c, self.0.elapsed().as_micros() as u64);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    pub(crate) const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// A write-watched allocation of `pages` pages (as the runtime's pools),
    /// on a 64 KiB boundary (the allocation granularity).
    pub(crate) fn pool(pages: usize) -> usize {
        // SAFETY: a fresh allocation, freed by the test.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                pages * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert!(p != 0 && p.is_multiple_of(PAGE));
        p
    }

    pub(crate) fn free(p: usize) {
        // SAFETY: an allocation of ours.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// Everything is changed at first; after taking, only what is written
    /// (by faults, or reported) is, and every tracker of a page sees it.
    #[test]
    fn writes_mark_every_tracker_of_their_page() {
        let p = pool(4);
        let a = track(p, 4 * PAGE, true).expect("pool memory is tracked");
        let b = track(p + PAGE, 2 * PAGE, true).expect("tracked");
        assert!(!a.clean() && !b.clean(), "changed at first");
        assert_eq!(a.take(p, p + 4 * PAGE), [(p, p + 4 * PAGE)]);
        assert!(a.clean() && !b.clean());
        assert_eq!(b.take(p, p + 4 * PAGE), [(p + PAGE, p + 3 * PAGE)]);
        assert!(a.take(p, p + 4 * PAGE).is_empty() && b.take(p, p + 4 * PAGE).is_empty());
        assert!(a.clean() && b.clean());
        // SAFETY: committed pages of ours, now read-only: the handler opens
        // the block; the second write needs no fault.
        unsafe {
            ((p + PAGE + 8) as *mut u8).write_volatile(1);
            ((p + PAGE + 9) as *mut u8).write_volatile(2);
        }
        assert!(!a.clean() && !b.clean(), "a write is seen by every tracker");
        assert_eq!(a.take(p, p + 4 * PAGE), [(p + PAGE, p + 2 * PAGE)]);
        assert!(a.clean());
        // `a` guarded it again; `b` still has it changed.
        assert!(b.changed(p / PAGE + 1));
        assert_eq!(b.take(p, p + 4 * PAGE), [(p + PAGE, p + 2 * PAGE)]);
        // SAFETY: as above: a fault again, for both.
        unsafe { ((p + PAGE + 10) as *mut u8).write_volatile(3) };
        assert!(a.changed(p / PAGE + 1) && b.changed(p / PAGE + 1));
        assert_eq!(a.take(p, p + 4 * PAGE), [(p + PAGE, p + 2 * PAGE)]);
        drop(writing(p + 3 * PAGE + 5, 1));
        assert_eq!(a.take(p, p + 4 * PAGE), [(p + 3 * PAGE, p + 4 * PAGE)]);
        title_memory_remapped(p, PAGE);
        assert!(a.changed(p / PAGE) && !a.changed(p / PAGE + 1));
        drop((a, b));
        free(p);
    }

    /// A range running from free memory over another allocation into a pool
    /// (a 4 MiB window of memory outside buffers) tracks the pool's pages
    /// and leaves the other allocation alone: taking guards the pool only.
    #[test]
    fn a_range_over_foreign_memory_guards_only_the_runtimes() {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_READONLY, VirtualQuery};
        const WINDOW: usize = 4 << 20;
        // In a free 4 MiB-aligned window: plain committed memory (as the
        // process heap's) 1 MiB in, a pool 2 MiB in. Other tests allocate
        // meanwhile, so a window taken under our feet is tried again.
        let (base, foreign, pool) = (0..8)
            .find_map(|_| {
                // SAFETY: a fresh reservation, released at once; then
                // allocations in the window it left free, freed below.
                unsafe {
                    let p = VirtualAlloc(std::ptr::null(), 2 * WINDOW, MEM_RESERVE, PAGE_READWRITE) as usize;
                    VirtualFree(p as *mut _, 0, MEM_RELEASE);
                    let base = p.next_multiple_of(WINDOW);
                    let at = |offset: usize, kind: u32| {
                        VirtualAlloc((base + offset) as *const _, PAGE, MEM_RESERVE | MEM_COMMIT | kind, PAGE_READWRITE) as usize
                    };
                    let (foreign, pool) = (at(1 << 20, 0), at(2 << 20, MEM_WRITE_WATCH));
                    if p != 0 && foreign == base + (1 << 20) && pool == base + (2 << 20) {
                        return Some((base, foreign, pool));
                    }
                    [foreign, pool].into_iter().filter(|&q| q != 0).for_each(free);
                    None
                }
            })
            .expect("a free window");
        let t = track(base, WINDOW, true).expect("the pool is tracked");
        let taken = t.take(base, base + WINDOW);
        assert!(taken.iter().any(|&(a, b)| a <= pool && pool + PAGE <= b), "{taken:x?}");
        assert!(!taken.iter().any(|&(a, b)| a < foreign + PAGE && foreign < b), "{taken:x?}");
        let protection = |p: usize| {
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
            info.Protect
        };
        assert_eq!(protection(pool), PAGE_READONLY, "the pool is guarded");
        assert_eq!(protection(foreign), PAGE_READWRITE, "the other allocation is not");
        drop(t);
        free(foreign);
        free(pool);
    }

    /// A tracker's dirty bit goes back when it goes, and the next tracker
    /// starts dirty on it (everything changed at first).
    #[test]
    fn a_new_tracker_starts_dirty_on_a_bit_given_back() {
        let p = pool(1);
        let a = track(p, PAGE, true).expect("tracked");
        assert_eq!(a.take(p, p + PAGE).len(), 1);
        assert!(a.clean() && clean_bit(a.bit()));
        drop(a);
        let b = track(p, PAGE, true).expect("tracked");
        assert!(!b.clean() && !clean_bit(b.bit()));
        assert!(!clean_bit(None), "no bit: never clean");
        drop(b);
        free(p);
    }

    /// A page with nothing mapped is taken as zeros: `take_split` lists it
    /// apart from the pages to copy.
    #[test]
    fn unmapped_pages_are_taken_apart() {
        // SAFETY: a fresh write-watched reservation of two pages, the first
        // committed; freed below.
        let p = unsafe {
            let p = VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE | MEM_WRITE_WATCH, PAGE_READWRITE) as usize;
            assert_ne!(p, 0);
            assert_ne!(VirtualAlloc(p as *const _, PAGE, MEM_COMMIT, PAGE_READWRITE) as usize, 0);
            p
        };
        let t = track(p, 2 * PAGE, true).expect("tracked");
        assert_eq!(t.take_split(p, p + 2 * PAGE), (vec![(p, p + PAGE)], vec![(p + PAGE, p + 2 * PAGE)]));
        assert!(t.take(p, p + 2 * PAGE).is_empty(), "both taken");
        drop(t);
        free(p);
    }

    /// A page only partly in the range asked is copied in part and stays
    /// changed; edge pages count whole when the tracked part is covered.
    #[test]
    fn partial_pages_stay_changed() {
        let p = pool(3);
        let t = track(p + 100, 2 * PAGE, true).expect("tracked");
        assert_eq!(t.take(p + 100, p + PAGE + 10), [(p + 100, p + PAGE + 10)]);
        assert!(!t.changed(p / PAGE) && t.changed(p / PAGE + 1));
        assert!(!t.clean(), "a page taken in part stays changed");
        assert_eq!(t.take(p, p + 3 * PAGE), [(p + PAGE, p + 2 * PAGE + 100)]);
        assert!(t.take(p, p + 3 * PAGE).is_empty());
        assert!(t.clean());
        drop(t);
        free(p);
    }

    /// Pages of the ranges armed keep their bytes from before the first
    /// write after they were taken, whichever overlapping ranges armed
    /// them; a page unarmed keeps none until it is armed again, and none
    /// does once the tracker is disarmed.
    #[test]
    fn armed_ranges_keep_their_pages_bytes_from_before_the_first_write() {
        let p = pool(4);
        let t = track(p, 4 * PAGE, true).expect("pool memory is tracked");
        let all = || t.take(p, p + 4 * PAGE);
        // SAFETY: (every write below) a byte of our pool; a guarded page
        // faults into the handler, which saves it first where it is armed.
        let write = |page: usize, value: u8| unsafe { ((p + page * PAGE + 8) as *mut u8).write_volatile(value) };
        let saved = |page: usize| t.saved(p / PAGE + page).map(|bytes| bytes[8]);
        all();
        write(0, 1);
        assert_eq!(saved(0), None, "nothing armed yet");
        all();
        // Bytes 100 of page 0 to 1 of page 2: pages 0 to 2, page 3 not.
        t.arm(p + 100, p + 2 * PAGE + 1);
        t.arm(p + PAGE, p + 2 * PAGE);
        t.arm(p + PAGE + 5, p + 2 * PAGE + 1);
        t.unarm(p / PAGE + 1);
        for page in 0..4 {
            write(page, 2);
        }
        assert_eq!([saved(0), saved(1), saved(2), saved(3)], [Some(1), None, Some(0), None]);
        assert_eq!(saved(0), None, "handed out once");
        all();
        // Armed again by a range holding it.
        t.arm(p + PAGE, p + 2 * PAGE);
        write(1, 3);
        write(1, 4);
        assert_eq!(saved(1), Some(2), "from before the first write");
        t.disarm();
        all();
        write(2, 5);
        assert_eq!(saved(2), None, "disarmed");
        drop(t);
        free(p);
    }

    /// Runs armed that touch or overlap merge into one, so a page's check
    /// stays a search however many sub-ranges are armed; pages unarmed in
    /// them stay unarmed until a range holding them is armed again.
    #[test]
    fn adjacent_and_overlapping_armed_runs_coalesce() {
        let mut s = Saved::default();
        // Adjacent: [2k, 2k + 2) end to end, in order and out of it.
        for k in 0..200 {
            s.arm(2 * k, 2 * k + 2);
        }
        assert_eq!(s.armed, [(0, 400)]);
        let mut s = Saved::default();
        for k in (0..200).rev() {
            s.arm(2 * k, 2 * k + 2);
        }
        assert_eq!(s.armed, [(0, 400)]);
        // Overlapping, nested and disjoint.
        let mut s = Saved::default();
        s.arm(10, 20);
        s.arm(15, 25);
        assert_eq!(s.armed, [(10, 25)]);
        s.arm(12, 14);
        assert_eq!(s.armed, [(10, 25)], "nested");
        s.arm(40, 50);
        s.arm(0, 5);
        assert_eq!(s.armed, [(0, 5), (10, 25), (40, 50)], "disjoint, sorted");
        s.arm(5, 10);
        assert_eq!(s.armed, [(0, 25), (40, 50)], "touching both sides");
        s.arm(30, 35);
        s.arm(20, 45);
        assert_eq!(s.armed, [(0, 50)], "spanning several");
        s.arm(60, 70);
        s.arm(55, 55);
        assert_eq!(s.armed, [(0, 50), (60, 70)], "an empty run arms nothing");
        assert!(s.armed(0) && s.armed(49) && !s.armed(50) && !s.armed(59));
        assert!(s.armed(60) && s.armed(69) && !s.armed(70) && !s.armed(1000));
        // Unarmed pages stay out of a merged run until armed again.
        s.unarmed.insert(45);
        s.unarmed.insert(65);
        s.arm(50, 60);
        assert_eq!(s.armed, [(0, 70)]);
        assert!(!s.armed(45) && !s.armed(65) && s.armed(55));
        s.arm(44, 46);
        assert!(s.armed(45) && !s.armed(65));
        s.arm(0, 70);
        assert!(s.armed(65) && s.unarmed.is_empty());
    }

    /// Plain committed memory (the C heap) is not tracked; reserved memory
    /// is, reads as taken, and changes when the runtime maps it.
    #[test]
    fn heap_memory_is_not_tracked_and_placeholders_wait_for_mappings() {
        let plain = vec![0u8; 2 * PAGE];
        assert!(track(plain.as_ptr() as usize, plain.len(), true).is_none());
        // SAFETY: a reservation of ours, freed below.
        let r = unsafe { VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE, PAGE_READWRITE) } as usize;
        assert_ne!(r, 0);
        let t = track(r, 2 * PAGE, true).expect("reserved memory is tracked");
        assert_eq!(t.take(r, r + 2 * PAGE).len(), 1, "copied once (as zeros)");
        assert!(t.take(r, r + 2 * PAGE).is_empty());
        title_memory_remapped(r, PAGE);
        assert_eq!(t.take(r, r + 2 * PAGE), [(r, r + PAGE)]);
        drop(t);
        // SAFETY: our reservation.
        unsafe { VirtualFree(r as *mut _, 0, MEM_RELEASE) };
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as changed.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * PAGE) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * PAGE).Value as usize
        };
        assert_ne!(view, 0);
        assert!(track(view, 2 * PAGE, false).is_none(), "views only where asked");
        let t = track(view, 2 * PAGE, true).expect("views are tracked");
        assert_eq!(t.take(view, view + 2 * PAGE), [(view, view + 2 * PAGE)]); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(!read(&mut file), "the kernel does not fault into our handler");
        let write = writing(view + 100, 13);
        // A tracker taking the page meanwhile leaves it open.
        assert_eq!(t.take(view, view + 2 * PAGE), [(view, view + PAGE)]);
        assert!(read(&mut file));
        drop(write);
        // SAFETY: the bytes just read.
        assert_eq!(unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) }, b"from the disk");
        assert_eq!(t.take(view, view + 2 * PAGE), [(view, view + PAGE)]);
        drop(t);
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
    }

    /// `pages` pages of a fresh pagefile section mapped twice (as the
    /// title's pool view and tiled heap show the same physical pages),
    /// reported as the runtime does when it maps them.
    pub(crate) fn twin_views(pages: usize) -> (usize, usize) {
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile};
        let len = pages * PAGE;
        // SAFETY: a pagefile section mapped read-write twice; the test
        // keeps both views.
        let (a, b) = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                len as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (
                MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize,
                MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize,
            )
        };
        assert!(a != 0 && b != 0);
        for k in (0..len).step_by(PAGE) {
            title_memory_aliased(b + k, &[a + k]);
        }
        (a, b)
    }

    /// A write through another mapping of the same pages marks the tracker
    /// (the guard there faults, the runtime says where else it is mapped);
    /// so does a write the runtime reports at the other mapping.
    #[test]
    fn writes_through_another_mapping_mark_the_tracker() {
        let (a, b) = twin_views(2);
        let t = track(a, 2 * PAGE, true).expect("views are tracked");
        assert_eq!(t.take(a, a + 2 * PAGE), [(a, a + 2 * PAGE)]);
        // SAFETY: the second view of the pages, guarded by the take.
        unsafe { ((b + PAGE + 4) as *mut u32).write_volatile(7) };
        assert_eq!(t.take(a, a + 2 * PAGE), [(a + PAGE, a + 2 * PAGE)]);
        // SAFETY: the tracked view, which shows the write.
        assert_eq!(unsafe { ((a + PAGE + 4) as *const u32).read_volatile() }, 7);
        drop(writing(b, 8));
        assert_eq!(t.take(a, a + 2 * PAGE), [(a, a + PAGE)]);
        assert!(t.take(a, a + 2 * PAGE).is_empty());
        // A third mapping made later starts guarded, so its writes count.
        let (c, _) = twin_views(1);
        title_memory_aliased(c, &[a]);
        // SAFETY: a view of ours (another section: only the tracking
        // takes it for `a`'s pages here).
        unsafe { (c as *mut u32).write_volatile(1) };
        assert_eq!(t.take(a, a + 2 * PAGE), [(a, a + PAGE)]);
    }

    /// A tracker of one mapping going away leaves the guard its page shares
    /// with a tracker of another mapping, so a write there still counts (it
    /// was opened, and the pages it went unseen in stayed clean).
    #[test]
    fn a_tracker_dropped_on_one_mapping_leaves_the_other_guarded() {
        let (a, b) = twin_views(1);
        let t = track(a, PAGE, true).expect("views are tracked");
        let u = track(b, PAGE, true).expect("views are tracked");
        assert_eq!(t.take(a, a + PAGE), [(a, a + PAGE)]);
        assert_eq!(u.take(b, b + PAGE), [(b, b + PAGE)]);
        drop(u);
        // SAFETY: the second view, guarded for `t`'s page too.
        unsafe { ((b + 4) as *mut u32).write_volatile(7) };
        assert_eq!(t.take(a, a + PAGE), [(a, a + PAGE)]);
    }

    /// A retake has every copy of the memory, and of its other mappings,
    /// take it again, and leaves it guarded: a later write still counts.
    #[test]
    fn a_retake_marks_the_copies_and_keeps_the_guard() {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_READONLY, VirtualQuery};
        let protection = |at: usize| {
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
            info.Protect
        };
        let (a, b) = twin_views(2);
        let t = track_copy(a, 2 * PAGE, true).expect("views are tracked");
        let u = track_copy(b, 2 * PAGE, true).expect("views are tracked");
        assert_eq!(t.take(a, a + 2 * PAGE), [(a, a + 2 * PAGE)]);
        assert_eq!(u.take(b, b + 2 * PAGE), [(b, b + 2 * PAGE)]);
        assert!(t.clean() && u.clean());
        retake(a + PAGE + 16, 32);
        // Marked in both copies, the second mapping's too, still guarded.
        assert!(!t.clean() && !u.clean());
        assert!(t.changed((a + PAGE) / PAGE) && u.changed((b + PAGE) / PAGE));
        assert!(!t.changed(a / PAGE));
        assert_eq!(protection(a + PAGE), PAGE_READONLY);
        assert_eq!(protection(b + PAGE), PAGE_READONLY);
        // Taken again (the copies re-copy that page), then guarded and clean.
        assert_eq!(t.take(a, a + 2 * PAGE), [(a + PAGE, a + 2 * PAGE)]);
        assert_eq!(u.take(b, b + 2 * PAGE), [(b + PAGE, b + 2 * PAGE)]);
        assert!(t.clean() && u.clean());
        assert_eq!(protection(a + PAGE), PAGE_READONLY);
        // SAFETY: the second view, guarded all along.
        unsafe { ((b + PAGE + 8) as *mut u32).write_volatile(3) };
        assert_eq!(t.take(a, a + 2 * PAGE), [(a + PAGE, a + 2 * PAGE)]);
        assert_eq!(u.take(b, b + 2 * PAGE), [(b + PAGE, b + 2 * PAGE)]);
        // A retake while marked and guarded, then a drop: nothing breaks.
        retake(a, 2 * PAGE);
        drop(u);
        assert_eq!(t.take(a, a + 2 * PAGE), [(a, a + 2 * PAGE)]);
        assert!(t.clean());
    }

    /// A retake marks host buffer copies holding the memory, not other
    /// trackers of it (a texture's staging): shaders' writes reach only the
    /// copies. A title write still marks both.
    #[test]
    fn a_retake_marks_buffer_copies_alone() {
        let p = pool(2);
        let copy = track_copy(p, 2 * PAGE, true).expect("pool memory is tracked");
        let texture = track(p, 2 * PAGE, true).expect("pool memory is tracked");
        assert_eq!(copy.take(p, p + 2 * PAGE), [(p, p + 2 * PAGE)]);
        assert_eq!(texture.take(p, p + 2 * PAGE), [(p, p + 2 * PAGE)]);
        retake(p + PAGE, 16);
        assert!(copy.changed(p / PAGE + 1), "the copy takes it again");
        assert!(!texture.changed(p / PAGE + 1) && texture.clean(), "the texture's tracker is left alone");
        assert_eq!(copy.take(p, p + 2 * PAGE), [(p + PAGE, p + 2 * PAGE)]);
        // SAFETY: a page of our pool, guarded: the handler opens it.
        unsafe { ((p + PAGE + 8) as *mut u8).write_volatile(1) };
        assert!(copy.changed(p / PAGE + 1) && texture.changed(p / PAGE + 1), "a title write marks both");
    }

    /// A write under way on another mapping of a page keeps the page open
    /// there, so taking the page (or marking it taken) through its tracked
    /// mapping must leave it changed and keep what was saved of it; aliasing
    /// a busy mapping to a guarded one must mark the guarded page changed.
    #[test]
    fn a_busy_alias_leaves_the_page_changed_and_keeps_saved_bytes() {
        let (a, b) = twin_views(1);
        let t = track(a, PAGE, true).expect("views are tracked");
        assert_eq!(t.take(a, a + PAGE), [(a, a + PAGE)]);
        t.arm(a, a + PAGE);
        let write = writing(b, 16);
        assert!(t.changed(a / PAGE));
        assert_eq!(t.take(a, a + PAGE), [(a, a + PAGE)]);
        assert!(t.changed(a / PAGE), "take must leave the page changed");
        t.taken(&[a / PAGE]);
        assert!(t.changed(a / PAGE), "taken must leave the page changed");
        // SAFETY: the second view of the page, kept open by `write`.
        unsafe { (b as *mut u32).write_volatile(0x1234) };
        assert_eq!(t.take(a, a + PAGE), [(a, a + PAGE)]);
        drop(write);
        let saved = t.saved(a / PAGE).expect("saved bytes survive");
        assert_eq!(&saved[..4], &[0, 0, 0, 0]);
        assert_eq!(t.take(a, a + PAGE), [(a, a + PAGE)]);
        assert!(t.take(a, a + PAGE).is_empty(), "taken once the write ended");
        let (c, _) = twin_views(1);
        let busy_c = writing(c, 16);
        title_memory_aliased(c, &[a]);
        assert!(
            t.changed(a / PAGE),
            "aliasing a busy page marks the tracked one"
        );
        drop(busy_c);
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        drop(writing(top, PAGE));
        title_memory_remapped(top, usize::MAX);
    }

    /// Helper: current Win32 page protection at `at`.
    fn page_protection(at: usize) -> u32 {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by VirtualQuery.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid pointer.
        unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        info.Protect
    }

    /// A hot `MEM_WRITE_WATCH` buffer-copy page promotes to hardware
    /// write-watch after `HOT_THRESHOLD` guard faults (staying `PAGE_READWRITE`
    /// with zero faults), detects every CPU write, and cools back down to
    /// `PAGE_READONLY` + `clean()` after `COOL_TAKES` quiet takes.
    #[test]
    fn hot_write_watch_pages_promote_and_cool_without_faulting() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let p = pool(2);
        let t = track_copy(p, 2 * PAGE, true).expect("pool memory is tracked");
        assert_eq!(t.take(p, p + 2 * PAGE), [(p, p + 2 * PAGE)]);
        assert_eq!(page_protection(p), PAGE_READONLY);
        // Trigger HOT_THRESHOLD guard faults on page 0 only.
        for k in 1..=u32::from(HOT_THRESHOLD) {
            // SAFETY: page 0 of our write-watched pool.
            unsafe { (p as *mut u32).write_volatile(k) };
            assert_eq!(t.take(p, p + 2 * PAGE), [(p, p + PAGE)]);
        }
        // Page 0 is now watched (PAGE_READWRITE), page 1 stays guarded.
        assert_eq!(page_protection(p), PAGE_READWRITE);
        assert_eq!(page_protection(p + PAGE), PAGE_READONLY);
        assert!(guards().watched.contains(&(p / PAGE)));
        assert!(!t.clean(), "watched tracker stays pollable");
        // Subsequent writes land without faulting and are still reported.
        for k in 10..15u32 {
            // SAFETY: page 0 is read-write (no fault).
            unsafe { ((p + 128) as *mut u32).write_volatile(k) };
            assert_eq!(page_protection(p), PAGE_READWRITE);
            assert_eq!(t.take(p, p + 2 * PAGE), [(p, p + PAGE)]);
            assert_eq!(page_protection(p), PAGE_READWRITE);
        }
        // Quiet takes return nothing and cool the page back to a guard.
        for _ in 0..COOL_TAKES {
            assert!(t.take(p, p + 2 * PAGE).is_empty());
        }
        assert!(!guards().watched.contains(&(p / PAGE)));
        assert_eq!(page_protection(p), PAGE_READONLY);
        assert!(t.clean(), "cooled tracker returns to zero-syscall clean()");
        drop(t);
        free(p);
    }

    /// Two host buffer copies sharing a hot `MEM_WRITE_WATCH` page both see a
    /// CPU write even though the first copy's `take()` resets `GetWriteWatch`.
    #[test]
    fn hot_write_watch_page_shared_by_two_copies_notifies_both() {
        let p = pool(1);
        let a = track_copy(p, PAGE, true).expect("pool memory is tracked");
        let b = track_copy(p, PAGE, true).expect("pool memory is tracked");
        assert_eq!(a.take(p, p + PAGE), [(p, p + PAGE)]);
        assert_eq!(b.take(p, p + PAGE), [(p, p + PAGE)]);
        for k in 1..=u32::from(HOT_THRESHOLD) {
            // SAFETY: page of our write-watched pool.
            unsafe { (p as *mut u32).write_volatile(k) };
            assert_eq!(a.take(p, p + PAGE), [(p, p + PAGE)]);
            assert_eq!(b.take(p, p + PAGE), [(p, p + PAGE)]);
        }
        assert!(guards().watched.contains(&(p / PAGE)));
        // Write once, take via `a` (resets OS write-watch), then verify `b` still sees it.
        // SAFETY: page of our write-watched pool.
        unsafe { ((p + 4096) as *mut u32).write_volatile(0xCAFE_BABE) };
        assert_eq!(a.take(p, p + PAGE), [(p, p + PAGE)]);
        assert_eq!(b.take(p, p + PAGE), [(p, p + PAGE)]);
        drop(a);
        drop(b);
        free(p);
    }

    /// Arming a range covering a hot write-watched page immediately demotes it
    /// to a `PAGE_READONLY` guard so the next CPU write saves pre-write bytes.
    #[test]
    fn arming_a_hot_write_watch_page_demotes_it_and_saves_pre_write_bytes() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let p = pool(1);
        let t = track_copy(p, PAGE, true).expect("pool memory is tracked");
        assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
        for k in 1..=u32::from(HOT_THRESHOLD) {
            // SAFETY: page of our write-watched pool.
            unsafe { (p as *mut u32).write_volatile(k) };
            assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
        }
        assert!(guards().watched.contains(&(p / PAGE)));
        // SAFETY: write known pre-submission value and take it.
        unsafe { (p as *mut u32).write_volatile(0xAAAA_5555) };
        assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
        // Arming demotes back to PAGE_READONLY.
        t.arm(p, p + PAGE);
        assert!(!guards().watched.contains(&(p / PAGE)));
        assert_eq!(page_protection(p), PAGE_READONLY);
        // Concurrent CPU write faults and saves 0xAAAA_5555.
        // SAFETY: guarded page of our pool; handler opens it and saves old bytes.
        unsafe { (p as *mut u32).write_volatile(0xDEAD_BEEF) };
        let saved = t.saved(p / PAGE).expect("pre-write bytes saved");
        assert_eq!(u32::from_le_bytes(saved[..4].try_into().unwrap()), 0xAAAA_5555);
        t.disarm();
        drop(t);
        free(p);
    }

    /// `Tracked::changed` on a hot write-watched page returns `false` when
    /// untouched (so marked GPU copy-back is never dropped) and `true` once
    /// the CPU writes to the page.
    #[test]
    fn changed_on_hot_write_watch_page_distinguishes_written_from_untouched() {
        let p = pool(1);
        let t = track_copy(p, PAGE, true).expect("pool memory is tracked");
        assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
        for k in 1..=u32::from(HOT_THRESHOLD) {
            // SAFETY: page of our write-watched pool.
            unsafe { (p as *mut u32).write_volatile(k) };
            assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
        }
        assert!(guards().watched.contains(&(p / PAGE)));
        assert!(!t.changed(p / PAGE), "untouched watched page is not changed");
        // SAFETY: write to watched page without faulting.
        unsafe { ((p + 8192) as *mut u32).write_volatile(0x1337) };
        assert!(t.changed(p / PAGE), "CPU write is visible to changed()");
        assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
        assert!(!t.changed(p / PAGE), "clean again after take()");
        drop(t);
        free(p);
    }

    /// Adding a non-copy tracker (texture staging), calling `taken()`, or
    /// remapping/aliasing demotes a hot write-watched page back to guards.
    #[test]
    fn non_copy_taken_and_remap_demote_hot_write_watch_pages() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let p = pool(2);
        let copy = track_copy(p, 2 * PAGE, true).expect("pool memory is tracked");
        assert_eq!(copy.take(p, p + 2 * PAGE), [(p, p + 2 * PAGE)]);
        for k in 1..=u32::from(HOT_THRESHOLD) {
            // SAFETY: pages 0 and 1 of our pool.
            unsafe {
                (p as *mut u32).write_volatile(k);
                ((p + PAGE) as *mut u32).write_volatile(k);
            }
            assert_eq!(copy.take(p, p + 2 * PAGE), [(p, p + 2 * PAGE)]);
        }
        assert!(guards().watched.contains(&(p / PAGE)));
        assert!(guards().watched.contains(&(p / PAGE + 1)));

        // `taken()` (GPU copy-back) demotes page 0.
        copy.taken(&[p / PAGE]);
        assert!(!guards().watched.contains(&(p / PAGE)));
        assert_eq!(page_protection(p), PAGE_READONLY);

        // Tracking with `copy=false` (texture staging) demotes page 1.
        let tex = track(p + PAGE, PAGE, true).expect("pool memory is tracked");
        assert!(!guards().watched.contains(&(p / PAGE + 1)));
        drop(tex);

        // Re-promote page 0 and verify `title_memory_remapped` demotes it.
        for k in 1..=u32::from(HOT_THRESHOLD) {
            // SAFETY: page 0 of our pool.
            unsafe { (p as *mut u32).write_volatile(100 + k) };
            assert_eq!(copy.take(p, p + 2 * PAGE), [(p, p + PAGE)]);
        }
        assert!(guards().watched.contains(&(p / PAGE)));
        title_memory_remapped(p, PAGE);
        assert!(!guards().watched.contains(&(p / PAGE)));

        drop(copy);
        free(p);
    }

    /// Multi-threaded hot-page churn: a writer thread continuously updates 32
    /// hot pages while the taker thread takes them every frame. Verifies zero
    /// missed final writes and measures hot write-watch throughput.
    #[test]
    fn hot_write_watch_concurrent_churn_loses_no_writes() {
        const PAGES: usize = 32;
        const ROUNDS: u32 = 120;
        let p = pool(PAGES);
        let t = track_copy(p, PAGES * PAGE, true).expect("pool memory is tracked");
        assert_eq!(t.take(p, p + PAGES * PAGE), [(p, p + PAGES * PAGE)]);
        // Warm up into hardware write-watch mode.
        for r in 1..=u32::from(HOT_THRESHOLD) {
            for k in 0..PAGES {
                // SAFETY: page `k` of our pool.
                unsafe { ((p + k * PAGE) as *mut u32).write_volatile(r) };
            }
            assert_eq!(t.take(p, p + PAGES * PAGE), [(p, p + PAGES * PAGE)]);
        }
        for k in 0..PAGES {
            assert!(guards().watched.contains(&(p / PAGE + k)));
        }
        let start = std::time::Instant::now();
        let writer = std::thread::spawn(move || {
            for r in 1..=ROUNDS {
                for k in 0..PAGES {
                    // SAFETY: page `k` of our pool, valid until thread joins.
                    unsafe { ((p + k * PAGE) as *mut u32).write_volatile(r) };
                }
                std::hint::spin_loop();
            }
        });
        let mut shadow = vec![0u32; PAGES];
        let mut takes = 0u32;
        while !writer.is_finished() {
            for (a, b) in t.take(p, p + PAGES * PAGE) {
                for page in a / PAGE..b.div_ceil(PAGE) {
                    let k = page - p / PAGE;
                    // SAFETY: page `k` of our pool.
                    shadow[k] = unsafe { ((p + k * PAGE) as *const u32).read_volatile() };
                }
            }
            takes += 1;
        }
        writer.join().unwrap();
        // Final take after writer finishes must observe ROUNDS on every page.
        for (a, b) in t.take(p, p + PAGES * PAGE) {
            for page in a / PAGE..b.div_ceil(PAGE) {
                let k = page - p / PAGE;
                // SAFETY: page `k` of our pool.
                shadow[k] = unsafe { ((p + k * PAGE) as *const u32).read_volatile() };
            }
        }
        let elapsed = start.elapsed();
        eprintln!(
            "hot_write_watch_concurrent_churn: {PAGES} pages x {ROUNDS} rounds ({takes} takes) in {elapsed:?}"
        );
        assert_eq!(shadow, vec![ROUNDS; PAGES], "every page must hold the final write");
        drop(t);
        free(p);
    }

    /// A/B benchmark comparing `PAGE_READONLY` guard faults (`track`, copy=false)
    /// against batched `MEM_WRITE_WATCH` (`track_copy`, copy=true) on 256 hot
    /// pages (16 MiB) written every frame for 100 frames.
    #[test]
    #[ignore = "benchmark"]
    fn bench_hot_write_watch_vs_guards() {
        const PAGES: usize = 256;
        const FRAMES: u32 = 100;
        let run = |as_copy: bool| -> std::time::Duration {
            let p = pool(PAGES);
            let t = if as_copy {
                track_copy(p, PAGES * PAGE, true)
            } else {
                track(p, PAGES * PAGE, true)
            }
            .expect("pool memory is tracked");
            let _ = t.take(p, p + PAGES * PAGE);
            // Warm up past HOT_THRESHOLD so we measure steady-state frame cost.
            for r in 1..=u32::from(HOT_THRESHOLD) {
                for k in 0..PAGES {
                    // SAFETY: page `k` of our pool.
                    unsafe { ((p + k * PAGE) as *mut u32).write_volatile(r) };
                }
                let _ = t.take(p, p + PAGES * PAGE);
            }
            let start = std::time::Instant::now();
            for r in 0..FRAMES {
                for k in 0..PAGES {
                    // SAFETY: page `k` of our pool.
                    unsafe { ((p + k * PAGE) as *mut u32).write_volatile(r) };
                }
                let runs = t.take(p, p + PAGES * PAGE);
                assert_eq!(runs, [(p, p + PAGES * PAGE)]);
            }
            let dt = start.elapsed();
            drop(t);
            free(p);
            dt
        };
        let guarded = run(false);
        let watched = run(true);
        let speedup = guarded.as_secs_f64() / watched.as_secs_f64();
        eprintln!(
            "256 hot pages (16 MiB) x {FRAMES} frames:\n  PAGE_READONLY guards + VEH: {guarded:?} ({:.2} ms/frame)\n  Batched MEM_WRITE_WATCH:    {watched:?} ({:.2} ms/frame)\n  Speedup:                    {speedup:.1}x",
            guarded.as_secs_f64() * 1e3 / f64::from(FRAMES),
            watched.as_secs_f64() * 1e3 / f64::from(FRAMES),
        );
    }

    /// End-to-end 8-thread + real D3D12 `Shadow::update` benchmark on the host
    /// GPU (RTX 3080 Ti): 8 title worker threads write 512 hot pages (32 MiB)
    /// per frame while the submitter refreshes the D3D12 shadow buffer for 100
    /// frames. Run with `--features diagnostics --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark"]
    fn bench_multithreaded_shadow_refresh_on_host_gpu() {
        use std::sync::Barrier;
        const THREADS: usize = 8;
        const PAGES_PER_THREAD: usize = 64;
        const PAGES: usize = THREADS * PAGES_PER_THREAD;
        const FRAMES: u32 = 100;
        let len = PAGES * PAGE;
        let p = pool(PAGES);
        crate::shadow::register(p, p as u64, len as u64);
        let Some(copy) = crate::shadow::find(p as u64, len as u64) else {
            crate::shadow::unregister(p, p as u64);
            free(p);
            eprintln!("no host GPU available");
            return;
        };
        // Warm up past HOT_THRESHOLD.
        let _ = copy.update(p, len);
        for r in 1..=u32::from(HOT_THRESHOLD) {
            for k in 0..PAGES {
                // SAFETY: page `k` of our pool.
                unsafe { ((p + k * PAGE) as *mut u32).write_volatile(r) };
            }
            let _ = copy.update(p, len);
        }
        let start_barrier = Arc::new(Barrier::new(THREADS + 1));
        let done_barrier = Arc::new(Barrier::new(THREADS + 1));
        let mut workers = Vec::with_capacity(THREADS);
        for tid in 0..THREADS {
            let sb = start_barrier.clone();
            let db = done_barrier.clone();
            workers.push(std::thread::spawn(move || {
                let base = p + tid * PAGES_PER_THREAD * PAGE;
                for frame in 0..FRAMES {
                    sb.wait();
                    for k in 0..PAGES_PER_THREAD {
                        // SAFETY: disjoint slice of our pool, valid until join.
                        unsafe { ((base + k * PAGE) as *mut u32).write_volatile(frame + 1) };
                    }
                    db.wait();
                }
            }));
        }
        let mut total_write = std::time::Duration::ZERO;
        let mut total_update = std::time::Duration::ZERO;
        let wall_start = std::time::Instant::now();
        for _ in 0..FRAMES {
            let t0 = std::time::Instant::now();
            start_barrier.wait();
            done_barrier.wait();
            let t1 = std::time::Instant::now();
            let copied = copy.update(p, len);
            let t2 = std::time::Instant::now();
            assert_eq!(copied, len);
            total_write += t1 - t0;
            total_update += t2 - t1;
        }
        let wall = wall_start.elapsed();
        for w in workers {
            w.join().unwrap();
        }
        let watched_now = guards().watched.range(p / PAGE..p / PAGE + PAGES).count();
        eprintln!(
            "8-thread + D3D12 Shadow::update ({PAGES} pages = {} MiB x {FRAMES} frames, watched={watched_now}/{PAGES}):\n  Total wall:   {wall:?} ({:.2} ms/frame)\n  CPU writes:   {total_write:?} ({:.2} ms/frame)\n  Shadow::upd:  {total_update:?} ({:.2} ms/frame)",
            len >> 20,
            wall.as_secs_f64() * 1e3 / f64::from(FRAMES),
            total_write.as_secs_f64() * 1e3 / f64::from(FRAMES),
            total_update.as_secs_f64() * 1e3 / f64::from(FRAMES),
        );
        crate::shadow::unregister(p, p as u64);
        free(p);
    }

    #[test]
    fn quiet_partial_refresh_preserves_the_next_write() {
        for quiet_len in [PAGE, 4] {
            let p = pool(1);
            let t = track_copy(p, PAGE, true).expect("write-watched pool");
            assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
            for value in 1..=u32::from(HOT_THRESHOLD) + 1 {
                // SAFETY: aligned word in our allocation.
                unsafe { (p as *mut u32).write_volatile(value) };
                assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
            }
            assert!(!t.clean(), "the page was promoted to write-watch");
            for _ in 0..COOL_TAKES {
                assert!(t.take(p, p + quiet_len).is_empty());
            }
            assert!(
                t.clean(),
                "the quiet tracker should return to its clean fast path"
            );
            let end_word = p + PAGE - 4;
            // SAFETY: last aligned word in the same allocation.
            unsafe { (end_word as *mut u32).write_volatile(99) };
            assert_eq!(
                t.take(end_word, p + PAGE),
                [(end_word, p + PAGE)],
                "a write after quiet {quiet_len}-byte refreshes must reach the next copy"
            );
            assert_eq!(
                t.take(p, p + PAGE),
                [(p, p + PAGE)],
                "partial take leaves the rest pending"
            );
            assert!(t.take(p, p + PAGE).is_empty());
            drop(t);
            free(p);
        }
    }

    #[test]
    fn hot_page_stays_visible_to_both_holders_while_a_writer_is_open() {
        let p = pool(1);
        let a = track_copy(p, PAGE, true).unwrap();
        let b = track_copy(p, PAGE, true).unwrap();
        a.take(p, p + PAGE);
        b.take(p, p + PAGE);
        for value in 1..=u32::from(HOT_THRESHOLD) + 1 {
            // SAFETY: aligned word in our live pool.
            unsafe { (p as *mut u32).write_volatile(value) };
            a.take(p, p + PAGE);
        }
        assert!(guards().watched.contains(&(p / PAGE)));
        let writer = writing(p, PAGE);
        a.take(p, p + PAGE);
        b.take(p, p + PAGE);
        for _ in 0..COOL_TAKES {
            a.take(p, p + 4);
        }
        let end_word = p + PAGE - 4;
        // SAFETY: last aligned word in the page held open by the writer.
        unsafe { (end_word as *mut u32).write_volatile(99) };
        for t in [&a, &b] {
            assert_eq!(t.take(end_word, p + PAGE), [(end_word, p + PAGE)]);
        }
        drop(writer);
        for t in [&a, &b] {
            assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
            assert!(t.take(p, p + PAGE).is_empty());
        }
        drop((a, b));
        free(p);
    }

    /// Concurrent `arm()` racing the `HOT_THRESHOLD` promotion `take()` must
    /// serialize under `GUARDS` so the armed page is never left read-write
    /// without a guard and always captures its pre-write snapshot.
    #[test]
    fn arming_during_hot_promotion_keeps_before_image() {
        use std::sync::Barrier;
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        for _ in 0..32 {
            let p = pool(1);
            let t = track_copy(p, PAGE, true).expect("tracked write-watched pool");
            assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
            for k in 1..u32::from(HOT_THRESHOLD) {
                // SAFETY: owned pool page, with the normal write-fault handler.
                unsafe { (p as *mut u32).write_volatile(k) };
                assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
            }
            // Write the pre-submission value; the next `take()` would promote
            // the page if `arm()` had not serialized against it.
            // SAFETY: owned pool page.
            unsafe { (p as *mut u32).write_volatile(0xAAAA_5555) };
            let barrier = Barrier::new(2);
            std::thread::scope(|scope| {
                let arm = scope.spawn(|| {
                    barrier.wait();
                    t.arm(p, p + PAGE);
                });
                barrier.wait();
                assert_eq!(t.take(p, p + PAGE), [(p, p + PAGE)]);
                arm.join().unwrap();
            });
            assert!(!guards().watched.contains(&(p / PAGE)));
            assert_eq!(page_protection(p), PAGE_READONLY);
            // SAFETY: guarded page of our pool; handler opens it and saves old bytes.
            unsafe { (p as *mut u32).write_volatile(0xDEAD_BEEF) };
            let before = t
                .saved(p / PAGE)
                .map(|bytes| u32::from_le_bytes(bytes[..4].try_into().unwrap()));
            t.disarm();
            drop(t);
            free(p);
            assert_eq!(before, Some(0xAAAA_5555), "arming lost its pre-write snapshot");
        }
    }
}
