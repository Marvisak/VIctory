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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The unit tracked and copied: 64 KiB, the console's page. Finer units
/// made the checks of the world's big buffers cost more than the copies
/// they saved.
pub(crate) const PAGE: usize = 64 << 10;

/// The pages a tracker has not taken since they changed, what it saves of
/// pages it armed, its bit in [`DIRTY`] (none: always taken as dirty), and
/// whether it is a host buffer copy's (the only trackers [`retake`] marks).
struct Changed(Mutex<BTreeSet<usize>>, Mutex<Saved>, Option<u32>, bool);

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
    fn taken(&self, page: usize) -> Option<Box<[u8]>> {
        self.1
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bytes
            .remove(&page)
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
    /// Pages noted guarded (in `protected`) that a take is making read-only
    /// outside this lock ([`Arming`], `VICTORY_GUARD_OUTSIDE`).
    arming: BTreeSet<usize>,
    /// Of those, the ones opened meanwhile: made writable again once the
    /// take has changed their protection.
    disturbed: BTreeSet<usize>,
    /// Tracked pages written this frame and the frame before (diagnostics).
    #[cfg(feature = "diagnostics")]
    written: BTreeSet<usize>,
    #[cfg(feature = "diagnostics")]
    written_before: BTreeSet<usize>,
}

static GUARDS: Mutex<Guards> = Mutex::new(Guards {
    protected: BTreeSet::new(),
    holders: BTreeMap::new(),
    busy: Vec::new(),
    alias_of: BTreeMap::new(),
    arming: BTreeSet::new(),
    disturbed: BTreeSet::new(),
    #[cfg(feature = "diagnostics")]
    written: BTreeSet::new(),
    #[cfg(feature = "diagnostics")]
    written_before: BTreeSet::new(),
});

#[cfg(not(feature = "diagnostics"))]
fn guards() -> std::sync::MutexGuard<'static, Guards> {
    #[cfg(test)]
    if let Err(std::sync::TryLockError::WouldBlock) = GUARDS.try_lock() {
        // Timed for the benchmark when it is taken.
        let started = std::time::Instant::now();
        let held = GUARDS.lock().unwrap_or_else(|e| e.into_inner());
        tests::WAITED.set(tests::WAITED.get() + started.elapsed().as_micros() as u64);
        return held;
    }
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

/// The guard lock as [`guards`] hands it out.
#[cfg(not(feature = "diagnostics"))]
type GuardsHeld = std::sync::MutexGuard<'static, Guards>;
#[cfg(feature = "diagnostics")]
type GuardsHeld = Held;

/// Signalled when a take has changed the protection of the pages it was
/// making read-only outside the guard lock ([`Arming::finish`]).
static ARMED: std::sync::Condvar = std::sync::Condvar::new();

/// Waits, the guard lock let go meanwhile, until none of `pages` is being
/// made read-only outside it by another take ([`Arming`]). Read now and
/// taken for guarded, such a page could take a write that lands before its
/// guard stands, after this taker's copy: in nobody's copy. Only the pages
/// themselves: pages showing memory elsewhere too are guarded under the
/// lock, with their other mappings ([`protect_runs`]).
fn wait_armed(mut g: GuardsHeld, pages: &[usize]) -> GuardsHeld {
    while !g.arming.is_empty() && pages.iter().any(|p| g.arming.contains(p)) {
        #[cfg(not(feature = "diagnostics"))]
        {
            g = ARMED.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        #[cfg(feature = "diagnostics")]
        {
            // The wait does not count as held.
            let held = g.1.elapsed();
            let inner = g.0.take().expect("held until dropped");
            g.0 = Some(ARMED.wait(inner).unwrap_or_else(|e| e.into_inner()));
            g.1 = std::time::Instant::now().checked_sub(held).unwrap_or(g.1);
        }
    }
    g
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
    let changed = Arc::new(Changed(Mutex::new(pages.iter().copied().collect()), Mutex::default(), bit, copy));
    let mut g = guards();
    for &page in &pages {
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
            if let Some(holders) = g.holders.get_mut(&page) {
                holders.retain(|c| !Arc::ptr_eq(c, &self.changed));
                if holders.is_empty() {
                    g.holders.remove(&page);
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
    if g.arming.contains(&page) {
        // Its take may make it read-only after this: writable again then
        // ([`Arming::finish`]).
        g.disturbed.insert(page);
    }
    let mut old = 0;
    // SAFETY: a block we made read-only (failing harmlessly if it is gone).
    unsafe { VirtualProtect((page * PAGE) as *const _, PAGE, PAGE_READWRITE, &mut old) };
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::OpenPages, 1);
    true
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
        let pages: Vec<usize> = {
            #[cfg(feature = "diagnostics")]
            let waited = Timed(std::time::Instant::now(), crate::copystat::C::GuardWaitUs);
            let changed = self.changed.0.lock().unwrap_or_else(|e| e.into_inner());
            #[cfg(feature = "diagnostics")]
            drop(waited);
            changed.range(from / PAGE..to.div_ceil(PAGE)).copied().collect()
        };
        if pages.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let mut whole: Vec<usize> = pages
            .iter()
            .copied()
            .filter(|&p| (p * PAGE).max(self.start) >= from && ((p + 1) * PAGE).min(self.end) <= to)
            .collect();
        // Every address showing the same pages is guarded too. A write in
        // between marks them changed again; the copy, made after, has it.
        #[cfg(feature = "diagnostics")]
        let waited = Timed(std::time::Instant::now(), crate::copystat::C::GuardWaitUs);
        let later = guard_outside();
        let mut g = wait_armed(guards(), &whole);
        let mut changed = self.changed.0.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "diagnostics")]
        drop(waited);
        #[cfg(feature = "diagnostics")]
        let asked = Timed(std::time::Instant::now(), crate::copystat::C::AliasUs);
        let others = aliases(&g, &whole);
        let unguarded = guard_aliases(&mut g, &others);
        #[cfg(feature = "diagnostics")]
        drop(asked);
        whole.retain(|p| !unguarded.contains(p));
        let mut unmapped = Vec::new();
        let mut arming = Arming::new(&self.changed);
        let runs = later.then_some(&mut arming.runs);
        for page in protect_runs(&mut g, &whole, &mut unmapped, runs) {
            changed.remove(&page);
            arming.taken(page);
        }
        set_dirty(self.changed.2, !changed.is_empty());
        drop(changed);
        drop(g);
        // Read-only before the caller copies the pages.
        unmapped.extend(arming.finish());
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
        self.changed
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&page)
    }

    /// The caller made pages (page numbers) of its copy equal to title
    /// memory itself (what shaders wrote went back): taken and guarded.
    pub(crate) fn taken(&self, pages: &[usize]) {
        let later = guard_outside();
        let mut g = wait_armed(guards(), pages);
        let mut changed = self.changed.0.lock().unwrap_or_else(|e| e.into_inner());
        let others = aliases(&g, pages);
        let unguarded = guard_aliases(&mut g, &others);
        let pages: Vec<usize> = pages
            .iter()
            .copied()
            .filter(|p| !unguarded.contains(p))
            .collect();
        let mut arming = Arming::new(&self.changed);
        let runs = later.then_some(&mut arming.runs);
        for page in protect_runs(&mut g, &pages, &mut Vec::new(), runs) {
            changed.remove(&page);
            arming.taken(page);
        }
        set_dirty(self.changed.2, !changed.is_empty());
        drop(changed);
        drop(g);
        arming.finish();
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
        if from < to {
            self.changed.1.lock().unwrap_or_else(|e| e.into_inner()).arm(from / PAGE, to.div_ceil(PAGE));
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
    protect_noting(g, pages, &mut Vec::new())
}

/// [`protect`], also listing in `unmapped` the pages taken because nothing
/// is mapped there.
fn protect_noting(g: &mut Guards, pages: &[usize], unmapped_pages: &mut Vec<usize>) -> Vec<usize> {
    protect_runs(g, pages, unmapped_pages, None)
}

/// [`protect_noting`]; with `later` (`VICTORY_GUARD_OUTSIDE`), runs of
/// pages no other mapping shows are only noted guarded here, and listed in
/// `later` for [`Arming::finish`] to make read-only once the caller has let
/// go of the lock. Pages showing memory elsewhere too stay guarded under
/// it, as their other mappings are ([`guard_aliases`]): a take through the
/// other mapping looks only at its own pages ([`wait_armed`]).
fn protect_runs(
    g: &mut Guards,
    pages: &[usize],
    unmapped_pages: &mut Vec<usize>,
    mut later: Option<&mut Vec<(usize, usize)>>,
) -> Vec<usize> {
    #[cfg(feature = "diagnostics")]
    let counted = |calls: u64, pages: u64| {
        crate::copystat::add(crate::copystat::C::ProtectCalls, calls);
        crate::copystat::add(crate::copystat::C::ProtectPages, pages);
    };
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, VirtualProtect};
    #[cfg(feature = "diagnostics")]
    let _timed = Timed(std::time::Instant::now(), crate::copystat::C::ProtectUs);
    let set = |from: usize, to: usize| {
        let mut old = 0;
        // SAFETY: title memory; only its protection changes.
        unsafe { VirtualProtect((from * PAGE) as *const _, (to - from) * PAGE, PAGE_READONLY, &mut old) != 0 }
    };
    let mut taken = Vec::with_capacity(pages.len());
    let busy = |page: usize| g.busy.iter().any(|&(a, b)| (a..b).contains(&page));
    let pages: Vec<usize> = pages.iter().copied().filter(|&p| !busy(p)).collect();
    let mut i = 0;
    while i < pages.len() {
        if g.protected.contains(&pages[i]) {
            taken.push(pages[i]);
            i += 1;
            continue;
        }
        // A run of consecutive pages not guarded yet.
        let first = i;
        while i + 1 < pages.len() && pages[i + 1] == pages[i] + 1 && !g.protected.contains(&pages[i + 1]) {
            i += 1;
        }
        let run = &pages[first..=i];
        i += 1;
        #[cfg(feature = "diagnostics")]
        counted(1, run.len() as u64);
        if let Some(later) = later.as_deref_mut() {
            let (from, to) = (run[0], run[run.len() - 1] + 1);
            let alone = g.alias_of.range(from..to).next().is_none();
            GUARD_LATER[usize::from(!alone)].fetch_add(1, Ordering::Relaxed);
            if alone {
                g.protected.extend(run);
                g.arming.extend(run);
                taken.extend(run);
                later.push((from, to));
                continue;
            }
        }
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

/// Whether nothing is mapped at page `page` (a page number).
fn unmapped(page: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((page * PAGE) as *const _, &mut info, size_of_val(&info)) };
    n == 0 || info.State != MEM_COMMIT
}

/// Since the start, with `VICTORY_GUARD_OUTSIDE`: runs of pages made
/// read-only outside the guard lock (0), and runs kept under it because
/// another mapping shows their memory (1).
pub(crate) static GUARD_LATER: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether a take makes the pages it takes read-only after letting go of
/// the guard lock (`VICTORY_GUARD_OUTSIDE=1`; off by default: under it, as
/// before).
///
/// Why (#3425): making pages read-only again is most of the time a take
/// holds the guard lock, and the title's threads wait for that lock at
/// every write fault (49-76 ms a frame on the reports there). Measured by
/// `tests::guard_contention`: there the writers' wait for the lock behind
/// takes goes, but not their time in the fault, which then goes to the
/// kernel's own changes of protection (most likely serialised there, as
/// the take's and the fault's both change this process's mappings), and a
/// take holds the lock twice. Off until a title shows a
/// gain. What a take answers does not change ([`Arming`]).
fn guard_outside() -> bool {
    #[cfg(test)]
    if let Some(on) = tests::OUTSIDE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_GUARD_OUTSIDE"))
}

/// Pages a take noted guarded, in `Guards::protected` and
/// `Guards::arming`, whose protection it has yet to change (runs
/// `[first, last)`), with the bytes their tracker had saved of them.
///
/// Why what the take answers stays what it was. Until [`Arming::finish`]
/// has made a page read-only, a write to it lands with no fault: the
/// take's own copy, made after `finish`, holds it; no other take reads the
/// page meanwhile (they wait, [`wait_armed`]); and no other mapping shows
/// it ([`protect_runs`]), so no tracker of another mapping takes it for
/// guarded. Whatever opens the page meanwhile ([`open`]: a fault once it is
/// read-only, [`writing`], a remapping) marks it changed as before and
/// notes it in `Guards::disturbed`; `finish` makes it writable again after
/// its own change.
#[must_use]
struct Arming {
    runs: Vec<(usize, usize)>,
    changed: Arc<Changed>,
    saved: Vec<(usize, Box<[u8]>)>,
}

impl Arming {
    fn new(changed: &Arc<Changed>) -> Arming {
        Arming {
            runs: Vec::new(),
            changed: changed.clone(),
            saved: Vec::new(),
        }
    }

    /// Page `page` was taken: what its tracker saved of it is stale, kept
    /// here only in case the page cannot be made read-only (and so stays
    /// changed with it, as under the lock).
    fn taken(&mut self, page: usize) {
        if let Some(bytes) = self.changed.taken(page)
            && self.runs.iter().any(|&(a, b)| (a..b).contains(&page))
        {
            self.saved.push((page, bytes));
        }
    }

    /// Makes the pages read-only and takes them out of `Guards::arming`;
    /// returns those taken because nothing is mapped there. Called with
    /// neither the guard lock nor the tracker's held.
    fn finish(mut self) -> Vec<usize> {
        self.arm()
    }

    fn arm(&mut self) -> Vec<usize> {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
        let runs = std::mem::take(&mut self.runs);
        if runs.is_empty() {
            return Vec::new();
        }
        #[cfg(test)]
        tests::pause();
        #[cfg(feature = "diagnostics")]
        let timed = Timed(std::time::Instant::now(), crate::copystat::C::ProtectUs);
        let set = |from: usize, to: usize| {
            let mut old = 0;
            // SAFETY: title memory; only its protection changes.
            unsafe {
                VirtualProtect(
                    (from * PAGE) as *const _,
                    (to - from) * PAGE,
                    PAGE_READONLY,
                    &mut old,
                ) != 0
            }
        };
        // Pages that could not be made read-only: (page, nothing mapped).
        let mut failed = Vec::new();
        for &(from, to) in &runs {
            if set(from, to) {
                continue;
            }
            for page in from..to {
                if !set(page, page + 1) {
                    failed.push((page, unmapped(page)));
                }
            }
        }
        #[cfg(feature = "diagnostics")]
        drop(timed);
        let mut g = guards();
        for &(from, to) in &runs {
            for page in from..to {
                g.arming.remove(&page);
                if g.disturbed.remove(&page) {
                    let mut old = 0;
                    // SAFETY: a page of ours opened while it was being made
                    // read-only: writable, as the opening left it.
                    unsafe {
                        VirtualProtect((page * PAGE) as *const _, PAGE, PAGE_READWRITE, &mut old)
                    };
                }
            }
        }
        let mut unmapped_pages = Vec::new();
        if !failed.is_empty() {
            // As under the lock: not guarded; taken as zeros if nothing is
            // mapped, else still changed, with what was saved of it.
            let mut changed = self.changed.0.lock().unwrap_or_else(|e| e.into_inner());
            let mut saved = self.changed.1.lock().unwrap_or_else(|e| e.into_inner());
            for &(page, nothing) in &failed {
                g.protected.remove(&page);
                if nothing {
                    unmapped_pages.push(page);
                    continue;
                }
                changed.insert(page);
                set_dirty(self.changed.2, true);
                if let Some(i) = self.saved.iter().position(|&(p, _)| p == page) {
                    let (_, bytes) = self.saved.swap_remove(i);
                    saved.bytes.entry(page).or_insert(bytes);
                }
            }
        }
        drop(g);
        ARMED.notify_all();
        unmapped_pages
    }
}

impl Drop for Arming {
    /// Not finished (a panic on the way): made read-only all the same, or
    /// every later take of the pages would wait for good.
    fn drop(&mut self) {
        self.arm();
    }
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
    if !g.arming.is_empty() {
        // Linked now, a page being made read-only outside the lock would
        // be taken for guarded through the new mapping ([`protect_runs`]).
        let pages: Vec<usize> = others.iter().map(|&o| o / PAGE).chain([page]).collect();
        g = wait_armed(g, &pages);
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
        if changed(&mut g, at / PAGE, at / PAGE + 1, true) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if g.disturbed.contains(&(at / PAGE)) {
            // Opened (and marked) while a take was making it read-only
            // outside the lock, which it then did: writable, as the
            // opening left it ([`Arming`]).
            use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
            let mut old = 0;
            // SAFETY: a page of ours; only its protection changes.
            unsafe {
                VirtualProtect(
                    (at & !(PAGE - 1)) as *const _,
                    PAGE,
                    PAGE_READWRITE,
                    &mut old,
                )
            };
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

    thread_local! {
        /// [`guard_outside`] for takes on this thread, when set.
        pub(crate) static OUTSIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
        /// Microseconds a take on this thread spins before making the pages
        /// it noted read-only outside the lock: the window a take preempted
        /// there leaves, made wide enough for a test to meet.
        static PAUSE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        /// Microseconds this thread waited for the guard lock (builds
        /// without diagnostics).
        pub(crate) static WAITED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Takes in their [`PAUSE`] now.
    static PAUSING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// [`PAUSE`] on this thread.
    pub(crate) fn pause() {
        let us = PAUSE.get();
        if us == 0 {
            return;
        }
        PAUSING.fetch_add(1, Ordering::SeqCst);
        let started = std::time::Instant::now();
        while (started.elapsed().as_micros() as u64) < us {
            std::hint::spin_loop();
        }
        PAUSING.fetch_sub(1, Ordering::SeqCst);
    }

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

    /// A host copy of tracked memory as `crate::shadow` keeps one: what
    /// [`Tracked::take`] answers, read from title memory into `side`.
    struct Copy {
        tracked: Tracked,
        start: usize,
        side: Vec<u64>,
    }

    impl Copy {
        fn new(start: usize, pages: usize) -> Copy {
            Copy {
                tracked: track(start, pages * PAGE, true).expect("tracked"),
                start,
                side: vec![0; pages * PAGE / 8],
            }
        }

        fn end(&self) -> usize {
            self.start + self.side.len() * 8
        }

        /// Takes what changed and copies it; the bytes copied.
        fn refresh(&mut self) -> usize {
            let mut bytes = 0;
            for (a, b) in self.tracked.take(self.start, self.end()) {
                for at in (a..b).step_by(8) {
                    // SAFETY: tracked title memory of the test, mapped;
                    // read-only or not, it reads. Written by other threads
                    // meanwhile: a word at a time, as they write.
                    self.side[(at - self.start) / 8] =
                        unsafe { (at as *const u64).read_volatile() };
                }
                bytes += b - a;
            }
            bytes
        }

        /// The word at title address `at` in the copy, if it covers it.
        fn word(&self, at: usize) -> Option<u64> {
            (self.start..self.end())
                .contains(&at)
                .then(|| self.side[(at - self.start) / 8])
        }
    }

    /// Refreshes `copies`, each on its own thread with
    /// `VICTORY_GUARD_OUTSIDE` as `outside`, all starting together.
    fn refresh_together(copies: &mut [Copy], outside: bool) {
        let start = std::sync::Barrier::new(copies.len());
        std::thread::scope(|s| {
            for copy in copies.iter_mut() {
                let start = &start;
                s.spawn(move || {
                    OUTSIDE.set(Some(outside));
                    PAUSE.set(if outside { 1000 } else { 0 });
                    start.wait();
                    copy.refresh();
                });
            }
        });
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Sets its flag when dropped (the writers stop however a test ends).
    struct Stop<'a>(&'a std::sync::atomic::AtomicBool);

    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// With `VICTORY_GUARD_OUTSIDE`, copies refreshed together while the
    /// title's threads write lose no write: every word written before a
    /// refresh began is in every copy covering it once it has returned, and
    /// the quiet refresh after it leaves every copy equal to title memory.
    /// Two copies of a pool's pages, refreshed together, catch a take that
    /// reads pages another take has yet to make read-only (with no wait in
    /// [`wait_armed`]); copies of the two mappings of the same pages catch
    /// such pages made read-only outside the lock (with no look at
    /// `alias_of` in [`protect_runs`]). Off, the same checks hold as before.
    #[test]
    fn refreshes_under_writes_lose_nothing() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Acquire, Ordering::Release};
        const POOL: usize = 24;
        const TWIN: usize = 24;
        const WRITERS: usize = 4;
        const SLOTS: usize = 96;
        let p = pool(POOL);
        let (a, b) = twin_views(TWIN);
        // Slot `k`: a word on page `k % (POOL + TWIN)` (the pool's, then
        // the twin pages'), at an offset of its own. Its address as the
        // copies of the first mapping see it, and the second mapping's.
        let slot = |k: usize| {
            let (page, offset) = (k % (POOL + TWIN), 8 * (k / (POOL + TWIN)) + 64);
            if page < POOL {
                (p + page * PAGE + offset, None)
            } else {
                let at = (page - POOL) * PAGE + offset;
                (a + at, Some(b + at))
            }
        };
        // The latest number each writer stored, and each slot's (across
        // both runs: the memory keeps what the first wrote).
        let latest: Vec<AtomicU64> = (0..WRITERS).map(|_| AtomicU64::new(0)).collect();
        let written: Vec<AtomicU64> = (0..WRITERS * SLOTS).map(|_| AtomicU64::new(0)).collect();
        for outside in [true, false] {
            let later = GUARD_LATER.each_ref().map(|c| c.load(Ordering::Relaxed));
            let mut copies = vec![
                Copy::new(p, POOL),
                Copy::new(p + 4 * PAGE, POOL - 4),
                Copy::new(a, TWIN),
                Copy::new(b, TWIN),
                Copy::new(b + 8 * PAGE, TWIN - 8),
            ];
            refresh_together(&mut copies, outside);
            for round in 0..120 {
                let stop = AtomicBool::new(false);
                // The writers write a while, and on through the refresh.
                let begun = std::sync::Barrier::new(WRITERS + 1);
                let (floor, ceiling) = std::thread::scope(|s| {
                    for w in 0..WRITERS {
                        let (stop, begun, latest, written) = (&stop, &begun, &latest[w], &written);
                        s.spawn(move || {
                            let mut rng = Rng(0x5eed + (w * 1000 + round) as u64);
                            let mut n = latest.load(Ordering::Relaxed);
                            for i in 0.. {
                                if i == 64 {
                                    begun.wait();
                                }
                                if i > 64 && stop.load(Ordering::Relaxed) {
                                    break;
                                }
                                // Outside the lock, during the refresh: only
                                // while a take has yet to make its pages
                                // read-only, so that a write lost there is
                                // not found again by a later fault.
                                if i > 64 && outside && PAUSING.load(Ordering::SeqCst) == 0 {
                                    std::hint::spin_loop();
                                    continue;
                                }
                                n += 1;
                                let k = w * SLOTS + rng.next() as usize % SLOTS;
                                let (at, other) = slot(k);
                                let at = match other {
                                    Some(other) if rng.next().is_multiple_of(2) => other,
                                    _ => at,
                                };
                                latest.store(n, Release);
                                // SAFETY: a word of the test's title memory;
                                // a guarded page faults into the handler.
                                unsafe { (at as *mut u64).write_volatile(n) };
                                // After the store: a refresh that reads this
                                // number before it begins finds it stored.
                                written[k].store(n, Release);
                            }
                        });
                    }
                    let _stop = Stop(&stop);
                    begun.wait();
                    let floor: Vec<u64> = written.iter().map(|v| v.load(Acquire)).collect();
                    refresh_together(&mut copies, outside);
                    let ceiling: Vec<u64> = latest.iter().map(|v| v.load(Acquire)).collect();
                    (floor, ceiling)
                });
                for (c, copy) in copies.iter().enumerate() {
                    for (k, &floor) in floor.iter().enumerate() {
                        let (at, other) = slot(k);
                        let Some(value) = copy.word(at).or_else(|| copy.word(other?)) else {
                            continue;
                        };
                        assert!(
                            value >= floor,
                            "outside {outside}, round {round}: copy {c} lost a write at {at:#x}: holds {value}, {floor} was written before the refresh"
                        );
                        assert!(
                            value <= ceiling[k / SLOTS],
                            "outside {outside}: copy {c} holds {value} at {at:#x}, never written"
                        );
                    }
                }
                // The writers are done: a refresh now finds every write.
                refresh_together(&mut copies, outside);
                for (c, copy) in copies.iter().enumerate() {
                    // SAFETY: the copy's title memory, mapped.
                    let now = unsafe {
                        std::slice::from_raw_parts(copy.start as *const u64, copy.side.len())
                    };
                    if let Some(i) = (0..now.len()).find(|&i| copy.side[i] != now[i]) {
                        panic!(
                            "outside {outside}, round {round}: copy {c} holds {} at {:#x} after a quiet refresh, title memory {}",
                            copy.side[i],
                            copy.start + 8 * i,
                            now[i]
                        );
                    }
                }
            }
            let [apart, kept] = [0, 1].map(|i| GUARD_LATER[i].load(Ordering::Relaxed) - later[i]);
            if outside {
                assert!(apart > 0, "no run made read-only outside the lock");
                assert!(kept > 0, "no run of linked pages kept under the lock");
            } else {
                assert_eq!((apart, kept), (0, 0), "off: everything under the lock");
            }
            drop(copies);
        }
        free(p);
    }

    /// A page opened while its take is making it read-only outside the
    /// lock ends writable, and changed; one with nothing mapped is taken
    /// apart, and not noted guarded.
    #[test]
    fn armed_pages_opened_or_unmapped_end_as_under_the_lock() {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        let protection = |at: usize| {
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
            info.Protect
        };
        let p = pool(2);
        let t = track(p, 2 * PAGE, true).expect("tracked");
        // Noted guarded under the lock, opened by a write the guards do
        // not see before the protection changes, then armed.
        let mut g = guards();
        let mut arming = Arming::new(&t.changed);
        let taken = protect_runs(
            &mut g,
            &[p / PAGE, p / PAGE + 1],
            &mut Vec::new(),
            Some(&mut arming.runs),
        );
        assert_eq!(taken, [p / PAGE, p / PAGE + 1]);
        let mut changed = t.changed.0.lock().unwrap();
        changed.clear();
        drop(changed);
        drop(g);
        drop(writing(p + PAGE + 8, 8));
        assert!(arming.finish().is_empty());
        assert_eq!(
            protection(p),
            windows_sys::Win32::System::Memory::PAGE_READONLY
        );
        assert_eq!(
            protection(p + PAGE),
            PAGE_READWRITE,
            "opened meanwhile: writable"
        );
        assert!(!t.changed(p / PAGE) && t.changed(p / PAGE + 1));
        let g = guards();
        assert!(g.arming.is_empty() && g.disturbed.is_empty());
        drop(g);
        drop(t);
        free(p);
        // A run over a page with nothing mapped: that page is taken as
        // zeros and not noted guarded, as under the lock.
        // SAFETY: a fresh write-watched reservation of two pages, the first
        // committed; freed below.
        let r = unsafe {
            let r = VirtualAlloc(
                std::ptr::null(),
                2 * PAGE,
                MEM_RESERVE | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            ) as usize;
            assert_ne!(r, 0);
            assert_ne!(
                VirtualAlloc(r as *const _, PAGE, MEM_COMMIT, PAGE_READWRITE) as usize,
                0
            );
            r
        };
        let t = track(r, 2 * PAGE, true).expect("tracked");
        OUTSIDE.set(Some(true));
        assert_eq!(
            t.take_split(r, r + 2 * PAGE),
            (vec![(r, r + PAGE)], vec![(r + PAGE, r + 2 * PAGE)])
        );
        OUTSIDE.set(None);
        assert!(t.take(r, r + 2 * PAGE).is_empty(), "both taken");
        let g = guards();
        assert!(g.protected.contains(&(r / PAGE)) && !g.protected.contains(&(r / PAGE + 1)));
        drop(g);
        drop(t);
        free(r);
    }
    fn env(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    /// The contention benchmark (`cargo test -p victory-gpu --release --lib
    /// guard_contention -- --ignored --nocapture`): host copies of a
    /// write-watched pool refreshed while the title's threads write it,
    /// `VICTORY_GUARD_OUTSIDE` off and on in turn. What a refresh takes,
    /// and what the writers spend in their writes (the write-fault handler
    /// and its wait for the guard lock). Sizes by the environment:
    /// `BENCH_PAGES` (title memory in 64 KiB pages, 4,096 = 256 MB),
    /// `BENCH_SPAN` (pages a copy, 32), `BENCH_DIRTY` (pages written before
    /// each refresh, 1,400, about 89 MB), `BENCH_WRITERS` (title threads,
    /// 4), `BENCH_GAP_US` (a writer's pause between writes during the
    /// refresh, 20), `BENCH_ROUNDS` (11), `BENCH_WIDE` (1: the copies
    /// refreshed on 4 threads), `BENCH_ALIASED` (1: the memory shown at two
    /// mappings, linked as the runtime links them: its runs stay under the
    /// lock).
    #[test]
    #[ignore = "benchmark: run by hand on a quiet machine"]
    fn guard_contention() {
        use std::sync::atomic::AtomicBool;
        let pages = env("BENCH_PAGES", 4096);
        let span = env("BENCH_SPAN", 32);
        let dirty = env("BENCH_DIRTY", 1400).min(pages);
        let writers = env("BENCH_WRITERS", 4);
        let gap = env("BENCH_GAP_US", 20) as u64;
        let rounds = env("BENCH_ROUNDS", 11);
        let wide = env("BENCH_WIDE", 0) == 1;
        let aliased = env("BENCH_ALIASED", 0) == 1;
        let at = if aliased {
            twin_views(pages).0
        } else {
            pool(pages)
        };
        let len = pages * PAGE;
        let mut copies: Vec<Copy> = (0..pages)
            .step_by(span)
            .map(|first| Copy::new(at + first * PAGE, span.min(pages - first)))
            .collect();
        // The bytes copied, and the microseconds waited for the guard lock.
        let refresh = |copies: &mut [Copy], outside: bool| -> (usize, u64) {
            let per = copies.len().div_ceil(if wide { 4 } else { 1 });
            std::thread::scope(|s| {
                let handles: Vec<_> = copies
                    .chunks_mut(per)
                    .map(|chunk| {
                        s.spawn(move || {
                            OUTSIDE.set(Some(outside));
                            let bytes = chunk.iter_mut().map(Copy::refresh).sum::<usize>();
                            (bytes, WAITED.get())
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .fold((0, 0), |(b, w), (b2, w2)| (b + b2, w + w2))
            })
        };
        refresh(&mut copies, false);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        // Per mode (off, on): refresh ms, ms the writers spent in their
        // writes, writes over 5 us, their mean us, the longest us, MB
        // copied, ms the writers and the refresh waited for the guard lock.
        let mut rows: [Vec<[f64; 8]>; 2] = [Vec::new(), Vec::new()];
        let later = GUARD_LATER.each_ref().map(|c| c.load(Ordering::Relaxed));
        for round in 0..rounds {
            let order = if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for outside in order {
                // The frame before: `dirty` distinct pages written.
                let mut all: Vec<usize> = (0..pages).collect();
                for i in (1..all.len()).rev() {
                    all.swap(i, rng.next() as usize % (i + 1));
                }
                std::thread::scope(|s| {
                    for part in all[..dirty].chunks(dirty.div_ceil(writers).max(1)) {
                        s.spawn(move || {
                            for &page in part {
                                let word = at + page * PAGE + 8 * (page % 1000);
                                // SAFETY: a word of the pool; guarded, the
                                // handler opens it.
                                unsafe { (word as *mut u64).write_volatile(round as u64) };
                            }
                        });
                    }
                });
                // The refresh, with the writers writing on.
                let stop = AtomicBool::new(false);
                let (ms, bytes, spent) = std::thread::scope(|s| {
                    let handles: Vec<_> = (0..writers)
                        .map(|k| {
                            let stop = &stop;
                            s.spawn(move || {
                                let mut rng = Rng(0x1234_5678 + (k * 77 + round) as u64);
                                let (mut total, mut slow, mut slow_us, mut longest) =
                                    (0f64, 0u64, 0f64, 0f64);
                                while !stop.load(Ordering::Relaxed) {
                                    let word = at + rng.next() as usize % (len / 8) * 8;
                                    let started = std::time::Instant::now();
                                    // SAFETY: a word of the pool; guarded,
                                    // the handler opens it.
                                    unsafe { (word as *mut u64).write_volatile(round as u64) };
                                    let us = started.elapsed().as_secs_f64() * 1e6;
                                    total += us;
                                    if us > 5.0 {
                                        slow += 1;
                                        slow_us += us;
                                    }
                                    longest = longest.max(us);
                                    let pause = std::time::Instant::now();
                                    while (pause.elapsed().as_micros() as u64) < gap {
                                        std::hint::spin_loop();
                                    }
                                }
                                (total, slow, slow_us, longest, WAITED.get())
                            })
                        })
                        .collect();
                    let _stop = Stop(&stop);
                    let started = std::time::Instant::now();
                    let bytes = refresh(&mut copies, outside);
                    let ms = started.elapsed().as_secs_f64() * 1e3;
                    stop.store(true, Ordering::Relaxed);
                    let spent: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
                    (ms, bytes, spent)
                });
                let (bytes, refresh_waited) = bytes;
                let writers_waited: u64 = spent.iter().map(|s| s.4).sum();
                let total: f64 = spent.iter().map(|s| s.0).sum();
                let slow: u64 = spent.iter().map(|s| s.1).sum();
                let slow_us: f64 = spent.iter().map(|s| s.2).sum();
                let longest = spent.iter().map(|s| s.3).fold(0f64, f64::max);
                rows[usize::from(outside)].push([
                    ms,
                    total / 1e3,
                    slow as f64,
                    if slow > 0 { slow_us / slow as f64 } else { 0.0 },
                    longest,
                    bytes as f64 / 1e6,
                    writers_waited as f64 / 1e3,
                    refresh_waited as f64 / 1e3,
                ]);
            }
        }
        println!(
            "{pages} pages, copies of {span}, {dirty} dirty, {writers} writers every {gap} us, wide {wide}, aliased {aliased}, {rounds} rounds (medians)"
        );
        for (outside, rows) in rows.iter().enumerate() {
            let m = |i: usize| median(rows.iter().map(|r| r[i]).collect());
            println!(
                "VICTORY_GUARD_OUTSIDE={outside}: refresh {:.2} ms ({:.1} MB), writers in their writes {:.2} ms (waiting for the guard lock {:.2} ms), {:.0} writes over 5 us at {:.1} us each, longest {:.0} us; the refresh waited {:.2} ms",
                m(0),
                m(5),
                m(1),
                m(6),
                m(2),
                m(3),
                m(4),
                m(7)
            );
        }
        let [apart, kept] = [0, 1].map(|i| GUARD_LATER[i].load(Ordering::Relaxed) - later[i]);
        println!("runs made read-only outside the lock {apart}, kept under it {kept}");
    }
}
