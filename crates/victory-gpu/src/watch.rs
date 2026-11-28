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
//! With `VICTORY_OWNER=1` a whole page a copy's shaders wrote need not go
//! back at the submission's end: most such pages the submissions after it
//! write again before anything reads them (a census of the pages one tree
//! wrote back: 87 % of them), and their copy-back is work thrown away. The page keeps its bytes
//! in the copy and title memory keeps the older ones behind no access at
//! all ([`lazify`]); the first touch gives it the copy's bytes through the
//! runtime's own writable mapping of its section
//! ([`set_writable_alias`]) while every title mapping still has no access,
//! so no reader ever sees the old bytes. Who touches title memory, and how
//! each gives a lazy page its bytes first: in-process reads and writes
//! (the title, the runtime, our host's direct loads and stores) fault into
//! the handler, which resolves the page before the access retries; our
//! host's kernel reads ([`serve`]); kernel writes and the runtime mapping
//! or unmapping memory ([`writing`], [`title_memory_remapped`]); the copy
//! going (`crate::shadow`'s drop); what it wrote being withheld
//! (`crate::shadow::Shadow::withhold`: the bytes kept at arming go back, or
//! the older valid ones, and the withheld ones are dropped). A page its copy's
//! shaders mark again before any touch is never written in between
//! ([`again`]: the copy holds the newest bytes).
//!
//! shortcut: a system call writing into a guarded block fails; the known
//! ones (file reads, DirectStorage) open their blocks first. Memory freed
//! and allocated again while a tracker holds it is not seen.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The unit tracked and copied: 64 KiB, the console's page. Finer units
/// made the checks of the world's big buffers cost more than the copies
/// they saved.
pub(crate) const PAGE: usize = 64 << 10;

/// The pages a tracker has not taken since they changed, what it saves of
/// pages it armed, its bit in [`DIRTY`] (none: always taken as dirty), and
/// whether it is a host buffer copy's (the only trackers [`retake`] marks).
pub(crate) struct Changed(Mutex<BTreeSet<usize>>, Mutex<Saved>, Option<u32>, bool);

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
    /// Whether page `page` (a page number) changed since the tracker took
    /// it (no guard lock held).
    fn changed_page(&self, page: usize) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&page)
    }

    /// Page `page` was taken: what it saved of it is stale.
    fn taken(&self, page: usize) {
        self.1.lock().unwrap_or_else(|e| e.into_inner()).bytes.remove(&page);
    }

    /// Page `page` is about to change for the first time since it was
    /// taken: its bytes now, if armed for it. Reads title memory raw: the
    /// caller holds the guard lock, which [`serve`] takes (a lazy page has
    /// no access anyway, and callers give lazy pages their bytes before they
    /// mark them).
    fn save(&self, page: usize) {
        let mut saved = self.1.lock().unwrap_or_else(|e| e.into_inner());
        if !saved.armed(page) || saved.bytes.contains_key(&page) {
            return;
        }
        let mut bytes = vec![0u8; PAGE].into_boxed_slice();
        if crate::host::read_raw((page * PAGE) as *const u8, &mut bytes) {
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
    /// Whole pages whose title bytes are older than a host copy's
    /// (`VICTORY_OWNER=1`, [`lazify`]): no access at the title's addresses
    /// until the first touch gives them the copy's bytes ([`resolve`]).
    lazy: BTreeMap<usize, LazyPage>,
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
    lazy: BTreeMap::new(),
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
        let mut g = guards();
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
        for page in protect_noting(&mut g, &whole, &mut unmapped) {
            changed.remove(&page);
            self.changed.taken(page);
        }
        set_dirty(self.changed.2, !changed.is_empty());
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
        self.changed
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&page)
    }

    /// The caller made pages (page numbers) of its copy equal to title
    /// memory itself (what shaders wrote went back): taken and guarded.
    pub(crate) fn taken(&self, pages: &[usize]) {
        let mut g = guards();
        let mut changed = self.changed.0.lock().unwrap_or_else(|e| e.into_inner());
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
        set_dirty(self.changed.2, !changed.is_empty());
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

    /// The bytes page `page` (a page number) kept at arming, if it kept any
    /// (a copy-back may need them; they stay kept).
    pub(crate) fn peek_saved(&self, page: usize) -> Option<Box<[u8]>> {
        self.changed
            .1
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bytes
            .get(&page)
            .cloned()
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

/// `VICTORY_OWNER=1`: whole pages a copy's shaders wrote stay in the copy
/// until something touches title memory, instead of going back at the
/// submission's end (1 on, 0 off, `u8::MAX` not asked yet). Off by default;
/// read by the fault handler.
static OWNER: AtomicU8 = AtomicU8::new(u8::MAX);

/// Whether whole pages a copy's shaders wrote stay in the copy
/// ([`lazify`]).
pub(crate) fn owner() -> bool {
    match OWNER.load(Ordering::Relaxed) {
        u8::MAX => {
            let on = std::env::var("VICTORY_OWNER").as_deref() == Ok("1");
            OWNER.store(u8::from(on), Ordering::Relaxed);
            on
        }
        mode => mode == 1,
    }
}

/// Tests: the pages held now.
#[cfg(test)]
pub(crate) fn lazy_live() -> usize {
    LAZY_LIVE.load(Ordering::Relaxed)
}

/// Tests: [`owner`] on or off (a process-wide switch, as the environment
/// variable is).
#[cfg(test)]
pub(crate) fn set_owner(on: bool) {
    OWNER.store(u8::from(on), Ordering::Relaxed);
}

/// The runtime's answer to where title page `page` (a page number) can be
/// written without its guards: the address of the same page in a writable
/// mapping of the runtime's own that no tracker holds and the title does
/// not use, or `None` (memory that is not a section's: the runtime's
/// pools). A function pointer, 0 for none ([`set_writable_alias`]).
static WRITABLE_ALIAS: AtomicUsize = AtomicUsize::new(0);

/// The runtime tells how to find its own writable mapping of title pages
/// that sections back (its physical-page views): copy-back of marked pages
/// writes there, and gives lazy pages their bytes there, instead of opening
/// the title's guarded mapping (`crate::shadow`). The function must not
/// call into this module while its own lock is held by a call in here.
pub fn set_writable_alias(f: fn(usize) -> Option<usize>) {
    WRITABLE_ALIAS.store(f as usize, Ordering::Release);
}

/// See [`set_writable_alias`].
pub(crate) fn writable_alias(page: usize) -> Option<usize> {
    match WRITABLE_ALIAS.load(Ordering::Acquire) {
        0 => None,
        f => {
            // SAFETY: stored from a `fn(usize) -> Option<usize>`.
            let f = unsafe { std::mem::transmute::<usize, fn(usize) -> Option<usize>>(f) };
            f(page)
        }
    }
}

/// A whole page whose bytes a host copy holds and title memory does not
/// ([`lazify`]).
struct LazyPage {
    /// The copy's 64 KiB of the page (alive while the copy is: it gives its
    /// lazy pages their bytes before it goes, `crate::shadow`'s drop).
    src: usize,
    /// The page in the runtime's own writable mapping of its section
    /// ([`writable_alias`]), which the title does not see.
    alias: usize,
    /// The tracker holding the copy's bytes.
    owner: Arc<Changed>,
}

/// The last submission host started, and the one whose copy-back is running:
/// a page is not kept while a later submission may be writing that copy
/// (`VICTORY_GPU_OVERLAP`).
static SUBMISSION: AtomicU64 = AtomicU64::new(0);
static COPY_BACK: AtomicU64 = AtomicU64::new(0);

/// Host started submission `id`: its lists may write the copies from now on.
pub(crate) fn submission_started(id: u64) {
    SUBMISSION.fetch_max(id, Ordering::Relaxed);
}

/// Submission `id`'s copy-back runs: what it keeps is its own.
pub(crate) fn copy_back_started(id: u64) {
    COPY_BACK.store(id, Ordering::Relaxed);
}

/// Lazy pages now: a read of zero skips the guard lock ([`serve`]).
static LAZY_LIVE: AtomicUsize = AtomicUsize::new(0);

/// Pages that may be lazy: those below 2^44 (title memory is below 2^40).
const LAZY_LIMIT: usize = 1 << 28;

/// `VICTORY_OWNER` counters ([`owner_counts`]).
#[derive(Clone, Copy)]
enum Owner {
    Lazy,
    LazyBytes,
    Again,
    TitleReads,
    TitleWrites,
    HostTouches,
    Served,
    ServedBytes,
    PutBack,
    Forgot,
    Refused,
}
const OWNER_COUNTERS: usize = 11;
static OWNER_COUNTS: [AtomicU64; OWNER_COUNTERS] = [const { AtomicU64::new(0) }; OWNER_COUNTERS];

/// Adds `n` to counter `c` (`VICTORY_OWNER`, logged with the frame line).
fn count(c: Owner, n: u64) {
    OWNER_COUNTS[c as usize].fetch_add(n, Ordering::Relaxed);
}

/// The [`Owner`] counters since the last line, in that order (diagnostics).
#[cfg(feature = "diagnostics")]
pub(crate) fn owner_counts() -> [u64; OWNER_COUNTERS] {
    OWNER_COUNTS
        .each_ref()
        .map(|a| a.swap(0, Ordering::Relaxed))
}

/// Whether page `page` (a page number) is committed with no access at all:
/// still the mapping [`lazify`] left, not one made since (the runtime may
/// free memory without reporting it, `VICTORY_OWNER`).
fn no_access(page: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((page * PAGE) as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_NOACCESS
}

/// Whether a read fault at `at` may continue: the page is readable now (a
/// resolve in another thread made it so, and the held count may read zero).
fn read_continues(at: usize) -> bool {
    readable(at / PAGE)
}

/// What the handler answers for a read fault while no page is held: the page
/// may have been resolved since the fault (its count read zero first), so a
/// readable one continues. Lock-free, and read by the handler.
fn no_held_page_read(at: usize) -> i32 {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    };
    if read_continues(at) {
        EXCEPTION_CONTINUE_EXECUTION
    } else {
        EXCEPTION_CONTINUE_SEARCH
    }
}

/// Whether page `page` (a page number) can be read now (a read fault that
/// loses a race with another thread's [`resolve`] finds it so).
fn readable(page: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
        PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
        VirtualQuery,
    };
    // Protections a read may use; execute-only ones are not readable.
    const READABLE: u32 = PAGE_READONLY
        | PAGE_READWRITE
        | PAGE_WRITECOPY
        | PAGE_EXECUTE_READ
        | PAGE_EXECUTE_READWRITE
        | PAGE_EXECUTE_WRITECOPY;
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery((page * PAGE) as *const _, &mut info, size_of_val(&info)) };
    n != 0
        && info.State == MEM_COMMIT
        && info.Protect & PAGE_GUARD == 0
        && info.Protect & READABLE != 0
}

/// Sets the protection of pages `[first, last)`; false if it failed.
fn set_protection(first: usize, last: usize, protection: u32) -> bool {
    use windows_sys::Win32::System::Memory::VirtualProtect;
    let mut old = 0;
    // SAFETY: title memory; only its protection changes.
    unsafe {
        VirtualProtect(
            (first * PAGE) as *const _,
            (last - first) * PAGE,
            protection,
            &mut old,
        ) != 0
    }
}

/// Page `page` shows the copy's bytes in title memory and has no access at
/// all there; false when it cannot be made lazy (and goes back now).
pub(crate) fn lazify(page: usize, src: usize, alias: Option<usize>, copy: &Arc<Changed>) -> bool {
    use windows_sys::Win32::System::Memory::PAGE_NOACCESS;
    if !self::owner() {
        return false;
    }
    let Some(alias) = alias else { return false };
    // Our own view must be writable now, or the first touch could not put
    // the bytes back.
    if !writable(alias) {
        count(Owner::Refused, 1);
        return false;
    }
    // A later submission may be writing this copy (`VICTORY_GPU_OVERLAP`):
    // its bytes are not its own until it has run.
    if SUBMISSION.load(Ordering::Relaxed) > COPY_BACK.load(Ordering::Relaxed) {
        count(Owner::Refused, 1);
        return false;
    }
    let mut g = guards();
    if page >= LAZY_LIMIT
        || g.lazy.contains_key(&page)
        || g.busy.iter().any(|&(a, b)| (a..b).contains(&page))
        || g.alias_of.contains_key(&page)
        // A page the title wrote since the copy took it (or opened by a
        // write fault): its bytes are not the copy's, and a write landing
        // between now and the guard would be lost.
        || !g.protected.contains(&page)
    {
        count(Owner::Refused, 1);
        return false;
    }
    if copy.changed_page(page) {
        count(Owner::Refused, 1);
        return false;
    }
    // Other holders read the copy's bytes next (`serve`), and armed ones
    // keep what title memory holds for them (their copy-back merges against
    // it): before the page loses its access, which a save reads.
    mark_changed_except(&mut g, page, copy, true);
    // The count says a read of this page may fault, so it goes up while the
    // page still has its access, and down again if it keeps it.
    LAZY_LIVE.fetch_add(1, Ordering::Relaxed);
    if !set_protection(page, page + 1, PAGE_NOACCESS) {
        LAZY_LIVE.fetch_sub(1, Ordering::Relaxed);
        count(Owner::Refused, 1);
        return false;
    }
    g.protected.remove(&page);
    g.lazy.insert(
        page,
        LazyPage {
            src,
            alias,
            owner: copy.clone(),
        },
    );
    count(Owner::Lazy, 1);
    count(Owner::LazyBytes, PAGE as u64);
    true
}

/// Whether page `page` (a page number) is lazy with this tracker's copy
/// bytes at `src`: shaders wrote it again before anyone touched it, so
/// nothing goes back (yet).
pub(crate) fn owning(page: usize, src: usize, owner: &Arc<Changed>) -> bool {
    LAZY_LIVE.load(Ordering::Relaxed) != 0
        && guards()
            .lazy
            .get(&page)
            .is_some_and(|l| l.src == src && Arc::ptr_eq(&l.owner, owner))
}

/// Lazy page `page` of this tracker (see [`owning`]) was written again by
/// its shaders: it stays lazy, and every other holder counts it changed
/// (their next read is given the new bytes, [`serve`]).
pub(crate) fn again(page: usize, owner: &Arc<Changed>) {
    let mut g = guards();
    if !g.lazy.contains_key(&page) {
        return;
    }
    mark_changed_except(&mut g, page, owner, true);
    count(Owner::Again, 1);
}

/// Writes page `page`'s bytes (`bytes`, the merged 64 KiB) into title memory
/// through the runtime's own mapping (`alias`), takes the page in `owner`,
/// and marks every other tracker holding the page or another mapping of it:
/// the title's mappings stay as they are (guarded), so a title write waits
/// for the guard lock and lands after ours. False if the page is not guarded
/// any more (a write fault opened it; the caller writes it as any page).
pub(crate) fn put_back(page: usize, bytes: usize, alias: usize, owner: &Arc<Changed>) -> bool {
    let mut g = guards();
    if g.lazy.contains_key(&page) || !g.protected.contains(&page) || !writable(alias) {
        return false;
    }
    // Armed trackers keep the page's bytes before it changes, as `lazify`
    // does: their own copy-back merges against them.
    mark_changed_except(&mut g, page, owner, true);
    let others: Vec<usize> = g
        .alias_of
        .range(page..page + 1)
        .flat_map(|(_, o)| o.iter().copied())
        .collect();
    for other in others {
        mark_changed(&mut g, other, other + 1, true);
    }
    // SAFETY: `bytes` is the page's 64 KiB (the caller's buffer or the
    // copy's, alive), `alias` the runtime's own writable mapping of the page.
    unsafe { std::ptr::copy_nonoverlapping(bytes as *const u8, alias as *mut u8, PAGE) };
    let mut changed = owner.0.lock().unwrap_or_else(|e| e.into_inner());
    changed.remove(&page);
    owner.taken(page);
    set_dirty(owner.2, !changed.is_empty());
    count(Owner::PutBack, 1);
    true
}

/// What touched a lazy page ([`resolve`]).
#[derive(Clone, Copy)]
enum Touch {
    TitleRead,
    TitleWrite,
    Host,
}

/// Page `page`, if lazy, gets its copy's bytes now, through the runtime's
/// own mapping of it while every title mapping still has no access, and is
/// guarded as any taken page. True if it was lazy.
fn resolve(g: &mut Guards, page: usize, touch: Touch) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE};
    let Some(lazy) = g.lazy.remove(&page) else {
        return false;
    };
    // The count of held pages says "a read of this one may fault", so it goes
    // down only once the page is readable again (every path below); a reader
    // that finds it at zero then reads the page, not no access.
    // Our own view is gone (its section was freed): the bytes cannot be put
    // back, so title memory keeps its own, readable again, and its copy
    // reads them before its next use.
    if !writable(lazy.alias) {
        if no_access(page) {
            set_protection(page, page + 1, PAGE_READWRITE);
        }
        LAZY_LIVE.fetch_sub(1, Ordering::Relaxed);
        mark_tracker(&lazy.owner, page, false);
        count(Owner::Forgot, 1);
        return true;
    }
    // A mapping made since is new memory (the runtime freed it without
    // saying so): the copy's bytes are not its, and nothing is written.
    if !no_access(page) {
        LAZY_LIVE.fetch_sub(1, Ordering::Relaxed);
        mark_tracker(&lazy.owner, page, false);
        count(Owner::Forgot, 1);
        return true;
    }
    // The title's bytes are still those we left: no writer went past us.
    // SAFETY: the copy's 64 KiB of the page (alive while it is lazy: its
    // copy gives them back under this lock before it goes) and the runtime's
    // own writable mapping of the page.
    unsafe { std::ptr::copy_nonoverlapping(lazy.src as *const u8, lazy.alias as *mut u8, PAGE) };
    // Guarded again as any taken page; where it cannot be (or is not mapped
    // any more), accessible as it was, and its copy reads title memory again
    // before its next use.
    if set_protection(page, page + 1, PAGE_READONLY) {
        g.protected.insert(page);
    } else {
        set_protection(page, page + 1, PAGE_READWRITE);
        mark_tracker(&lazy.owner, page, false);
    }
    LAZY_LIVE.fetch_sub(1, Ordering::Relaxed);
    match touch {
        Touch::TitleRead => count(Owner::TitleReads, 1),
        Touch::TitleWrite => count(Owner::TitleWrites, 1),
        Touch::Host => count(Owner::HostTouches, 1),
    }
    true
}

/// [`resolve`] of the lazy pages of `[first, last)`.
fn resolve_range(g: &mut Guards, first: usize, last: usize, touch: Touch) {
    if LAZY_LIVE.load(Ordering::Relaxed) == 0 {
        return;
    }
    let pages: Vec<usize> = g.lazy.range(first..last).map(|(&p, _)| p).collect();
    for page in pages {
        resolve(g, page, touch);
    }
}

/// The pages of `[first, last)` (page numbers) held for a copy now.
/// The pages of `[first, last)` (page numbers) held for tracker `owner`: a
/// page another copy holds is its own to give back ([`put_kept`]).
pub(crate) fn held_pages(first: usize, last: usize, owner: &Arc<Changed>) -> Vec<usize> {
    if LAZY_LIVE.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    guards()
        .lazy
        .range(first..last)
        .filter(|(_, l)| Arc::ptr_eq(&l.owner, owner))
        .map(|(&p, _)| p)
        .collect()
}

/// Page `page`, if held, gets its copy's bytes now: they are older, valid
/// data (a submission that did not write it).
pub(crate) fn resolve_page(page: usize) {
    if LAZY_LIVE.load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut g = guards();
    resolve(&mut g, page, Touch::Host);
}

/// Page `page` was held in a copy and the shaders' writes to it this
/// submission are withheld: `bytes` (what it held before them, kept at
/// arming, [`arm_keeping`]) go back instead, the held record is dropped, and
/// every holder reads title memory again. False if it was not held.
pub(crate) fn put_kept(page: usize, bytes: &[u8], owner: &Arc<Changed>) -> bool {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE};
    let mut g = guards();
    // Another copy's page is its own to give back: this one's bytes are not
    // what it holds.
    if !g
        .lazy
        .get(&page)
        .is_some_and(|l| Arc::ptr_eq(&l.owner, owner))
    {
        return false;
    }
    // Its own view or the page itself may be gone or show something else now:
    // then title memory keeps what it has.
    let shown = no_access(page);
    let lazy = g.lazy.remove(&page).expect("checked above");
    if shown && writable(lazy.alias) {
        // SAFETY: `bytes` is the page's 64 KiB (the tracker's saved bytes),
        // `alias` the runtime's own writable mapping of the page.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), lazy.alias as *mut u8, PAGE) };
    }
    // A mapping made since is left as it is (`resolve` does the same), and a
    // page that cannot be guarded again is left readable.
    if shown && !set_protection(page, page + 1, PAGE_READONLY) {
        set_protection(page, page + 1, PAGE_READWRITE);
    } else if shown {
        g.protected.insert(page);
    }
    // Reads of it must not meet no access with nothing holding it: counted
    // once the page is readable again (`B1`).
    LAZY_LIVE.fetch_sub(1, Ordering::Relaxed);
    // What it shows is not what any copy holds: all read it again.
    mark_changed(&mut g, page, page + 1, false);
    count(Owner::Forgot, 1);
    true
}

/// Marks page `page` (a page number) changed in every tracker holding it but
/// `except`; with `save`, armed trackers keep its bytes first (read raw:
/// callers hold the guard lock, which [`serve`] takes).
fn mark_changed_except(g: &mut Guards, page: usize, except: &Arc<Changed>, save: bool) {
    let holders: Vec<Arc<Changed>> = g
        .holders
        .get(&page)
        .into_iter()
        .flatten()
        .filter(|c| !Arc::ptr_eq(c, except))
        .cloned()
        .collect();
    for c in holders {
        mark_tracker(&c, page, save);
    }
}

/// Marks page `page` changed in tracker `c`; with `save`, before a write
/// lands: its bytes are kept if it is armed for the page.
fn mark_tracker(c: &Arc<Changed>, page: usize, save: bool) {
    let first_write = {
        let mut set = c.0.lock().unwrap_or_else(|e| e.into_inner());
        set_dirty(c.2, true);
        set.insert(page)
    };
    if first_write && save {
        c.save(page);
    }
}

/// The copy's bytes of whole pages `[from, from + len)` at title address
/// `address` that are lazy for this tracker are kept now as their bytes
/// before the submission: a title write's fault while they are armed
/// resolves them (its bytes would be the copy's of then, not of before),
/// and this is what its copy-back merges against.
pub(crate) fn arm_keeping(
    copy_start: usize,
    data: usize,
    address: usize,
    len: usize,
    owner: &Arc<Changed>,
) {
    if LAZY_LIVE.load(Ordering::Relaxed) == 0 {
        return;
    }
    let (first, last) = (address / PAGE, address.saturating_add(len).div_ceil(PAGE));
    let g = guards();
    // The held pages of this copy in the armed range: its copy-back merges
    // against what its page held before the submission, and a page another
    // copy holds is that copy's to give back (`crate::shadow::Shadow::withhold`).
    let mine: Vec<usize> = g
        .lazy
        .range(first..last)
        .filter(|(_, l)| Arc::ptr_eq(&l.owner, owner))
        .map(|(&p, _)| p)
        .collect();
    drop(g);
    if mine.is_empty() {
        return;
    }
    let mut saved = owner.1.lock().unwrap_or_else(|e| e.into_inner());
    for page in mine {
        if saved.armed(page) && !saved.bytes.contains_key(&page) && page * PAGE >= copy_start {
            // SAFETY: the copy's 64 KiB of the page, inside its mapping.
            let bytes = unsafe {
                std::slice::from_raw_parts((data + (page * PAGE - copy_start)) as *const u8, PAGE)
            };
            saved.bytes.insert(page, bytes.into());
        }
    }
}

impl Tracked {
    /// The tracker's identity, as [`lazify`], [`owning`] and the rest name
    /// it (`crate::shadow`'s copies).
    pub(crate) fn id(&self) -> &Arc<Changed> {
        &self.changed
    }

    /// The copy goes: its lazy pages get their bytes back before it can
    /// (`crate::shadow`'s drop).
    pub(crate) fn resolve_all(&self) {
        let (first, last) = (self.start / PAGE, self.end.div_ceil(PAGE));
        if LAZY_LIVE.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut g = guards();
        let mine: Vec<usize> = g
            .lazy
            .range(first..last)
            .filter(|(_, l)| Arc::ptr_eq(&l.owner, &self.changed))
            .map(|(&p, _)| p)
            .collect();
        for page in mine {
            resolve(&mut g, page, Touch::Host);
        }
    }
}

/// Reads of title memory `[at, at + len)` through the kernel: the lazy
/// pages among them are given the copy's bytes from the copy itself (no
/// write of title memory, which keeps the title's), the rest through `read`.
pub(crate) fn serve(
    at: usize,
    dst: *mut u8,
    len: usize,
    read: &mut impl FnMut(usize, *mut u8, usize) -> bool,
) -> bool {
    if LAZY_LIVE.load(Ordering::Relaxed) == 0 {
        return read(at, dst, len);
    }
    let end = at.saturating_add(len);
    let (first, last) = (at / PAGE, end.div_ceil(PAGE));
    // Under the guard lock the lazy pages' bytes, then the rest without it.
    let mut gaps: Vec<(usize, usize)> = Vec::new();
    {
        let g = guards();
        let mut cur = at;
        for page in first..last {
            let Some(lazy) = g.lazy.get(&page) else {
                continue;
            };
            let (a, b) = ((page * PAGE).max(at), ((page + 1) * PAGE).min(end));
            if cur < a {
                gaps.push((cur, a));
            }
            // SAFETY: the copy's 64 KiB of the page (alive while it is lazy:
            // its copy gives them back under this lock before it goes) and
            // the caller's `len` bytes at `dst`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (lazy.src + (a - page * PAGE)) as *const u8,
                    dst.add(a - at),
                    b - a,
                )
            };
            count(Owner::Served, 1);
            count(Owner::ServedBytes, (b - a) as u64);
            cur = b;
        }
        if cur < end {
            gaps.push((cur, end));
        }
    }
    // A page of a gap made lazy meanwhile fails its read: served then.
    gaps.into_iter().fold(true, |ok, (a, b)| {
        // SAFETY: inside the caller's `len` bytes at `dst`.
        let to = unsafe { dst.add(a - at) };
        (read(a, to, b - a) || serve(a, to, b - a, read)) && ok
    })
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
    let busy = |page: usize| g.busy.iter().any(|&(a, b)| (a..b).contains(&page));
    let pages: Vec<usize> = pages.iter().copied().filter(|&p| !busy(p)).collect();
    let mut i = 0;
    // A lazy page has no access at all: guarded as it is (made read-only,
    // it would show its old bytes).
    let guarded = |g: &Guards, p: usize| g.protected.contains(&p) || g.lazy.contains_key(&p);
    while i < pages.len() {
        if guarded(g, pages[i]) {
            taken.push(pages[i]);
            i += 1;
            continue;
        }
        // A run of consecutive pages not guarded yet.
        let first = i;
        while i + 1 < pages.len() && pages[i + 1] == pages[i] + 1 && !guarded(g, pages[i + 1]) {
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
/// in every tracker holding them. Lazy pages among them get their copy's
/// bytes first (the write lands on those). True if one of `[first, last)`
/// was guarded.
fn changed(g: &mut Guards, first: usize, last: usize, save: bool, touch: Touch) -> bool {
    let others: Vec<usize> = g
        .alias_of
        .range(first..last)
        .flat_map(|(_, o)| o.iter().copied())
        .collect();
    resolve_range(g, first, last, touch);
    for &page in &others {
        resolve(g, page, touch);
    }
    let guarded = g.protected.range(first..last).next().is_some();
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
            mark_tracker(c, page, save);
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
        changed(&mut g, self.first, self.last, false, Touch::Host);
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
    changed(&mut g, first, last, true, Touch::Host);
    Writing { first, last }
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new, and read-write.
pub fn title_memory_remapped(at: usize, len: usize) {
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    let mut g = guards();
    // What it showed is gone already: nothing to keep. A lazy page there gets
    // its copy's bytes first (the runtime reports an unmap before it unmaps:
    // the physical page keeps them for the views still showing it).
    changed(&mut g, first, last, false, Touch::Host);
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
    // The new mapping shows the physical page: where it is lazy already, it
    // gets its copy's bytes now (a lazy page has no other address).
    resolve(&mut g, page, Touch::Host);
    for &other in others {
        resolve(&mut g, other / PAGE, Touch::Host);
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
        changed(&mut g, page, page + 1, false, Touch::Host);
    }
}

/// Registers the handler that takes writes to guarded blocks and reads and
/// writes of lazy pages, once, ahead of the crash reporter's.
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
            || r.ExceptionInformation[0] > WRITE
        {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let write = r.ExceptionInformation[0] == WRITE;
        let at = r.ExceptionInformation[1];
        // Reads fault here only on lazy pages (`VICTORY_OWNER`): with none
        // held, a read fault is someone else's, unless it lost the race with
        // a resolve that made the page readable again (`readable`, no lock).
        if !write && LAZY_LIVE.load(Ordering::Relaxed) == 0 {
            // With nothing kept nothing is ever held: a read fault is not
            // ours. Only the switch is asked (a cached load).
            return if owner() {
                no_held_page_read(at)
            } else {
                EXCEPTION_CONTINUE_SEARCH
            };
        }
        #[cfg(feature = "diagnostics")]
        let _timed = Timed(std::time::Instant::now(), crate::copystat::C::HandlerUs);
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        let mut g = guards();
        // A lazy page has no access at all: reads and writes fault here and
        // get the copy's bytes first (`VICTORY_OWNER`).
        if g.lazy.contains_key(&(at / PAGE)) {
            resolve(
                &mut g,
                at / PAGE,
                if write {
                    Touch::TitleWrite
                } else {
                    Touch::TitleRead
                },
            );
            if !write {
                return EXCEPTION_CONTINUE_EXECUTION;
            }
        }
        if !write {
            // Another thread resolved it since this one faulted: readable now.
            return if read_continues(at) {
                EXCEPTION_CONTINUE_EXECUTION
            } else {
                EXCEPTION_CONTINUE_SEARCH
            };
        }
        if changed(&mut g, at / PAGE, at / PAGE + 1, true, Touch::TitleWrite) {
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
    // `VICTORY_OWNER` (`lazify`, `serve`, `put_back`): the pages a copy's
    // shaders wrote stay in the copy and title memory keeps its older bytes
    // behind no access, until something touches it.

    /// The addresses [`writable_alias`] gives the owner tests' pages: the
    /// runtime's own writable view of the page (here another view of the
    /// same section, which the guards do not know).
    static ALIASES: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

    fn test_alias(page: usize) -> Option<usize> {
        ALIASES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&page)
            .copied()
    }

    /// The switch and the hook the runtime installs, once; the guard keeps
    /// [`set_owner`] on for the caller alone (a process-wide switch).
    fn owner_on() -> std::sync::MutexGuard<'static, ()> {
        static OWNER_TESTS: Mutex<()> = Mutex::new(());
        static HOOK: std::sync::Once = std::sync::Once::new();
        let guard = OWNER_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        HOOK.call_once(|| set_writable_alias(test_alias));
        set_owner(true);
        guard
    }

    fn protection(at: usize) -> u32 {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        info.Protect
    }

    /// A page of title memory (a view of a section, filled with `title`),
    /// the same page in another view of it that the guards do not know (what
    /// the runtime's own writable view is), and a host copy's 64 KiB.
    struct Owned {
        title: usize,
        alias: usize,
        copy: usize,
        page: usize,
    }

    impl Owned {
        fn new(title: u8, fill: u8) -> Owned {
            use windows_sys::Win32::System::Memory::{
                CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
            };
            // SAFETY: a pagefile section mapped read-write twice; the test
            // keeps both views, as does the runtime its own.
            let (a, b) = unsafe {
                let section = CreateFileMappingW(
                    -1isize as _,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    0,
                    PAGE as u32,
                    std::ptr::null(),
                );
                assert!(!section.is_null());
                (
                    MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, PAGE).Value as usize,
                    MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, PAGE).Value as usize,
                )
            };
            assert!(a != 0 && b != 0);
            let copy = pool(1);
            // SAFETY: our page and the copy's, both ours to fill.
            unsafe {
                std::ptr::write_bytes(a as *mut u8, title, PAGE);
                std::ptr::write_bytes(copy as *mut u8, fill, PAGE);
            }
            ALIASES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(a / PAGE, b);
            Owned {
                title: a,
                alias: b,
                copy,
                page: a / PAGE,
            }
        }

        /// A tracker that took the page: guarded, and holding the copy's
        /// bytes (`VICTORY_OWNER`).
        fn tracked(&self) -> Tracked {
            let t = track(self.title, PAGE, true).expect("a section view is tracked");
            assert_eq!(
                t.take(self.title, self.title + PAGE),
                [(self.title, self.title + PAGE)]
            );
            assert!(t.clean());
            t
        }

        fn lazy(&self, t: &Tracked) -> bool {
            lazify(self.page, self.copy, writable_alias(self.page), t.id())
        }

        /// The page's bytes at its title address (faults resolve lazy pages).
        fn title_byte(&self, at: usize) -> u8 {
            // SAFETY: inside the page; a lazy page faults into the handler.
            unsafe { ((self.title + at) as *const u8).read_volatile() }
        }

        fn set_copy(&self, at: usize, v: u8) {
            // SAFETY: inside the copy.
            unsafe { *((self.copy + at) as *mut u8) = v };
        }

        /// The page through the runtime's own view: what title memory holds
        /// without touching a guard.
        fn raw_byte(&self, at: usize) -> u8 {
            // SAFETY: inside the other view.
            unsafe { *((self.alias + at) as *const u8) }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            ALIASES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.page);
            free(self.copy);
        }
    }

    /// The count of held pages covers exactly the pages with no access: up
    /// before a page loses its access (and while it has none), back down once
    /// it is readable again. (The order inside `lazify`/`resolve` is not
    /// observable from one thread; this pins the states at the boundaries.)
    #[test]
    fn the_count_of_held_pages_covers_the_pages_with_no_access() {
        use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, PAGE_READONLY};
        let _owner = owner_on();
        let before = lazy_live();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        assert!(p.lazy(&a));
        assert_eq!(lazy_live(), before + 1, "held");
        assert_eq!(protection(p.title), PAGE_NOACCESS);
        resolve_page(p.page);
        assert_eq!(lazy_live(), before, "readable again");
        assert_eq!(protection(p.title), PAGE_READONLY);
        drop(a);
    }

    /// A page made lazy keeps title memory's bytes until something touches
    /// it: nothing goes back for a page nobody reads.
    #[test]
    fn a_lazy_page_leaves_title_memory_alone_until_a_touch() {
        use windows_sys::Win32::System::Memory::PAGE_NOACCESS;
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let t = p.tracked();
        assert!(p.lazy(&t));
        assert_eq!(protection(p.title), PAGE_NOACCESS);
        assert_eq!(
            p.raw_byte(0),
            0xAA,
            "the copy's bytes are not in title memory"
        );
        assert_eq!(p.raw_byte(PAGE - 1), 0xAA);
    }

    /// A title read of a lazy page gets the copy's bytes (the fault handler
    /// gives them first) and the page is left readable, as any taken page.
    #[test]
    fn a_title_read_of_a_lazy_page_gets_its_copys_bytes() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let t = p.tracked();
        p.set_copy(3, 0xDD);
        assert!(p.lazy(&t));
        assert_eq!(p.title_byte(0), 0xBB);
        assert_eq!(p.title_byte(3), 0xDD);
        assert_eq!(p.title_byte(PAGE - 1), 0xBB);
        assert_eq!(
            protection(p.title),
            PAGE_READONLY,
            "readable, guarded again"
        );
        assert_eq!(p.raw_byte(7), 0xBB);
    }

    /// A title write to a lazy page lands on the copy's bytes: the shaders'
    /// bytes are not lost where the title did not write.
    #[test]
    fn a_title_write_of_a_lazy_page_keeps_the_copys_other_bytes() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let t = p.tracked();
        p.set_copy(5, 0xDD);
        assert!(p.lazy(&t));
        // SAFETY: inside the page; the write faults into the handler.
        unsafe { ((p.title + 9) as *mut u8).write_volatile(0xCC) };
        assert_eq!(p.title_byte(0), 0xBB);
        assert_eq!(
            p.title_byte(5),
            0xDD,
            "the shaders' byte survived the write"
        );
        assert_eq!(p.title_byte(9), 0xCC);
        assert_eq!(p.title_byte(PAGE - 1), 0xBB);
    }

    /// Our host's kernel reads are given a lazy page's copy bytes without
    /// writing title memory, which keeps its bytes.
    #[test]
    fn kernel_reads_are_served_a_lazy_pages_copys_bytes() {
        use windows_sys::Win32::System::Memory::PAGE_NOACCESS;
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let t = p.tracked();
        p.set_copy(11, 0xDD);
        assert!(p.lazy(&t));
        let mut bytes = vec![0u8; PAGE];
        assert!(crate::host::read_into(p.title as *const u8, &mut bytes));
        assert!(
            bytes
                .iter()
                .enumerate()
                .all(|(i, &b)| b == if i == 11 { 0xDD } else { 0xBB })
        );
        assert_eq!(protection(p.title), PAGE_NOACCESS, "nothing went back");
        assert_eq!(p.raw_byte(11), 0xAA);
        // The same through a copy that runs over the page (`copy_mapped`).
        let mut whole = vec![0u8; 2 * PAGE];
        crate::host::copy_mapped(whole.as_mut_ptr(), p.title as *const u8, PAGE, false);
        assert!(
            whole[..PAGE]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == if i == 11 { 0xDD } else { 0xBB })
        );
        assert_eq!(protection(p.title), PAGE_NOACCESS);
    }

    /// A kernel write gives a lazy page the copy's bytes first, then writes
    /// its own: the title's bytes land on the shaders'.
    #[test]
    fn a_kernel_write_gives_a_lazy_page_its_copys_bytes_first() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let t = p.tracked();
        p.set_copy(5, 0xDD);
        assert!(p.lazy(&t));
        {
            let _writing = writing(p.title + 9, 2);
            crate::host::write_mapped((p.title + 9) as *mut u8, [0xCC, 0xCE].as_ptr(), 2);
        }
        assert_eq!(p.title_byte(5), 0xDD);
        assert_eq!(p.title_byte(9), 0xCC);
        assert_eq!(p.title_byte(10), 0xCE);
        assert_eq!(p.title_byte(0), 0xBB);
    }

    /// A lazy page its copy's shaders write again keeps the copy's newest
    /// bytes and is never written in between; the other holders of the page
    /// read them next.
    #[test]
    fn a_lazy_page_written_again_is_never_written_in_between() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let t = p.tracked();
        let other = track(p.title, PAGE, true).expect("a second tracker of the page");
        other.take(p.title, p.title + PAGE);
        assert!(p.lazy(&t));
        assert!(
            other.changed(p.page),
            "other holders read the copy's bytes next"
        );
        p.set_copy(0, 0xDD);
        assert!(owning(p.page, p.copy, t.id()));
        again(p.page, t.id());
        assert_eq!(p.raw_byte(0), 0xAA, "still nothing written");
        assert_eq!(p.title_byte(0), 0xDD, "the newest bytes reach the title");
    }

    /// A page that cannot stay lazy goes back while our own mapping writes
    /// it: the title's mapping is never opened, so a title write in between
    /// faults first and lands after.
    #[test]
    fn put_back_writes_through_our_own_mapping() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let t = p.tracked();
        // SAFETY: inside the copy, as shaders would write it.
        unsafe { std::ptr::write_bytes(p.copy as *mut u8, 0xEE, PAGE) };
        assert!(put_back(p.page, p.copy, p.alias, t.id()));
        assert_eq!(
            protection(p.title),
            PAGE_READONLY,
            "the title's mapping was not opened"
        );
        assert_eq!(p.title_byte(0), 0xEE, "written through our own view");
        assert!(t.clean(), "the copy holds the title's bytes");
    }

    /// What the world does: pages its shaders write are marked again by the
    /// next submissions before anything reads them, and only the last write
    /// reaches title memory.
    #[test]
    fn a_page_written_again_and_again_goes_back_once() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0);
        let t = p.tracked();
        for round in 0..16u8 {
            if owning(p.page, p.copy, t.id()) {
                p.set_copy(0, round);
                again(p.page, t.id());
            } else {
                p.set_copy(0, round);
                assert!(p.lazy(&t), "round {round}: kept in the copy");
            }
            assert_eq!(p.raw_byte(0), 0xAA, "round {round}: nothing written yet");
        }
        assert_eq!(p.title_byte(0), 15, "the last write reaches the title");
    }

    /// What cannot stay lazy goes back now: with the switch off, a page the
    /// title wrote, an unguarded page, an aliased page, and one without the
    /// runtime's own mapping of it.
    #[test]
    fn pages_that_cannot_stay_lazy_go_back() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        // Not taken (unguarded): a title write may have landed unseen.
        let u = track(p.title, PAGE, true).expect("tracked");
        assert!(!p.lazy(&u), "an unguarded page");
        let t = p.tracked();
        assert!(p.lazy(&t), "a taken one stays");
        assert!(!p.lazy(&t), "one already lazy");
        assert!(!lazify(p.page, p.copy, None, t.id()), "no mapping of ours");
        // SAFETY: the page through our own view: a title write behind a
        // fault.
        unsafe { *(p.alias as *mut u8) = 0xCC };
        title_memory_remapped(p.title, PAGE);
        assert!(!p.lazy(&t), "the page was written or remapped since");
        set_owner(false);
        let q = Owned::new(0x11, 0x22);
        let v = q.tracked();
        assert!(!q.lazy(&v), "the switch off");
    }

    /// A copy that goes gives its lazy pages their bytes back first.
    #[test]
    fn a_copy_going_gives_its_lazy_pages_back() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let t = p.tracked();
        p.set_copy(0, 0xDD);
        assert!(p.lazy(&t));
        t.resolve_all();
        assert_eq!(p.raw_byte(0), 0xDD);
        assert_eq!(p.raw_byte(1), 0xBB);
    }

    /// A withheld page the shaders wrote keeps the bytes kept at arming, not
    /// the copy's page (which holds the withheld writes).
    #[test]
    fn a_withheld_page_gives_back_the_bytes_kept_at_arming() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        assert!(p.lazy(&a));
        // The next submission writes the page (0xDD) and withholds it: the
        // bytes it held before them are kept at arming.
        a.arm(p.title, p.title + PAGE);
        arm_keeping(p.title, p.copy, p.title, PAGE, a.id());
        // ... then its shaders write the page, and it is withheld.
        p.set_copy(0, 0xDD);
        let before = a.peek_saved(p.page).expect("kept at arming");
        assert_eq!(before[0], 0xBB);
        assert!(put_kept(p.page, &before, a.id()));
        assert_eq!(p.title_byte(0), 0xBB, "not the withheld 0xDD");
        assert_eq!(protection(p.title), PAGE_READONLY);
        assert!(a.changed(p.page), "the copy reads title memory again");
    }

    /// A withheld page the shaders did not write keeps its older, valid bytes
    /// (they go back) and the record is dropped.
    #[test]
    fn a_withheld_page_they_did_not_write_gives_back_its_older_bytes() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        assert!(p.lazy(&a));
        assert!(
            a.peek_saved(p.page).is_none(),
            "not written this submission"
        );
        assert_eq!(held_pages(p.page, p.page + 1, a.id()), [p.page]);
        resolve_page(p.page);
        assert_eq!(p.title_byte(0), 0xBB, "the older bytes go back");
        assert_eq!(protection(p.title), PAGE_READONLY);
        assert!(held_pages(p.page, p.page + 1, a.id()).is_empty());
    }

    /// `put_back` marks the page changed in every other holder, and in the
    /// trackers of the addresses showing it: the old path went through
    /// `changed`, which did.
    #[test]
    fn put_back_marks_the_other_holders_of_the_page() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        let b = track(p.title, PAGE, true).expect("a second copy of the page");
        assert_eq!(b.take(p.title, p.title + PAGE), [(p.title, p.title + PAGE)]);
        assert!(b.clean());
        // SAFETY: inside the copy, as shaders would write it.
        unsafe { std::ptr::write_bytes(p.copy as *mut u8, 0xEE, PAGE) };
        assert!(put_back(p.page, p.copy, p.alias, a.id()));
        assert!(b.changed(p.page), "the other copy reads the new bytes next");
    }

    /// `put_back` writes the bytes it is given, not the copy's page: with a
    /// page the title wrote during the submission, the two differ (the merge
    /// holds both).
    #[test]
    fn put_back_writes_the_bytes_it_is_given() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        let merged = vec![0x5Au8; PAGE];
        assert!(put_back(p.page, merged.as_ptr() as usize, p.alias, a.id()));
        assert_eq!(p.title_byte(0), 0x5A);
        assert_eq!(p.title_byte(PAGE - 1), 0x5A);
    }

    /// A page a copy's shaders write again marks the holders that refreshed
    /// meanwhile: their bytes are the older ones until they are told.
    #[test]
    fn a_page_written_again_marks_the_holders_that_refreshed() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        let b = track(p.title, PAGE, true).expect("a second copy of the page");
        assert!(p.lazy(&a));
        // `b` refreshes: served `a`'s bytes, and takes the page.
        assert_eq!(b.take(p.title, p.title + PAGE), [(p.title, p.title + PAGE)]);
        assert!(b.clean());
        p.set_copy(0, 0xDD);
        again(p.page, a.id());
        assert!(
            b.changed(p.page),
            "its next read is served the newest bytes"
        );
        assert_eq!(p.title_byte(0), 0xDD);
    }

    /// A page whose own view is not writable is not kept: the first touch
    /// could not put its bytes back.
    #[test]
    fn lazify_refuses_a_page_whose_own_view_is_not_writable() {
        use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, PAGE_READWRITE, VirtualProtect};
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        let mut old = 0;
        // SAFETY: our own view of the page; its protection comes back below.
        unsafe { VirtualProtect(p.alias as *const _, PAGE, PAGE_NOACCESS, &mut old) };
        assert!(!p.lazy(&a), "no way to write the bytes back");
        // SAFETY: as above.
        unsafe { VirtualProtect(p.alias as *const _, PAGE, PAGE_READWRITE, &mut old) };
        assert!(p.lazy(&a));
    }

    /// A page of a copy a later submission may be writing is not kept: the
    /// copy's bytes are not its own until that submission has run.
    #[test]
    fn lazify_refuses_while_a_later_submission_runs() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        copy_back_started(4);
        submission_started(5);
        assert!(!p.lazy(&a), "the next submission may write the copy");
        copy_back_started(5);
        assert!(p.lazy(&a), "its own submission: nothing else runs");
    }

    /// A read fault that lost the race with another thread's resolve finds
    /// the last held page gone and the page readable: the handler's lock-free
    /// early out must continue, not crash.
    #[test]
    fn a_read_fault_after_the_last_resolve_continues() {
        let _owner = owner_on();
        let p = Owned::new(0xAA, 0xBB);
        let a = p.tracked();
        assert!(p.lazy(&a));
        assert_eq!(held_pages(p.page, p.page + 1, a.id()), [p.page]);
        // Another thread resolves the last held page.
        resolve_page(p.page);
        assert!(
            held_pages(p.page, p.page + 1, a.id()).is_empty(),
            "the last one"
        );
        use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_CONTINUE_EXECUTION;
        assert_eq!(
            no_held_page_read(p.title),
            EXCEPTION_CONTINUE_EXECUTION,
            "readable: the handler continues"
        );
        assert_eq!(p.title_byte(0), 0xBB);
        use windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_CONTINUE_SEARCH;
        assert_eq!(
            no_held_page_read(0x1000),
            EXCEPTION_CONTINUE_SEARCH,
            "nothing mapped: it does not"
        );
    }

    /// The same race for real: one thread faults into the handler while
    /// another resolves the last held page.
    #[test]
    fn a_read_fault_racing_the_last_resolve_does_not_crash() {
        let _owner = owner_on();
        for _ in 0..64 {
            let p = Owned::new(0xAA, 0xBB);
            let a = p.tracked();
            assert!(p.lazy(&a));
            let at = p.title;
            let reader = std::thread::spawn(move || {
                // SAFETY: inside the page; a held page faults into the handler.
                unsafe { (at as *const u8).read_volatile() }
            });
            resolve_page(p.page);
            let got = reader.join().expect("the read must not crash");
            assert_eq!(got, 0xBB);
            drop(a);
        }
    }

    /// Random touches of one page, with the copy brought up to date by the
    /// runtime's own path before its shaders write it: every byte the title
    /// ever reads is the newest one written there.
    #[test]
    fn random_touches_lose_no_title_visible_byte() {
        let _owner = owner_on();
        let p = Owned::new(0, 0);
        let t = p.tracked();
        // `bytes`: what the page must show, in the copy or in title memory.
        let mut bytes = vec![0u8; PAGE];
        // The copy's bytes and whether the page is lazy now.
        let mut lazy = false;
        let mut seed = 0x1234_5678u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for step in 0..400 {
            let at = (next() as usize) % PAGE;
            match next() % 5 {
                // The title reads (a lazy page gives its bytes back).
                0 => {
                    assert_eq!(p.title_byte(at), bytes[at], "step {step}");
                    lazy = false;
                }
                // The title writes.
                1 => {
                    let v = next() as u8;
                    // SAFETY: inside the page (a lazy page faults first).
                    unsafe { ((p.title + at) as *mut u8).write_volatile(v) };
                    bytes[at] = v;
                    lazy = false;
                    // Its next refresh takes the title's bytes for the copy.
                    assert_eq!(t.take(p.title, p.title + PAGE), [(p.title, p.title + PAGE)]);
                    let mut now = vec![0u8; PAGE];
                    assert!(crate::host::read_into(p.title as *const u8, &mut now));
                    assert_eq!(now, bytes, "step {step}: the copy takes the title's bytes");
                    // SAFETY: inside the copy.
                    unsafe { std::ptr::copy_nonoverlapping(now.as_ptr(), p.copy as *mut u8, PAGE) };
                }
                // The shaders write the copy.
                2 => {
                    let v = next() as u8;
                    p.set_copy(at, v);
                    bytes[at] = v;
                    if lazy {
                        assert!(owning(p.page, p.copy, t.id()));
                        again(p.page, t.id());
                    } else {
                        assert!(p.lazy(&t), "step {step}: nothing has touched the page");
                        lazy = true;
                    }
                }
                // Our host reads.
                3 => {
                    let mut now = vec![0u8; PAGE];
                    assert!(crate::host::read_into(p.title as *const u8, &mut now));
                    assert_eq!(now, bytes, "step {step}: served");
                }
                // Our host writes.
                _ => {
                    let v = next() as u8;
                    {
                        let _writing = writing(p.title + at, 1);
                        crate::host::write_mapped((p.title + at) as *mut u8, &v, 1);
                    }
                    bytes[at] = v;
                    lazy = false;
                    assert_eq!(t.take(p.title, p.title + PAGE), [(p.title, p.title + PAGE)]);
                    let mut now = vec![0u8; PAGE];
                    assert!(crate::host::read_into(p.title as *const u8, &mut now));
                    assert_eq!(now, bytes, "step {step}");
                    // SAFETY: inside the copy.
                    unsafe { std::ptr::copy_nonoverlapping(now.as_ptr(), p.copy as *mut u8, PAGE) };
                }
            }
        }
    }
}
