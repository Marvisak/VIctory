//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: read-only
//! guards and the runtime's reports), all of it where the memory is not the
//! runtime's. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! The ranges brought up to date are those draws are known to read (vertex
//! and index buffer views, constant buffers in tables, root descriptors,
//! buffer views in tables) and, for shaders that index the heap
//! themselves, every buffer range the heap's views reach
//! (`crate::bindless`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::{HostList, host};
use crate::watch::PAGE;

fn merge_writeback(title: &mut [u8], host: &mut [u8], old: &[u8]) -> bool {
    let mut changed = 0;
    for ((title_byte, host_byte), old_byte) in title.iter_mut().zip(host).zip(old) {
        let mask = 0u8.wrapping_sub(u8::from(*host_byte != *old_byte));
        let delta = (*title_byte ^ *host_byte) & mask;
        *title_byte ^= delta;
        *host_byte = *title_byte;
        changed |= delta;
    }
    changed != 0
}

#[test]
fn writeback_merge_matches_bytewise_oracle() {
    for len in [0, 1, 63, 64, 65, 193, 65_536] {
        let old = vec![0x11; len];
        for stride in [1, 5, 67, len + 1] {
            let mut title = old.clone();
            let mut host = old.clone();
            for index in (0..len).step_by(3) {
                title[index] = 0x22;
            }
            for index in (0..len).step_by(stride) {
                host[index] = if index % 2 == 0 { 0x22 } else { 0x33 };
            }
            let expected: Vec<_> = host
                .iter()
                .zip(&old)
                .zip(&title)
                .map(|((&gpu, &previous), &cpu)| if gpu == previous { cpu } else { gpu })
                .collect();
            let changed = expected != title;
            assert_eq!(merge_writeback(&mut title, &mut host, &old), changed);
            assert_eq!(title, expected);
            assert_eq!(host, expected);
        }
    }
}

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// The pages changed since the copy took them (none: the memory is not
    /// the runtime's, copied whole each time).
    tracked: Option<crate::watch::Tracked>,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<std::collections::HashSet<(usize, usize)>>,
    /// The copy's bytes before the submission ran where its shaders write:
    /// what the GPU wrote differs. Disjoint segments (offset, bytes) in
    /// order, and the 64 KiB pieces (by title address) already compared.
    before: Mutex<Snapshot>,
    /// Its first bit in the buffer views' written-pages bitmap, one per
    /// 64 KiB title page it touches (tracked memory only): writes through
    /// views of it are marked there ([`Shadow::mark_entry`]).
    marks: Option<u32>,
    /// Where it starts in `buffer` (0 but for copies sharing a buffer), and
    /// the run of the shared buffer it holds ([`Arena`]), given back when it
    /// goes.
    base: u64,
    slice: Option<Slice>,
    /// Last presented frame a draw asked for this copy. Loose windows the
    /// world has stopped naming are the ones [`trim_loose`] gives up.
    used: std::sync::atomic::AtomicU64,
    /// Monotonic required vertex/index use sequence (zero means never).
    /// Pressure trimming retains only a bounded recent set of larger copies.
    required_draw: std::sync::atomic::AtomicU64,
    /// This multi-window loose copy was admitted through the bounded
    /// required-draw reserve while ordinary admission was closed.
    priority_multi: std::sync::atomic::AtomicBool,
    /// Call site and frame that first made this copy. Used only when a
    /// later required union cannot safely overlap it.
    #[cfg(feature = "diagnostics")]
    created_at: &'static std::panic::Location<'static>,
    #[cfg(feature = "diagnostics")]
    created_frame: u64,
}

/// Snapshot segments (offset, bytes) and the pieces compared.
type Snapshot = (Vec<(usize, Vec<u8>)>, std::collections::HashSet<usize>);

/// Snapshot memory kept for reuse (already touched, so no page faults).
static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// An empty buffer with room for `n` bytes, from the pool if it has one.
fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

/// Returns a snapshot buffer to the pool, which keeps the largest up to
/// [`POOL_BYTES`] in all: a world submission snapshots several copies, and
/// fresh buffers cost a page fault per 4 KiB touched (millions per frame).
fn give_back(v: Vec<u8>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
    let mut total = 0;
    pool.retain(|v| {
        total += v.capacity();
        total <= POOL_BYTES
    });
}

/// Snapshot memory kept for reuse.
const POOL_BYTES: usize = 512 << 20;

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// written only on the submitting thread.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    /// The host GPU address of a title address inside the buffer.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    /// Bytes from a title address to the buffer's end.
    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    /// Allocation bytes kept by a descriptor that views this copy.
    pub(crate) fn allocation_len(&self) -> u64 {
        self.len
    }

    /// `n` dwords of the copy at title address `address` (tests).
    #[cfg(test)]
    pub(crate) fn read_host(&self, address: u64, n: usize) -> Vec<u32> {
        let at = (address - self.start) as usize;
        assert!(at + 4 * n <= self.len as usize);
        // SAFETY: inside the mapping (checked); the GPU is done with it.
        unsafe { std::slice::from_raw_parts(self.data.add(at).cast::<u32>(), n).to_vec() }
    }

    /// Writes `bytes` into the copy at title address `address`, as shaders
    /// do (tests).
    #[cfg(test)]
    pub(crate) fn write_host(&self, address: u64, bytes: &[u8]) {
        let at = (address - self.start) as usize;
        assert!(at + bytes.len() <= self.len as usize);
        // SAFETY: inside the mapping (checked).
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.data.add(at), bytes.len()) };
    }

    /// Up to 64 bytes of the copy at title address `address`, as floats, and
    /// which copy it is (diagnostics: what shaders see there).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn peek(&self, address: u64, len: u64) -> String {
        let at = address.saturating_sub(self.start) as usize;
        let n = (len.min(64) as usize).min((self.len as usize).saturating_sub(at)) / 4;
        // SAFETY: inside the mapping (checked); a racing GPU write only makes
        // the logged value stale.
        let v = unsafe { std::slice::from_raw_parts(self.data.add(at).cast::<f32>(), n) };
        format!("copy {:#x}+{:#x} {v:?}", self.start, self.len)
    }

    /// Raw constant words for a bounded shader-input diagnostic.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn peek_words(&self, address: u64, len: u64) -> Vec<u32> {
        let at = address.saturating_sub(self.start) as usize;
        let n = (len.min(64) as usize).min((self.len as usize).saturating_sub(at)) / 4;
        // SAFETY: `find` checked this range against the live mapping.
        unsafe { std::slice::from_raw_parts(self.data.add(at).cast::<u32>(), n).to_vec() }
    }

    /// Offset of a title address from the buffer's start.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start + self.base
    }

    /// Where the title memory it copies starts, and its length.
    pub(crate) fn range(&self) -> (u64, u64) {
        (self.start, self.len)
    }

    /// No submitted list still has a declared write to copy back.
    fn quiescent(&self) -> bool {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// A draw named this copy. [`trim_loose`] keeps what was named this frame.
    fn touch(&self) {
        self.used.store(crate::queue::presented(), std::sync::atomic::Ordering::Relaxed);
    }

    fn used(&self) -> u64 {
        self.used.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A draw cannot run without this vertex or index range. Keep its copy
    /// eligible for the small pressure-retention set if it spans windows.
    pub(crate) fn note_required_draw(&self) {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let at = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed).saturating_add(1);
        self.required_draw.fetch_max(at, std::sync::atomic::Ordering::Relaxed);
    }

    fn required_at(&self) -> u64 {
        self.required_draw.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// After a submission whose shaders wrote title memory at addresses they
    /// computed (`crate::memory`), somewhere in the 64 KiB page at `page`:
    /// the bytes of the page (cut to the copy) that differ from title memory
    /// go back into it, when the title has not written the page since the
    /// copy took it (then title memory is the copy as it was before the
    /// submission, and what differs is what the GPU wrote). Returns the
    /// bytes written; a page the title wrote meanwhile keeps the title's
    /// bytes (logged once).
    // shortcut: a page the copy holds only in part (at its ends) has no
    // "copied" mark and is compared without the title-write check.
    pub(crate) fn copy_back_page(&self, page: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (page.max(start), (page + PAGE).min(end));
        if from >= to {
            return 0;
        }
        if let Some(t) = &self.tracked
            && t.changed(page / PAGE)
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    page = format_args!("{page:#x}"),
                    "the title and translated shaders both wrote a page of title memory in one submission: the title's bytes kept (logged once)"
                )
            });
            return 0;
        }
        let n = to - from;
        // SAFETY: inside the mapping; only the submitting thread writes it.
        let host = unsafe { std::slice::from_raw_parts(self.data.add(from - start), n) };
        let mut title = vec![0u8; n];
        if !crate::host::read_into(from as *const u8, &mut title) || title == host {
            return 0;
        }
        {
            let _writing = crate::watch::writing(from, n);
            crate::host::write_mapped(from as *mut u8, host.as_ptr(), n);
        }
        // Our write is not the title's: the copy holds the page as it is.
        if let Some(t) = &self.tracked {
            t.taken(&[page / PAGE]);
        }
        n
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        self.touch();
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    #[track_caller]
    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Arc<Shadow> {
        Shadow::new_at(buffer, data, gpu, start, len, None)
    }

    /// [`Shadow::new`] of a copy in a run of a shared buffer (`data` and
    /// `gpu` are the run's).
    #[track_caller]
    fn new_at(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64, slice: Option<Slice>) -> Arc<Shadow> {
        let tracked = crate::watch::track_copy(start as usize, len as usize, true);
        let marks = tracked.as_ref().and_then(|_| marks().take(pages(start, len)));
        let shadow = Arc::new(Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            tracked,
            pending: Mutex::default(),
            before: Mutex::default(),
            marks,
            base: slice.map_or(0, |s| s.offset),
            slice,
            used: std::sync::atomic::AtomicU64::new(crate::queue::presented()),
            required_draw: std::sync::atomic::AtomicU64::new(0),
            priority_multi: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "diagnostics")]
            created_at: std::panic::Location::caller(),
            #[cfg(feature = "diagnostics")]
            created_frame: crate::queue::presented(),
        });
        if let Some(base) = marks {
            self::marks().owners.insert(base, Arc::downgrade(&shadow));
        }
        shadow
    }

    /// The view-mark entry (`victory_shader::emit`, "Write marks") of a
    /// view of this copy whose first byte is at title address `start`: the
    /// bit of that byte's page and its offset in the page. None where
    /// writes through it are not marked (memory not tracked, or no bits
    /// left): such views declare what they write ([`Shadow::gpu_writes`]).
    pub(crate) fn mark_entry(&self, start: u64) -> Option<(u32, u32)> {
        let base = self.marks?;
        let page = (start >> 16).checked_sub(self.start >> 16)?;
        Some((base + u32::try_from(page).ok()?, (start & 0xFFFF) as u32))
    }

    /// Whether writes through views of this copy are marked.
    pub(crate) fn marked(&self) -> bool {
        self.marks.is_some()
    }

    /// `[address, address + len)` cut to the buffer, as offsets in the copy.
    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, the range
    /// is copied back into title memory, where the title reads it; until
    /// then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    ///
    /// `marked`: the shaders mark the pages they write through this copy's
    /// views (`victory_shader::emit`, "Write marks"): no snapshot then, only
    /// the pages they marked are compared ([`Shadow::copy_back_marked`]).
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64, marked: bool) {
        self.touch();
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize, marked && self.marked());
    }

    /// At submission, after the range's refresh: a submitted list's shaders
    /// write this range. Its bytes are kept to tell later what the GPU wrote:
    /// per copy and submission, segments covering the ranges the submission
    /// declares (the world declares overlapping views of its 174 MB heap
    /// dozens of times; one segment over all of them copied most of the
    /// heap), in reused memory (fresh snapshots cost ~2.3 million page
    /// faults per world frame).
    ///
    /// With `marked`, nothing is kept: the pages are armed instead, so that
    /// a title write there before the copy-back keeps the page's bytes from
    /// before it ([`crate::watch::Tracked::arm`]).
    pub(crate) fn writing(&self, address: usize, len: usize, marked: bool) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((address, address + len));
        if marked && let Some(t) = &self.tracked {
            t.arm(address, address + len);
            return;
        }
        let (from, to) = self.inside(address, len);
        if from >= to {
            return;
        }
        // SAFETY: inside the mapping.
        let host = |a: usize, b: usize| unsafe { std::slice::from_raw_parts(self.data.add(a), b - a) };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let segments = &mut before.0;
        // The segments the range overlaps or touches.
        let first = segments.partition_point(|(at, bytes)| at + bytes.len() < from);
        let last = first + segments[first..].iter().take_while(|(at, _)| *at <= to).count();
        if let [(at, bytes)] = &segments[first..last]
            && *at <= from
            && to <= at + bytes.len()
        {
            return;
        }
        let start = segments.get(first).filter(|_| first < last).map_or(from, |s| s.0.min(from));
        let end = segments[first..last].last().map_or(to, |(at, bytes)| (at + bytes.len()).max(to));
        #[cfg(feature = "diagnostics")]
        let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::SnapshotUs);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::SnapshotBytes, (end - start) as u64);
        #[cfg(feature = "diagnostics")]
        crate::copystat::snapshot(self.start, self.len, (end - start) as u64);
        // Parts newly covered from the copy, the rest as it was taken.
        let mut grown = pooled(end - start);
        let mut at = start;
        for (taken, bytes) in segments.drain(first..last) {
            grown.extend_from_slice(host(at, taken));
            grown.extend_from_slice(&bytes);
            at = taken + bytes.len();
            give_back(bytes);
        }
        grown.extend_from_slice(host(at, end));
        segments.insert(first, (start, grown));
    }

    /// `[address, address + len)` expanded outward to 64 KiB page
    /// boundaries and cut to the copy (`[start, start + len)`): whole pages
    /// so [`crate::watch::Tracked::take`] guards them for
    /// [`Shadow::copy_back_marked`].
    pub(crate) fn page_range(&self, address: u64, len: u64) -> (u64, u64) {
        let end = self.start.saturating_add(self.len);
        let from = address.clamp(self.start, end);
        let to = address.saturating_add(len).clamp(from, end);
        if from >= to {
            return (from, 0);
        }
        let page = PAGE as u64;
        let from = (from & !(page - 1)).max(self.start);
        let to = to.checked_next_multiple_of(page).unwrap_or(end).min(end);
        (from, to - from)
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        self.copy_back_or_not(address, len, true)
    }

    /// After the submission ran: what its shaders wrote in the range stays
    /// in the copy and does not go into title memory (they computed it
    /// from memory with no copy, `crate::memory`).
    pub(crate) fn keep_writes(&self, address: usize, len: usize) {
        self.copy_back_or_not(address, len, false);
    }

    fn copy_back_or_not(&self, address: usize, len: usize, back: bool) -> usize {
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let (segments, compared) = &mut *snapshot;
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some((taken, before)) = segments.iter().find(|(at, bytes)| *at <= from && to <= at + bytes.len())
            && back
            && from < to
        {
            let before = &before[from - taken..to - taken];
            #[cfg(feature = "diagnostics")]
            let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::CompareUs);
            // By 64 KiB piece: the copy compared with its bytes before the
            // submission, title memory read only where the GPU wrote.
            const PIECE: usize = 64 << 10;
            let mut title = vec![0u8; PIECE];
            let mut synced = Vec::new();
            let mut at = from;
            while at < to {
                let n = (PIECE - (start + at) % PIECE).min(to - at);
                // SAFETY: inside the mapping; only the submitting thread
                // writes it.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                let old = &before[at - from..at - from + n];
                let title = &mut title[..n];
                // A whole piece is compared once per submission, whichever
                // declared ranges overlap it: then what the GPU wrote there
                // went back.
                if n == PIECE && !compared.insert(start + at) {
                    at += n;
                    continue;
                }
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::CompareBytes, n as u64);
                if host != old && crate::host::read_into((start + at) as *const u8, title) {
                    // Title memory with the GPU's bytes put in; the copy
                    // takes the title's bytes in turn.
                    if merge_writeback(title, host, old) {
                        let _writing = crate::watch::writing(start + at, n);
                        crate::host::write_mapped((start + at) as *mut u8, title.as_ptr(), n);
                        written += n;
                    }
                    synced.push((start + at) / PAGE);
                }
                at += n;
            }
            // Pages made equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh).
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            if let Some(t) = &self.tracked {
                let whole: Vec<usize> = synced.into_iter().filter(|p| (first..last).contains(p)).collect();
                t.taken(&whole);
            }
        }
        // Every submitted step that declared the range is done with it (one
        // submission runs at a time), and the snapshot with the last range.
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(&(address, address + len));
        if pending.is_empty() {
            compared.clear();
            for (_, bytes) in segments.drain(..) {
                give_back(bytes);
            }
            if let Some(t) = &self.tracked {
                t.disarm();
            }
        }
        written
    }

    /// After a submission whose shaders marked the 64 KiB title page at
    /// `page` as written through views of this copy: the bytes they changed
    /// go into title memory, and the copy takes the title's others. What
    /// they changed is what differs from the copy's bytes before the
    /// submission: title memory where the title has not written the page
    /// since the copy took it, else what the page held when the title first
    /// wrote it ([`crate::watch::Tracked::saved`], if the page was armed).
    /// Returns the bytes written.
    // shortcut: a page the title wrote that was not armed (a write through
    // the heap's views no list declared) keeps the title's bytes, losing
    // the shaders' there (counted).
    pub(crate) fn copy_back_marked(&self, page: usize, title: &mut [u8]) -> usize {
        let Some(t) = &self.tracked else {
            return 0;
        };
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (page.max(start), (page + PAGE).min(end));
        if from >= to {
            return 0;
        }
        // Our own write below must not save the page for this copy again.
        t.unarm(page / PAGE);
        let saved = t.saved(page / PAGE);
        if saved.is_none() && t.changed(page / PAGE) {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::MarkedKept, 1);
            return 0;
        }
        let n = to - from;
        // SAFETY: inside the mapping; only the submitting thread writes it.
        let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(from - start), n) };
        // The caller reuses one page-sized buffer across the submission.
        // `read_into` fills every byte used here before any comparison.
        let title = &mut title[..n];
        #[cfg(feature = "diagnostics")]
        let read_at = std::time::Instant::now();
        let readable = crate::host::read_into(from as *const u8, title);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::MarkedReadUs, read_at.elapsed().as_micros() as u64);
        if !readable {
            return 0;
        }
        #[cfg(feature = "diagnostics")]
        let compare_at = std::time::Instant::now();
        let written = match &saved {
            // The title's bytes are the copy's from before: what differs is
            // the shaders'.
            None => (*host != title[..]).then_some(host.as_ptr()),
            Some(before) => {
                let before = &before[from - page..to - page];
                merge_writeback(title, host, before).then_some(title.as_ptr())
            }
        };
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::MarkedCompareUs, compare_at.elapsed().as_micros() as u64);
        if let Some(bytes) = written {
            #[cfg(feature = "diagnostics")]
            let write_at = std::time::Instant::now();
            let _writing = crate::watch::writing(from, n);
            crate::host::write_mapped(from as *mut u8, bytes, n);
            drop(_writing);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::MarkedWriteUs, write_at.elapsed().as_micros() as u64);
        }
        // Equal to title memory now: taken, unless it holds the page in part.
        if from == page && to == page + PAGE {
            #[cfg(feature = "diagnostics")]
            let take_at = std::time::Instant::now();
            t.taken(&[page / PAGE]);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::MarkedTakenUs, take_at.elapsed().as_micros() as u64);
        }
        written.map_or(0, |_| n)
    }

    /// Whether the title wrote none of the copy's memory since the copy took
    /// it (tracked memory only): [`Shadow::update`] would copy nothing.
    pub(crate) fn clean(&self) -> bool {
        self.tracked.as_ref().is_some_and(crate::watch::Tracked::clean)
    }

    /// Its tracker's bit: `crate::watch::clean_bit` of it is
    /// [`Shadow::clean`] (none for memory that is not tracked: never clean).
    pub(crate) fn dirty_bit(&self) -> Option<u32> {
        self.tracked.as_ref().and_then(crate::watch::Tracked::bit)
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        if self.clean() {
            return 0;
        }
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        if self.tracked.is_some() {
            return self.update_tracked(from, to, !busy.is_empty());
        }
        if !busy.is_empty() {
            // Around ranges shaders are writing: the gaps, each as below
            // (in the world most submissions have such ranges; copying the
            // gaps whole cost ~1 s per frame). Under them nothing: what the
            // title wrote there is not known.
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                let (a, e) = (a.max(from), e.min(to));
                if a > at {
                    bytes += self.update_untracked(at, a);
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_untracked(at, to);
            }
            return bytes;
        }
        self.update_untracked(from, to)
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where the
    /// memory is tracked: the pages the title wrote since they were taken,
    /// in one [`crate::watch::Tracked::take`], so that a page wholly in the
    /// range is guarded even where shaders write part of it (split at those
    /// ranges it stayed changed, and [`Shadow::copy_back_marked`] dropped
    /// what they wrote).
    ///
    /// `busy`: a list submitted earlier declared that shaders write in the
    /// range. A submission waits for the one before it, so none of this
    /// submission's shaders has run yet: the pages the title wrote are
    /// copied as anywhere else. Skipped, the title's constants there stayed
    /// stale for the whole submission (a dispatch read its sizes as zeros
    /// and looped until the device was removed). The same bytes go into the
    /// snapshot `writing` took, so copy-back neither takes them for the
    /// GPU's nor puts them back over a later title write.
    fn update_tracked(&self, from: usize, to: usize, busy: bool) -> usize {
        let Some(t) = &self.tracked else {
            return 0;
        };
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let (runs, unmapped) = t.take_split(from, to);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::WatchUs, started.elapsed().as_micros() as u64);
        let start = self.start as usize;
        let mut before = busy.then(|| self.before.lock().unwrap_or_else(|e| e.into_inner()));
        let mut sync_before = |a: usize, b: usize| {
            let Some(before) = &mut before else { return };
            let (a, b) = (a - start, b - start);
            for (at, bytes) in before.0.iter_mut() {
                let (s, e) = ((*at).max(a), (*at + bytes.len()).min(b));
                if s < e {
                    // SAFETY: inside the mapping.
                    let host = unsafe { std::slice::from_raw_parts(self.data.add(s), e - s) };
                    bytes[s - *at..e - *at].copy_from_slice(host);
                }
            }
        };
        let mut copied = 0;
        for (a, b) in runs {
            copied += self.copy(a, b);
            sync_before(a, b);
        }
        for (a, b) in unmapped {
            // SAFETY: inside the mapping (`take_split` cuts to the buffer).
            unsafe { self.data.add(a - start).write_bytes(0, b - a) };
            sync_before(a, b);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, (b - a) as u64);
        }
        // Parts on memory that is not the runtime's: whole, as below.
        for (a, b) in t.untracked(from, to) {
            #[cfg(feature = "diagnostics")]
            let started = std::time::Instant::now();
            copied += self.copy(a, b);
            sync_before(a, b);
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::WholeBytes, (b - a) as u64);
                add(C::WholeUs, started.elapsed().as_micros() as u64);
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::Watches, 1);
            add(C::Checked, (to - from) as u64);
            add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
        }
        copied
    }

    /// [`Shadow::update`] of untracked `[from, to)` (inside the buffer).
    fn update_untracked(&self, from: usize, to: usize) -> usize {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self.copy(from, to);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::WholeBytes, (to - from) as u64);
            add(C::WholeUs, started.elapsed().as_micros() as u64);
        }
        to - from
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        crate::host::copy_mapped(at, from as *const u8, to - from, false);
        if let Some(t) = &self.tracked {
            // SAFETY: `to - from` bytes inside the mapping, just copied.
            t.refreshed(from, unsafe { std::slice::from_raw_parts(at, to - from) });
        }
        to - from
    }
}

impl Drop for Shadow {
    fn drop(&mut self) {
        if let Some(slice) = self.slice {
            arena().give_back(slice);
        }
        if let Some(base) = self.marks {
            let mut m = marks();
            m.owners.remove(&base);
            m.give_back(base, pages(self.start, self.len));
        }
    }
}

/// The 64 KiB title pages `[start, start + len)` touches.
fn pages(start: u64, len: u64) -> u32 {
    ((start + len.max(1) - 1) >> 16).saturating_sub(start >> 16) as u32 + 1
}

/// The bits of the buffer views' written-pages bitmap (`victory_shader::
/// emit`, "Write marks"): a run per tracked copy, a bit per title page it
/// touches, for as long as it lives.
#[derive(Default)]
struct Marks {
    /// Free runs below `end`, by first bit: their length.
    free: BTreeMap<u32, u32>,
    /// Bits handed out at some time: those past it are clear.
    end: u32,
    /// The copy holding each run, by its first bit.
    owners: BTreeMap<u32, std::sync::Weak<Shadow>>,
}

static MARKS: Mutex<Marks> = Mutex::new(Marks {
    free: BTreeMap::new(),
    end: 0,
    owners: BTreeMap::new(),
});

fn marks() -> std::sync::MutexGuard<'static, Marks> {
    MARKS.lock().unwrap_or_else(|e| e.into_inner())
}

impl Marks {
    /// A run of `n` bits: its first. None when the bitmap is full (the last
    /// bit takes the marks of offsets past it, so it is never handed out).
    fn take(&mut self, n: u32) -> Option<u32> {
        if let Some((&at, &len)) = self.free.iter().find(|&(_, &len)| len >= n) {
            self.free.remove(&at);
            if len > n {
                self.free.insert(at + n, len - n);
            }
            return Some(at);
        }
        if self.end + n < victory_shader::emit::VIEW_WRITE_BITS {
            self.end += n;
            return Some(self.end - n);
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("no bits left in the views' written-pages bitmap: copies made now declare their shaders' writes"));
        None
    }

    /// The run of `n` bits at `at` is free again (joined to its neighbours).
    fn give_back(&mut self, mut at: u32, mut n: u32) {
        if let Some((&before, &len)) = self.free.range(..at).next_back()
            && before + len == at
        {
            self.free.remove(&before);
            (at, n) = (before, n + len);
        }
        if let Some(len) = self.free.remove(&(at + n)) {
            n += len;
        }
        self.free.insert(at, n);
    }
}

/// The buffer views' written-pages bitmap: the buffer, its mapping (bits
/// as dwords) and host GPU address.
struct Bitmap {
    _buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

// SAFETY: D3D12 resources are free-threaded; the mapping is read and cleared
// only between submissions, which run one at a time.
unsafe impl Send for Bitmap {}
// SAFETY: as above.
unsafe impl Sync for Bitmap {}

/// The bitmap of other shaders, or (`memory`) of shaders that use title
/// memory, whose marks go back only while it is not withheld
/// (`crate::memory::withholding`).
fn bitmap(memory: bool) -> Option<&'static Bitmap> {
    static BITMAPS: [std::sync::OnceLock<Option<Bitmap>>; 2] = [const { std::sync::OnceLock::new() }; 2];
    BITMAPS[usize::from(memory)]
        .get_or_init(|| {
            // Written pages, then one read mark per heap index.
            let bits = victory_shader::emit::VIEW_READS + victory_shader::emit::HEAP_VIEWS;
            let bytes = u64::from(bits / 8);
            let (buffer, data) = host()?.shared_buffer(bytes)?;
            crate::host::name(&buffer, || "views' written pages and read slots".into());
            // SAFETY: our mapped buffer of `bytes` bytes.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some(Bitmap {
                _buffer: buffer,
                data: data as usize,
                gpu,
            })
        })
        .as_ref()
}

/// The host GPU address of the buffer views' written-pages bitmap, for the
/// root UAV of shaders that mark their writes (`memory`: shaders that use
/// title memory, see [`bitmap`]).
pub(crate) fn view_writes(memory: bool) -> Option<u64> {
    bitmap(memory).map(|b| b.gpu)
}

/// Heap slots whose texture views shaders read: the marks seen so far (a
/// slot's stays set in the bitmap until [`forget_read`]), and the slots to
/// clear at the next [`take_reads`].
static READS: Mutex<(Vec<u32>, Vec<u32>)> = Mutex::new((Vec::new(), Vec::new()));

/// After a submission: the heap slots its shaders first read texture views
/// at (`victory_shader::emit`, "Read marks"), each once until
/// [`forget_read`]. Clears the slots forgotten since first.
pub(crate) fn take_reads() -> Vec<u32> {
    let words = (victory_shader::emit::HEAP_VIEWS / 32) as usize;
    let first = (victory_shader::emit::VIEW_READS / 32) as usize;
    // Both bitmaps: shaders that use title memory mark their reads in theirs.
    let mut maps: Vec<&mut [u32]> = [false, true]
        .into_iter()
        .filter_map(bitmap)
        // SAFETY: the mapped bitmap's read marks; the GPU is done with them
        // (one submission runs at a time).
        .map(|b| unsafe { std::slice::from_raw_parts_mut((b.data as *mut u32).add(first), words) })
        .collect();
    if maps.is_empty() {
        return Vec::new();
    }
    let mut reads = READS.lock().unwrap_or_else(|e| e.into_inner());
    let (seen, forgotten) = &mut *reads;
    seen.resize(words, 0);
    for slot in forgotten.drain(..) {
        let (w, bit) = ((slot / 32) as usize, 1u32 << (slot % 32));
        if w < words {
            maps.iter_mut().for_each(|bits| bits[w] &= !bit);
            seen[w] &= !bit;
        }
    }
    let mut new = Vec::new();
    for (w, s) in seen.iter_mut().enumerate() {
        let mut fresh = maps.iter().fold(0, |x, bits| x | bits[w]) & !*s;
        *s |= fresh;
        while fresh != 0 {
            new.push(w as u32 * 32 + fresh.trailing_zeros());
            fresh &= fresh - 1;
        }
    }
    new
}

/// The heap slots shaders read texture views at since the last call (both
/// bitmaps), whose marks are cleared: what the title used (eviction stamps,
/// `crate::resource::evict`). Between submissions, after [`take_reads`]
/// has seen every mark: its own record of slots seen keeps a slot read
/// again from being reported as new.
pub(crate) fn reads_since() -> Vec<u32> {
    let words = (victory_shader::emit::HEAP_VIEWS / 32) as usize;
    let first = (victory_shader::emit::VIEW_READS / 32) as usize;
    let mut slots = Vec::new();
    for b in [false, true].into_iter().filter_map(bitmap) {
        // SAFETY: the mapped bitmap's read marks; no submission runs.
        let bits = unsafe { std::slice::from_raw_parts_mut((b.data as *mut u32).add(first), words) };
        for (w, word) in bits.iter_mut().enumerate() {
            let mut x = std::mem::take(word);
            while x != 0 {
                slots.push(w as u32 * 32 + x.trailing_zeros());
                x &= x - 1;
            }
        }
    }
    slots.sort_unstable();
    slots.dedup();
    slots
}

/// Heap slot `slot` now holds a texture view left null: its next read is
/// marked again.
pub(crate) fn forget_read(slot: u32) {
    READS.lock().unwrap_or_else(|e| e.into_inner()).1.push(slot);
}

/// After a submission: every page its shaders marked as written through
/// views goes back ([`Shadow::copy_back_marked`]), but for pages in
/// `withheld` (ranges of copies that lists whose writes stay out of title
/// memory declared) and, with `memory_withheld`, pages that shaders using
/// title memory marked; the bitmaps are cleared. Before the copy-backs of
/// declared ranges, which disarm the copies.
// shortcut: marks do not tell lists apart: a page that a withheld list and
// another both wrote through views goes back unless the withheld list
// declared it.
pub(crate) fn copy_back_marks(withheld: &[(Arc<Shadow>, usize, usize)], memory_withheld: bool) {
    let (mut marked, mut held) = (Vec::new(), Vec::new());
    for memory in [false, true] {
        let Some(b) = bitmap(memory) else {
            continue;
        };
        let words = (victory_shader::emit::VIEW_WRITE_BITS / 32) as usize;
        // SAFETY: the mapped bitmap; the GPU is done with it (one submission
        // runs at a time).
        let bits = unsafe { std::slice::from_raw_parts_mut(b.data as *mut u32, words) };
        // Marks of offsets past the bitmap land in its last bit, no copy's.
        bits[words - 1] = 0;
        let used = (marks().end as usize).div_ceil(32).min(words);
        for (w, word) in bits[..used].iter_mut().enumerate() {
            let mut x = std::mem::take(word);
            let into = if memory && memory_withheld { &mut held } else { &mut marked };
            while x != 0 {
                into.push((w * 32) as u32 + x.trailing_zeros());
                x &= x - 1;
            }
        }
    }
    #[cfg(feature = "diagnostics")]
    let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::MarkedUs);
    marked.sort_unstable();
    marked.dedup();
    let page_of = |m: &Marks, n: u32| {
        let (&base, copy) = m.owners.range(..=n).next_back()?;
        let copy = copy.upgrade()?;
        let k = n - base;
        (k < pages(copy.start, copy.len)).then(|| {
            let page = (((copy.start >> 16) + u64::from(k)) << 16) as usize;
            (copy, page)
        })
    };
    // Held back: dropped from the copies (`crate::memory::drop_withheld`).
    if !held.is_empty() {
        let m = marks();
        for (_, page) in held.into_iter().filter_map(|n| page_of(&m, n)) {
            crate::memory::drop_withheld(page, PAGE);
        }
    }
    if marked.is_empty() {
        return;
    }
    let pages: Vec<(Arc<Shadow>, usize)> = {
        let m = marks();
        marked.iter().filter_map(|&n| page_of(&m, n)).collect()
    };
    // One page scratch per submission, rather than a fresh zeroed allocation
    // for each of the hundreds of marked pages in a world frame.
    let mut title = vec![0u8; PAGE];
    for (copy, page) in pages {
        if withheld
            .iter()
            .any(|(c, a, n)| Arc::ptr_eq(c, &copy) && *a < page + PAGE && page < a + n)
        {
            continue;
        }
        #[cfg(feature = "diagnostics")]
        let page_at = std::time::Instant::now();
        let written = copy.copy_back_marked(page, &mut title);
        #[cfg(feature = "diagnostics")]
        {
            crate::copystat::add(crate::copystat::C::MarkedPageUs, page_at.elapsed().as_micros() as u64);
            crate::copystat::add(crate::copystat::C::MarkedPages, 1);
            crate::copystat::add(crate::copystat::C::MarkedBytes, written as u64);
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = written;
    }
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
static BUFFERS: Mutex<BTreeMap<(u64, usize), Entry>> = Mutex::new(BTreeMap::new());

fn buffers() -> std::sync::MutexGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts copies made and title buffers released: [`all`] may differ when
/// it changed.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The generation of [`all`]: read it before calling that.
pub(crate) fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

fn changed() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    if buffers().remove(&(start, resource)).is_some_and(|e| e.shadow.is_some()) {
        changed();
    }
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
#[track_caller]
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    find_with_priority(address, len, false)
}

/// A root descriptor must have a host copy for its draw to run. It may use
/// the small reserve above the ordinary loose-copy budget, while the same
/// window and lifetime rules still apply.
#[track_caller]
pub(crate) fn find_root(address: u64, len: u64) -> Option<Arc<Shadow>> {
    find_with_priority(address, len, true)
}

/// The exact branch that refused a statically used table buffer's bounded
/// union copy. Only collected while that table lookup runs in diagnostics.
#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Debug)]
pub(crate) enum TableUnionDenial {
    InvalidRange,
    Cached,
    PartialRegisteredOverlap,
    RegisteredUnavailable,
    TitleFree,
    StrideMismatch,
    RootLooseCap,
    MultiwindowCap,
    TableCap,
    LooseOverlap,
    ReuseDisabled,
    LocalHeadroom,
    NoReusableSpan,
    ReusedAllocationFailed,
    HostAllocationFailed,
}

#[cfg(feature = "diagnostics")]
thread_local! {
    static TABLE_UNION_DIAG: std::cell::Cell<(bool, Option<TableUnionDenial>)> =
        const { std::cell::Cell::new((false, None)) };
}

#[cfg(feature = "diagnostics")]
fn note_table_union_denial(reason: TableUnionDenial) {
    TABLE_UNION_DIAG.with(|state| {
        let (active, current) = state.get();
        if active && current.is_none() {
            state.set((true, Some(reason)));
        }
    });
}

#[cfg(feature = "diagnostics")]
fn note_table_loose_denial(root: bool, required_draw: bool, reason: TableUnionDenial) {
    if root && required_draw {
        note_table_union_denial(reason);
    }
}

#[cfg(feature = "diagnostics")]
fn note_table_required_denial(required_draw: bool, reason: TableUnionDenial) {
    if required_draw {
        note_table_union_denial(reason);
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn diagnose_required_table_union(
    address: u64,
    len: u64,
    priority_creation_allowed: bool,
) -> (Option<(Arc<Shadow>, bool)>, Option<TableUnionDenial>) {
    struct Restore((bool, Option<TableUnionDenial>));
    impl Drop for Restore {
        fn drop(&mut self) {
            TABLE_UNION_DIAG.with(|state| state.set(self.0));
        }
    }
    let previous = TABLE_UNION_DIAG.with(|state| state.replace((true, None)));
    let _restore = Restore(previous);
    let result = find_required_union_with_room(address, len, priority_creation_allowed);
    let reason = TABLE_UNION_DIAG.with(|state| state.get().1);
    (result, reason)
}

/// A required vertex or index buffer missed ordinary admission. Small
/// multi-window ranges may use the root reserve, but only within a separate
/// 16 MiB cap and without creating a new write-back heap above the nonlocal
/// limit. A full-range copy is required; a partial GPU view would be unsafe.
#[track_caller]
pub(crate) fn find_required_draw_after_ordinary_miss(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let start = address & !(WINDOW - 1);
    let aligned_end = end.checked_next_multiple_of(WINDOW)?;
    let key = (start, aligned_end - start);
    if let Some(shadow) = existing_required_loose(key) {
        return Some(shadow);
    }
    if required_denied(key, false) {
        return None;
    }
    let map = buffers();
    if (key.1 > WINDOW && registered_intersects_map(&map, start, aligned_end))
        || (key.1 == WINDOW
            && map.range(..=(address, usize::MAX)).rev().take(64).any(|(_, entry)| entry.end >= end))
    {
        // The ordinary registered-buffer attempt failed for another reason;
        // a loose priority copy cannot safely alias a registered resource.
        required_denied(key, true);
        return None;
    }
    drop(map);
    let shadow = loose(address, end, true, true)?.0;
    shadow.note_required_draw();
    Some(shadow)
}

/// Preseed one shared vertex/index copy before binding either range. Both
/// admission attempts use `loose`'s map lock and overlap check; an ordinary
/// `find` followed by a priority fallback could create the first half and
/// then reject the second as an overlapping independent GPU copy.
pub(crate) fn find_required_union(address: u64, len: u64) -> Option<Arc<Shadow>> {
    find_required_union_with_room(address, len, true).map(|(shadow, _)| shadow)
}

/// The second field is true only if this call made a new loose copy through
/// the priority fallback. Registered copies, already-cached loose copies,
/// and new ordinary-admitted loose copies do not consume a caller's extra
/// priority reserve.
#[track_caller]
pub(crate) fn find_required_union_with_room(
    address: u64,
    len: u64,
    priority_creation_allowed: bool,
) -> Option<(Arc<Shadow>, bool)> {
    let Some(end) = address.checked_add(len).filter(|_| len > 0) else {
        #[cfg(feature = "diagnostics")]
        note_table_union_denial(TableUnionDenial::InvalidRange);
        return None;
    };
    let start = address & !(WINDOW - 1);
    let Some(aligned_end) = end.checked_next_multiple_of(WINDOW) else {
        #[cfg(feature = "diagnostics")]
        note_table_union_denial(TableUnionDenial::InvalidRange);
        return None;
    };
    let key = (start, aligned_end - start);
    if key.1 > REQUIRED_DRAW_MAX_SPAN {
        #[cfg(feature = "diagnostics")]
        note_table_union_denial(TableUnionDenial::InvalidRange);
        return None;
    }
    if let Some(shadow) = existing_required_loose(key) {
        return Some((shadow, false));
    }
    if required_denied(key, false) {
        #[cfg(feature = "diagnostics")]
        note_table_union_denial(TableUnionDenial::Cached);
        return None;
    }
    let registered = buffers();
    if registered_intersects_map(&registered, start, aligned_end) {
        let contained = registered.range(..=(address, usize::MAX)).rev().take(64)
            .any(|(_, entry)| entry.end >= end);
        drop(registered);
        // Use a registered copy only when it covers the entire union.
        // A partial registration cannot be replaced by a loose alias.
        if !contained {
            #[cfg(feature = "diagnostics")]
            note_table_union_denial(TableUnionDenial::PartialRegisteredOverlap);
            return None;
        }
        let found = find(address, len).inspect(|shadow| shadow.note_required_draw());
        #[cfg(feature = "diagnostics")]
        {
            if found.is_none() {
                note_table_union_denial(TableUnionDenial::RegisteredUnavailable);
            }
        }
        return found.map(|shadow| (shadow, false));
    }
    drop(registered);
    if let Some((shadow, _)) = loose(address, end, false, true) {
        shadow.note_required_draw();
        return Some((shadow, false));
    }
    if !priority_creation_allowed {
        #[cfg(feature = "diagnostics")]
        note_table_union_denial(TableUnionDenial::TableCap);
        return None;
    }
    let (shadow, created) = loose(address, end, true, true)?;
    shadow.note_required_draw();
    Some((shadow, created))
}

/// A covering loose copy already exists. This read-only admission probe
/// cannot allocate a buffer; table descriptors use it before considering a
/// priority-cap denial. A partially overlapping registered buffer must keep
/// its own copy, so it cannot share the loose one.
pub(crate) fn existing_required_table_copy(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len).filter(|_| len > 0)?;
    let start = address & !(WINDOW - 1);
    let aligned_end = end.checked_next_multiple_of(WINDOW)?;
    // Most denials have no existing copy. Avoid a full registered-buffer
    // scan for each of those hot table uses; recheck under both locks below.
    covering_loose_key(&LOOSE.lock().unwrap_or_else(|e| e.into_inner()),
        start, aligned_end - start)?;
    let registered = buffers();
    if registered_intersects_map(&registered, start, aligned_end) {
        return None;
    }
    let shadow = {
        let map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
        let key = covering_loose_key(&map, start, aligned_end - start)?;
        map.get(&key)?.clone()
    };
    drop(registered);
    shadow.touch();
    Some(shadow)
}

fn existing_required_loose(key: (u64, u64)) -> Option<Arc<Shadow>> {
    let map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    let key = covering_loose_key(&map, key.0, key.1)?;
    let shadow = map.get(&key)?.clone();
    shadow.touch();
    shadow.note_required_draw();
    Some(shadow)
}

/// A combined vertex/index copy must not alias any registered title buffer,
/// including one that covers only part of the aligned span. Scanning all
/// earlier starts is necessary because registered buffers can be nested.
fn registered_intersects_map(map: &BTreeMap<(u64, usize), Entry>, start: u64, end: u64) -> bool {
    map.range(..(end, 0)).any(|(&(at, _), entry)| at < end && entry.end > start)
}

#[track_caller]
fn find_with_priority(address: u64, len: u64, root: bool) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let mut map = buffers();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end, root, false).map(|(shadow, _)| shadow);
    };
    let entry = map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Freed title memory reads as zeros. A copy of it would only pin
        // write-back until something noticed.
        if matches!(title_memory(key.0, len), TitleMemory::Free) {
            return None;
        }
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(1 << 16))?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        entry.shadow = Some(Shadow::new(buffer, data, gpu, key.0, len));
        changed();
    }
    let shadow = entry.shadow.clone();
    if let Some(shadow) = &shadow {
        shadow.touch();
    }
    shadow
}

/// Describes why a root address without a copy may have failed to bind.
/// Called only for a bounded sample of failed draws in diagnostics builds.
#[cfg(feature = "diagnostics")]
pub(crate) fn diagnose_missed_root(address: u64) -> String {
    let map = buffers();
    let registered = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, entry)| entry.end > address)
        .map(|(&(start, _), entry)| (start, entry.end, entry.shadow.is_some()));
    // The ordinary lookup examines 64 preceding registrations. Diagnose
    // whether a deeper overlapping buffer was hidden by that bound.
    let beyond_64 = registered.is_none().then(|| {
        map.range(..=(address, usize::MAX))
            .rev()
            .skip(64)
            .find(|(_, entry)| entry.end > address)
            .map(|(&(start, _), entry)| (start, entry.end, entry.shadow.is_some()))
    }).flatten();
    drop(map);
    let start = address & !(WINDOW - 1);
    let len = WINDOW;
    let loose_cached = LOOSE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&(start, len));
    let title = match title_memory(start, len) {
        TitleMemory::Live => "live",
        TitleMemory::Uncommitted => "uncommitted",
        TitleMemory::Free => "free",
    };
    let memory = crate::host::video_memory();
    let root_limit = root_system_mb();
    let loose_bytes = LOOSE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    // This is a snapshot taken after find_root failed. Reproduce its branch
    // order so an address rejected by a budget is distinguishable from one
    // the title has actually freed. Allocation failures and concurrent
    // changes cannot be identified without retrying the allocation.
    let admission = if address.checked_add(1).is_none() {
        "root address overflow"
    } else if let Some((registered_start, registered_end, cached)) = registered {
        if cached {
            "registered copy changed after lookup"
        } else if matches!(
            title_memory(registered_start, registered_end - registered_start),
            TitleMemory::Free
        ) {
            "registered title memory free"
        } else {
            "registered host buffer unavailable or state changed"
        }
    } else if loose_cached {
        "loose copy changed after lookup"
    } else if title == "free" {
        "loose title memory free"
    } else if !root_reserve_room(loose_bytes, len) {
        "root loose-copy reserve full"
    } else if let Some([(local, local_budget), (system, _)]) = memory {
        if system >= root_limit {
            if root_reuse_only() {
                "nonlocal copy budget; no reusable write-back range"
            } else {
                "nonlocal copy budget"
            }
        } else if local + ROOT_LOCAL_HEADROOM_MB >= local_budget {
            "local copy headroom"
        } else {
            "loose host buffer unavailable or state changed"
        }
    } else {
        "loose host buffer unavailable or state changed"
    };
    format!(
        "admission={admission} registered={registered:?} beyond_64={beyond_64:?} loose={start:#x}+{len:#x} cached={loose_cached} title={title} video_memory_mb={memory:?} root_copy_budget_mb={root_limit} local_headroom_mb={ROOT_LOCAL_HEADROOM_MB} loose_bytes={loose_bytes} root_loose_cap={ROOT_LOOSE_CAP}"
    )
}

/// Describes a failed buffer lookup without creating a host copy. Unlike a
/// root descriptor, an index or argument buffer may span several loose
/// windows and normally uses the ordinary nonlocal admission limit.
#[cfg(feature = "diagnostics")]
pub(crate) fn diagnose_missed_buffer(address: u64, len: u64, priority: bool) -> String {
    let end = address.checked_add(len.max(1));
    let map = buffers();
    let registered = end.and_then(|end| {
        map.range(..=(address, usize::MAX))
            .rev()
            .take(64)
            .find(|(_, entry)| entry.end >= end)
            .map(|(&(start, _), entry)| (start, entry.end, entry.shadow.is_some()))
    });
    let beyond_64 = (registered.is_none()).then(|| {
        end.and_then(|end| {
            map.range(..=(address, usize::MAX))
                .rev()
                .skip(64)
                .find(|(_, entry)| entry.end >= end)
                .map(|(&(start, _), entry)| (start, entry.end, entry.shadow.is_some()))
        })
    }).flatten();
    drop(map);
    let start = address & !(WINDOW - 1);
    let loose_len = end
        .and_then(|end| end.checked_next_multiple_of(WINDOW))
        .and_then(|rounded| rounded.checked_sub(start));
    let (loose_cached, overlaps, overlap_count) = loose_len.map_or((false, Vec::new(), 0), |len| {
        let map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
        let covered = covering_loose_key(&map, start, len).is_some();
        let end = start + len;
        let mut overlaps = Vec::new();
        let mut count = 0;
        for (&(other, bytes), shadow) in map.iter() {
            if other < end && other.saturating_add(bytes) > start {
                count += 1;
                if overlaps.len() < 8 {
                    overlaps.push((other, bytes, shadow.used(), shadow.required_at(), shadow.quiescent(), Arc::strong_count(shadow), shadow.created_frame, shadow.created_at));
                }
            }
        }
        (covered, overlaps, count)
    });
    let requested_title = match title_memory(address, len.max(1)) {
        TitleMemory::Live => "live",
        TitleMemory::Uncommitted => "uncommitted",
        TitleMemory::Free => "free",
    };
    let loose_title = match loose_len.map(|len| title_memory(start, len)) {
        Some(TitleMemory::Live) => "live",
        Some(TitleMemory::Uncommitted) => "uncommitted",
        Some(TitleMemory::Free) => "free",
        None => "invalid range",
    };
    let memory = crate::host::video_memory();
    let limit = if priority { root_system_mb() } else { ORDINARY_SYSTEM_MB };
    let loose_bytes = loose_len.unwrap_or(0);
    let reuse = crate::placement::reused_buffer_room(loose_bytes);
    let admission = if end.is_none() || loose_len.is_none() {
        "address range overflow"
    } else if let Some((registered_start, registered_end, cached)) = registered {
        if cached {
            "registered copy changed after lookup"
        } else if matches!(title_memory(registered_start, registered_end - registered_start), TitleMemory::Free) {
            "registered title memory free"
        } else {
            "registered host buffer unavailable or state changed"
        }
    } else if loose_cached {
        "loose copy changed after lookup"
    } else if loose_title == "free" {
        "loose title memory free"
    } else if priority && !required_draw_reserve_room(
        LOOSE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
        REQUIRED_DRAW_PRIORITY_BYTES.load(std::sync::atomic::Ordering::Relaxed),
        loose_bytes,
    ) {
        "priority draw-copy span or aggregate reserve full"
    } else if priority && loose_bytes > WINDOW && overlap_count > 0 {
        "priority multi-window copy overlaps cached loose windows"
    } else if let Some([(local, local_budget), (system, _)]) = memory {
        if system >= limit {
            if priority && root_reuse_only() {
                if reuse.1 {
                    "nonlocal copy budget; reusable range present but allocation unavailable or state changed"
                } else {
                    "nonlocal copy budget; no contiguous reusable write-back range"
                }
            } else {
                "nonlocal copy budget"
            }
        } else if priority && local + ROOT_LOCAL_HEADROOM_MB >= local_budget {
            "local copy headroom"
        } else {
            "loose host buffer unavailable or state changed"
        }
    } else {
        "loose host buffer unavailable or state changed"
    };
    let loose = loose_len.map(|len| format!("{start:#x}+{len:#x}"));
    format!(
        "admission={admission} requested={address:#x}+{len:#x} requested_title={requested_title} registered={registered:?} beyond_64={beyond_64:?} loose={loose:?} loose_cached={loose_cached} overlaps={overlap_count} first_overlaps={overlaps:?} loose_title={loose_title} video_memory_mb={memory:?} priority={priority} copy_budget_mb={limit} loose_bytes={} priority_loose_cap={ROOT_LOOSE_CAP} priority_multi_bytes={} priority_multi_cap={REQUIRED_DRAW_PRIORITY_CAP} reused_largest_bytes={} reused_fits={}",
        LOOSE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
        REQUIRED_DRAW_PRIORITY_BYTES.load(std::sync::atomic::Ordering::Relaxed),
        reuse.0, reuse.1,
    )
}

/// Titles also use memory outside any buffer object as GPU memory (Grand
/// Theft Auto VI's interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches. A window the title has
/// freed is given up ([`reap_free`]); the copies used to live for the
/// whole process, and the menu's were still resident when the world
/// allocated its own.
/// A draw that names a few bytes used to keep a 4 MB window. debug14 held
/// about 2 GB of those, and when the world textures arrived system memory
/// crossed about 5.8 GB and the card's budget collapsed. 256 KB still
/// covers the named bytes. A shader that reads further from them misses
/// until a draw names that address.
const WINDOW: u64 = 256 << 10;
const REQUIRED_DRAW_MAX_SPAN: u64 = 2 << 20;
const REQUIRED_DRAW_PRIORITY_CAP: u64 = 16 << 20;
const REQUIRED_DRAW_RETAIN_CAP: u64 = 32 << 20;

/// Copies of title memory outside buffers, by (start, length).
static LOOSE: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

/// One five-second diagnostic snapshot of cached loose copies. No Arc is
/// cloned, so the census itself cannot make an otherwise releasable copy
/// appear externally held. Callers compare these pointer identities with
/// descriptor-held Shadows only after releasing their descriptor locks.
#[cfg(feature = "diagnostics")]
pub(crate) fn loose_pin_snapshot() -> Vec<(usize, u64)> {
    let frame = crate::queue::presented();
    LOOSE.lock().unwrap_or_else(|e| e.into_inner()).values()
        .filter(|shadow| shadow.used() < frame && shadow.quiescent() && Arc::strong_count(shadow) > 1)
        .map(|shadow| (Arc::as_ptr(shadow) as usize, shadow.len))
        .collect()
}
/// Bytes in `LOOSE`, updated while its lock is held. Root-view admission
/// needs a hard cache-size check even if DXGI reports usage a little late.
static LOOSE_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Multi-window required-draw copies created with priority while ordinary
/// admission was closed. Updated under `LOOSE`'s lock as copies enter/leave.
static REQUIRED_DRAW_PRIORITY_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Failed priority multi-window attempts in the current submission epoch.
/// The same draw buffer can be named thousands of times before a reap can
/// change the placement/overlap state; avoid rescanning the whole loose map
/// and probing the allocator for each repetition.
struct RequiredDrawDenials {
    epoch: u64,
    keys: std::collections::HashSet<(u64, u64)>,
}
static REQUIRED_DRAW_DENIED: std::sync::OnceLock<Mutex<RequiredDrawDenials>> =
    std::sync::OnceLock::new();

fn required_denied(key: (u64, u64), insert: bool) -> bool {
    let epoch = admission_epoch();
    let mut denied = REQUIRED_DRAW_DENIED.get_or_init(|| Mutex::new(RequiredDrawDenials {
        epoch: u64::MAX,
        keys: std::collections::HashSet::new(),
    }))
        .lock().unwrap_or_else(|e| e.into_inner());
    if denied.epoch != epoch {
        denied.epoch = epoch;
        denied.keys.clear();
    }
    if insert {
        if denied.keys.len() < 4096 {
            denied.keys.insert(key);
        }
        true
    } else {
        denied.keys.contains(&key)
    }
}

/// Prefer a copy beginning at the requested window, then a nearby earlier
/// copy that contains it. Buffer views often make two-window copies; the
/// root view of the second window must use that copy too. Looking behind at
/// most 32 entries bounds the cost when a new window has no covering copy.
fn covering_loose_key<V>(map: &BTreeMap<(u64, u64), V>, start: u64, len: u64) -> Option<(u64, u64)> {
    if let Some((&key, _)) = map.range((start, len)..=(start, u64::MAX)).next() {
        return Some(key);
    }
    let end = start.checked_add(len)?;
    map.range(..(start, 0))
        .rev()
        .take(32)
        .map(|(&key, _)| key)
        .find(|&(at, bytes)| at.checked_add(bytes).is_some_and(|covered| covered >= end))
}

#[track_caller]
fn loose(address: u64, end: u64, root: bool, required_draw: bool) -> Option<(Arc<Shadow>, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    let start = address & !(WINDOW - 1);
    let len = end.checked_next_multiple_of(WINDOW)?.checked_sub(start)?;
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    // A buffer view may already cover this window, including from an
    // earlier start. Reuse also keeps GPU writes in the same host copy.
    if let Some(key) = covering_loose_key(&map, start, len) {
        let shadow = &map[&key];
        if shadow.start != start || shadow.len > len {
            use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
            static COVERED: AtomicU64 = AtomicU64::new(0);
            let count = COVERED.fetch_add(1, Relaxed) + 1;
            if count == 1 || count.is_power_of_two() {
                tracing::info!(count, requested = len, existing = shadow.len,
                    earlier_start = shadow.start != start,
                    "root or buffer view reused a covering loose copy");
            }
            if shadow.start != start {
                static EARLIER: AtomicU64 = AtomicU64::new(0);
                let earlier = EARLIER.fetch_add(1, Relaxed) + 1;
                if earlier == 1 || earlier.is_power_of_two() {
                    tracing::info!(earlier, requested = len, existing = shadow.len,
                        "loose copy reused from an earlier window");
                }
            }
        }
        shadow.touch();
        return Some((shadow.clone(), false));
    }
    if required_draw && required_denied((start, len), false) {
        #[cfg(feature = "diagnostics")]
        note_table_required_denial(required_draw, TableUnionDenial::Cached);
        return None;
    }
    if matches!(title_memory(start, len), TitleMemory::Free) {
        #[cfg(feature = "diagnostics")]
        note_table_required_denial(required_draw, TableUnionDenial::TitleFree);
        return None;
    }
    // The priority reserve is for one small root-view window, not an
    // arbitrary multi-window copy. The map lock serializes admissions.
    if root && !(if required_draw {
        required_draw_reserve_room(LOOSE_BYTES.load(Relaxed), REQUIRED_DRAW_PRIORITY_BYTES.load(Relaxed), len)
    } else {
        root_reserve_room(LOOSE_BYTES.load(Relaxed), len)
    }) {
        #[cfg(feature = "diagnostics")]
        {
            let reason = if len > REQUIRED_DRAW_MAX_SPAN {
                TableUnionDenial::InvalidRange
            } else if LOOSE_BYTES.load(Relaxed).checked_add(len)
                .is_none_or(|bytes| bytes > ROOT_LOOSE_CAP)
            {
                TableUnionDenial::RootLooseCap
            } else {
                TableUnionDenial::MultiwindowCap
            };
            note_table_loose_denial(root, required_draw, reason);
        }
        if required_draw {
            required_denied((start, len), true);
        }
        return None;
    }
    // A larger copy and an older smaller copy of the same title pages would
    // have independent GPU writes until copy-back. Reuse a covering copy
    // above, but never create a new overlapping priority copy.
    if required_draw && len > WINDOW && map.iter().any(|(&(other, bytes), _)| {
        other < start.saturating_add(len) && other.saturating_add(bytes) > start
    }) {
        #[cfg(feature = "diagnostics")]
        note_table_required_denial(required_draw, TableUnionDenial::LooseOverlap);
        required_denied((start, len), true);
        return None;
    }
    // debug17 paused streamed textures and the card's budget stayed 7741 MB.
    // System memory still climbed from about 4.7 GB to 5.3 GB as new windows
    // were copied, and the device was removed. A window named again is
    // copied once system memory is back under this.
    let over_budget = !copy_room(root);
    let (buffer, data) = if over_budget {
        if !root || !root_reuse_only() || !root_reuse_room() {
            #[cfg(feature = "diagnostics")]
            {
                let reason = if !root_reuse_only() {
                    TableUnionDenial::ReuseDisabled
                } else {
                    TableUnionDenial::LocalHeadroom
                };
                note_table_loose_denial(root, required_draw, reason);
            }
            if required_draw && root {
                required_denied((start, len), true);
            }
            return None;
        }
        let Some(reused) = host().and_then(|h| h.shared_buffer_reused(len)) else {
            #[cfg(feature = "diagnostics")]
            {
                let reason = if crate::placement::reused_buffer_room(len).1 {
                    TableUnionDenial::ReusedAllocationFailed
                } else {
                    TableUnionDenial::NoReusableSpan
                };
                note_table_loose_denial(root, required_draw, reason);
            }
            if required_draw && root {
                required_denied((start, len), true);
            }
            return None;
        };
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static REUSED: AtomicU64 = AtomicU64::new(0);
        let count = REUSED.fetch_add(1, Relaxed) + 1;
        if count <= 4 || count.is_power_of_two() {
            tracing::info!(count, "root-view window reused an existing write-back range above the nonlocal limit");
        }
        reused
    } else {
        let Some(buffer) = host().and_then(|h| h.shared_buffer(len)) else {
            #[cfg(feature = "diagnostics")]
            note_table_loose_denial(root, required_draw, TableUnionDenial::HostAllocationFailed);
            if required_draw && root {
                required_denied((start, len), true);
            }
            return None;
        };
        buffer
    };
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let shadow = Shadow::new(buffer, data, gpu, start, len);
    if required_draw && root && len > WINDOW {
        shadow.priority_multi.store(true, Relaxed);
    }
    map.insert((start, len), shadow.clone());
    loose_count_added(&shadow);
    changed();
    Some((shadow, true))
}

/// What of `[start, start + len)` the title still has.
enum TitleMemory {
    /// A committed page. The copy stays.
    Live,
    /// Reserved, or otherwise not free, and nothing committed. Kept: the
    /// title may commit it and read the copy it already has.
    Uncommitted,
    /// Every page is free. The copy is waste.
    Free,
}

/// `Live` when the query fails or the span is empty: a copy is kept unless
/// the title has released the whole span.
fn title_memory(start: u64, len: u64) -> TitleMemory {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_FREE, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let Some(end) = start.checked_add(len).filter(|_| len > 0) else {
        return TitleMemory::Live;
    };
    let mut at = start;
    let mut reserved = false;
    while at < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return TitleMemory::Live;
        }
        if info.State == MEM_COMMIT {
            return TitleMemory::Live;
        }
        reserved |= info.State != MEM_FREE;
        let next = (info.BaseAddress as u64).saturating_add(info.RegionSize as u64);
        if next <= at {
            return TitleMemory::Live;
        }
        at = next;
    }
    if reserved { TitleMemory::Uncommitted } else { TitleMemory::Free }
}

/// A bounded null-view diagnostic only needs to know whether its first byte
/// has been released. The regular copy path remains conservative if a query
/// fails or another part of the requested range is still committed.
#[cfg(feature = "diagnostics")]
pub(crate) fn title_address_is_free(address: u64) -> bool {
    matches!(title_memory(address, 1), TitleMemory::Free)
}

/// Copies given up because their title memory was free (count, bytes), then
/// the last scan's live copies and the ones with nothing committed.
static COPY_CENSUS: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// Whether a new loose window still fits in system memory.
///
/// debug18 paused at 4600 MB, with the copy set already at 2.4 GB. System
/// memory then crept to 5.4 GB with that set flat, the budget fell to
/// 6613 MB, and the GPU stopped responding (device reset 0x887A0007) while
/// the card was under that budget. At 3500 MB the same run's copy set was
/// still 1.7 GB. New windows wait until system memory is back under this.
const ORDINARY_SYSTEM_MB: f64 = 3500.0;
// RX 5700 XT Series S: world mesh root views begin at 3515 MB of nonlocal
// DXGI usage, with 4 GB of local budget and 8 GB of physical RAM still free.
// These are 256 KiB windows, so 250 MB above the ordinary threshold admits
// up to about 1000 otherwise-dropped windows while staying well below the
// earlier 5.4 GB nonlocal device-reset observation.
const ROOT_SYSTEM_MB: f64 = 3750.0;
const ROOT_LOCAL_HEADROOM_MB: f64 = 512.0;
const ROOT_LOOSE_CAP: u64 = LOOSE_CAP + (256 << 20);

/// Opt-in diagnostic headroom for root copies. The ordinary copy limit and
/// default root limit stay at their tested values; a higher cap needs a
/// bounded offline run because DXGI nonlocal budget alone did not predict
/// earlier device removals.
fn root_system_mb() -> f64 {
    static LIMIT: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        let Ok(raw) = std::env::var("VICTORY_ROOT_SYSTEM_MB") else {
            return ROOT_SYSTEM_MB;
        };
        match raw.parse::<f64>() {
            Ok(mb) if (ROOT_SYSTEM_MB..=5000.0).contains(&mb) => {
                tracing::info!(mb, "root-copy nonlocal admission limit overridden");
                mb
            }
            _ => {
                tracing::warn!(value = %raw, "VICTORY_ROOT_SYSTEM_MB must be 3750..=5000; using default");
                ROOT_SYSTEM_MB
            }
        }
    })
}

fn root_reserve_room(current: u64, len: u64) -> bool {
    len == WINDOW && current.checked_add(len).is_some_and(|bytes| bytes <= ROOT_LOOSE_CAP)
}

fn required_draw_reserve_room(current: u64, special: u64, len: u64) -> bool {
    len > 0
        && len <= REQUIRED_DRAW_MAX_SPAN
        && current.checked_add(len).is_some_and(|bytes| bytes <= ROOT_LOOSE_CAP)
        && (len == WINDOW || special.checked_add(len).is_some_and(|bytes| bytes <= REQUIRED_DRAW_PRIORITY_CAP))
}

fn loose_count_removed(shadow: &Shadow) {
    use std::sync::atomic::Ordering::Relaxed;
    LOOSE_BYTES.fetch_sub(shadow.len, Relaxed);
    if shadow.priority_multi.load(Relaxed) {
        REQUIRED_DRAW_PRIORITY_BYTES.fetch_sub(shadow.len, Relaxed);
    }
}

fn loose_count_added(shadow: &Shadow) {
    use std::sync::atomic::Ordering::Relaxed;
    LOOSE_BYTES.fetch_add(shadow.len, Relaxed);
    if shadow.priority_multi.load(Relaxed) {
        REQUIRED_DRAW_PRIORITY_BYTES.fetch_add(shadow.len, Relaxed);
    }
}

fn copy_room_at(memory: Option<[(f64, f64); 2]>, root: bool) -> bool {
    memory.is_none_or(|[(local, local_budget), (system, _)]| {
        let limit = if root { root_system_mb() } else { ORDINARY_SYSTEM_MB };
        system < limit && (!root || local + ROOT_LOCAL_HEADROOM_MB < local_budget)
    })
}

/// The loader can give up old copies at the next safe frame boundary.
/// World draws can ask for thousands of distinct
/// loose windows between those boundaries; querying both DXGI segments for
/// each denied window holds the loose-cache lock and adds work to every
/// dropped draw. A recovered external budget is retried at that boundary.
/// Successful admissions are never cached because allocations can consume
/// the remaining budget within the same frame.
struct CopyRoomGate {
    epoch: std::sync::atomic::AtomicU64,
    denied: [std::sync::atomic::AtomicU64; 2],
}

impl CopyRoomGate {
    const fn new() -> Self {
        Self {
            epoch: std::sync::atomic::AtomicU64::new(0),
            denied: [const { std::sync::atomic::AtomicU64::new(u64::MAX) }; 2],
        }
    }

    fn allowed(&self, root: bool, query: impl FnOnce() -> Option<[(f64, f64); 2]>) -> bool {
        use std::sync::atomic::Ordering::{Acquire, Release};
        let epoch = self.epoch.load(Acquire);
        let denied = &self.denied[usize::from(root)];
        if denied.load(Acquire) == epoch {
            #[cfg(feature = "diagnostics")]
            if root {
                COPY_ROOM_STATS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return false;
        }
        #[cfg(feature = "diagnostics")]
        if root {
            COPY_ROOM_STATS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let allowed = copy_room_at(query(), root);
        if !allowed {
            denied.store(epoch, Release);
        }
        allowed
    }

    fn after_reap(&self) {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    fn denied_current(&self, root: bool) -> bool {
        use std::sync::atomic::Ordering::Acquire;
        self.denied[usize::from(root)].load(Acquire) == self.epoch.load(Acquire)
    }
}

static COPY_ROOM: CopyRoomGate = CopyRoomGate::new();

/// Changes after the submission-boundary reap. A failed descriptor may
/// attempt to make its host buffer again once the copy budget or free ranges
/// could have changed, rather than staying null for its unchanged title
/// record or retrying on every draw.
pub(crate) fn admission_epoch() -> u64 {
    #[cfg(test)]
    if let Some(epoch) = TEST_ADMISSION_EPOCH.with(std::cell::Cell::get) {
        return epoch;
    }
    COPY_ROOM.epoch()
}

/// A failed loose buffer view is worth retrying only when the ordinary
/// admission gate actually denied a live, uncached range. Invalid view
/// shapes and already-copied buffers must not enter an endless retry list.
pub(crate) fn retryable_ordinary_view(address: u64, len: u64) -> bool {
    #[cfg(test)]
    let denied = TEST_VIDEO_MEMORY.with(std::cell::Cell::get)
        .map_or_else(|| COPY_ROOM.denied_current(false), |memory| !copy_room_at(Some(memory), false));
    #[cfg(not(test))]
    let denied = COPY_ROOM.denied_current(false);
    if len == 0 || !denied {
        return false;
    }
    // `title_memory` deliberately treats a failed VirtualQuery as live so
    // normal reaping never releases a possibly valid copy. A retry queue
    // has the opposite requirement: do not rescan an invalid address (such
    // as a null CBV) every frame under pressure.
    // SAFETY: MEMORY_BASIC_INFORMATION is an output-only plain data struct;
    // zero is a valid initial byte pattern before VirtualQuery fills it.
    let mut info: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live writable buffer of the size supplied, and
    // VirtualQuery probes the address without dereferencing it in Rust.
    let queried = unsafe {
        windows_sys::Win32::System::Memory::VirtualQuery(
            address as *const _, &mut info, std::mem::size_of_val(&info),
        )
    };
    if queried == 0 || info.State == windows_sys::Win32::System::Memory::MEM_FREE {
        return false;
    }
    let Some(end) = address.checked_add(len) else {
        return false;
    };
    let map = buffers();
    if map.range(..=(address, usize::MAX)).rev().take(64).any(|(_, entry)| entry.end >= end) {
        return false;
    }
    drop(map);
    let start = address & !(WINDOW - 1);
    let Some(bytes) = end.checked_next_multiple_of(WINDOW).and_then(|end| end.checked_sub(start)) else {
        return false;
    };
    if covering_loose_key(&LOOSE.lock().unwrap_or_else(|e| e.into_inner()), start, bytes).is_some() {
        return false;
    }
    !matches!(title_memory(start, bytes), TitleMemory::Free)
}

/// One DXGI check per submission epoch before retrying failed descriptors;
/// a denied epoch remains a cheap atomic read for the other heaps.
pub(crate) fn ordinary_room_for_retry() -> bool {
    copy_room(false)
}

pub(crate) fn ordinary_admission_denied() -> bool {
    #[cfg(test)]
    if let Some(memory) = TEST_VIDEO_MEMORY.with(std::cell::Cell::get) {
        return !copy_room_at(Some(memory), false);
    }
    COPY_ROOM.denied_current(false)
}

#[cfg(test)]
thread_local! {
    static TEST_VIDEO_MEMORY: std::cell::Cell<Option<[(f64, f64); 2]>> = const { std::cell::Cell::new(None) };
    static TEST_ADMISSION_EPOCH: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Exercises the ordinary and priority lookup paths on one test thread
/// without changing the process-wide admission gate used by other tests.
#[cfg(test)]
pub(crate) fn with_test_video_memory<R>(memory: [(f64, f64); 2], run: impl FnOnce() -> R) -> R {
    struct Restore(Option<[(f64, f64); 2]>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_VIDEO_MEMORY.with(|test| test.set(self.0));
        }
    }
    let previous = TEST_VIDEO_MEMORY.with(|test| test.replace(Some(memory)));
    let _restore = Restore(previous);
    run()
}

#[cfg(test)]
pub(crate) fn with_test_admission_epoch<R>(epoch: u64, run: impl FnOnce() -> R) -> R {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_ADMISSION_EPOCH.with(|test| test.set(self.0));
        }
    }
    let previous = TEST_ADMISSION_EPOCH.with(|test| test.replace(Some(epoch)));
    let _restore = Restore(previous);
    run()
}
/// Root admissions that queried DXGI and that reused a known denial.
#[cfg(feature = "diagnostics")]
static COPY_ROOM_STATS: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

fn copy_room(root: bool) -> bool {
    #[cfg(test)]
    if let Some(memory) = TEST_VIDEO_MEMORY.with(std::cell::Cell::get) {
        return copy_room_at(Some(memory), root);
    }
    let open = COPY_ROOM.allowed(root, crate::host::video_memory);
    if !open {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::info!(
                root,
                ordinary_mb = ORDINARY_SYSTEM_MB,
                root_mb = root_system_mb(),
                root_local_headroom_mb = ROOT_LOCAL_HEADROOM_MB,
                "loose copies paused by the nonlocal or local video-memory budget"
            );
        });
    }
    open
}

/// Loose windows kept on the card. Past this, the oldest ones a draw did
/// not name this frame are given up. debug13 stayed under the card's budget
/// while copies were about 3.3 GB and system-memory use about 5.1 GB, and
/// the budget collapsed once system memory crossed about 5.8 GB. The
/// registered buffers are about 1.1 GB of that and stay; the rest is these
/// windows, which the title was not freeing.
const LOOSE_CAP: u64 = 1536 << 20;

/// How many copies one frame may ask the kernel about. Asking about every
/// copy (about 3,400 while the menu is up) took most of a frame and held
/// the title near 1 fps.
const REAP_LOOK: usize = 64;

/// Gives up host copies the title has freed, and loose windows past
/// [`LOOSE_CAP`].
///
/// A copy a list still has a declared write on stays until that write is
/// finished. One a descriptor or a list still holds is put back, so the
/// next draw does not build a second copy of the same bytes. Slots viewing
/// a copy given up are left null; [`find`] makes one when a draw names the
/// address.
pub(crate) fn reap_free() {
    let mut trimmed = trim_loose();
    // This runs at the start of a host submission, after the preceding GPU
    // work finished. Page tables can safely release old copies here; doing
    // this from a recording thread's root-view lookup cannot guarantee that.
    trimmed.extend(trim_loose_pressure());
    let (chosen, open_n, open_b) = scan_freed();
    let mut gone = take_freed(chosen);
    let freed_bytes: u64 = gone.iter().map(|s| s.len).sum();
    if freed_bytes >= 1 << 20 {
        tracing::info!(
            copies = gone.len(),
            mb = format_args!("{:.1}", freed_bytes as f64 / 1e6),
            "copies of freed title memory given up"
        );
    }
    gone.extend(trimmed);
    {
        use std::sync::atomic::Ordering::Relaxed;
        let (live_n, live_b) = resident_copies();
        let bytes: u64 = gone.iter().map(|s| s.len).sum();
        COPY_CENSUS[0].fetch_add(gone.len() as u64, Relaxed);
        COPY_CENSUS[1].fetch_add(bytes, Relaxed);
        COPY_CENSUS[2].store(live_n, Relaxed);
        COPY_CENSUS[3].store(live_b, Relaxed);
        COPY_CENSUS[4].store(open_n, Relaxed);
        COPY_CENSUS[5].store(open_b, Relaxed);
    }
    release_copies(gone);
    COPY_ROOM.after_reap();
}

/// Copies still cached, and their bytes. No query: the free scan is what
/// tells a reserved window from a live one, a few dozen a frame.
fn resident_copies() -> (u64, u64) {
    let loose = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut n, mut bytes) = (loose.len() as u64, loose.values().map(|s| s.len).sum::<u64>());
    drop(loose);
    for shadow in buffers().values().filter_map(|e| e.shadow.as_ref()) {
        n += 1;
        bytes += shadow.len;
    }
    (n, bytes)
}

/// Optional pressure trim at the safe submission boundary. Start before the
/// root-copy limit so released write-back spans are reusable by the first
/// root views after that limit is reached. Admission itself keeps its limit.
fn trim_loose_pressure() -> Vec<Arc<Shadow>> {
    let Some(target) = root_pressure_trim_target() else {
        return Vec::new();
    };
    let total = LOOSE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    if total <= target {
        return Vec::new();
    }
    // Thousands of root views can be recorded between presents. Check DXGI
    // and sort the loose cache at most once a second.
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let now = std::time::Instant::now();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|at| now.duration_since(at) < std::time::Duration::from_secs(1)) {
        return Vec::new();
    }
    *last = Some(now);
    drop(last);
    if !pressure_trim_needed(total, target, crate::host::video_memory()) {
        return Vec::new();
    }
    let gone = trim_loose_to(target, true);
    if gone.is_empty() {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static NO_PROGRESS: AtomicU64 = AtomicU64::new(0);
        let attempts = NO_PROGRESS.fetch_add(1, Relaxed) + 1;
        if attempts <= 4 || attempts.is_power_of_two() {
            let frame = crate::queue::presented();
            let map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
            let (mut old, mut old_quiescent, mut externally_held) = (0u64, 0u64, 0u64);
            for shadow in map.values() {
                if shadow.used() >= frame { continue; }
                old += 1;
                if shadow.quiescent() {
                    old_quiescent += 1;
                    if Arc::strong_count(shadow) > 1 { externally_held += 1; }
                }
            }
            tracing::info!(attempts, cached_mb = total >> 20, target_mb = target >> 20,
                copies = map.len(), old, old_quiescent, externally_held,
                "root-view pressure trim found no releasable loose copies");
        }
    }
    gone
}

fn root_pressure_trim_target() -> Option<u64> {
    static TARGET: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *TARGET.get_or_init(|| {
        let Ok(raw) = std::env::var("VICTORY_ROOT_PRESSURE_TRIM_MB") else {
            return None;
        };
        match raw.parse::<u64>() {
            Ok(mb @ 256..=1536) => {
                tracing::info!(mb, "root-view pressure trim enabled");
                Some(mb << 20)
            }
            _ => {
                tracing::warn!(value = %raw, "VICTORY_ROOT_PRESSURE_TRIM_MB must be 256..=1536; pressure trim disabled");
                None
            }
        }
    })
}

fn root_reuse_only() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_ROOT_REUSE_ONLY").is_ok_and(|value| value == "1");
        if on {
            tracing::info!("root-view reuse of old write-back ranges enabled");
        }
        on
    })
}

/// Reuse may bypass only the nonlocal copy limit. The local headroom guard
/// remains mandatory even though the replacement buffer is in system memory.
fn root_reuse_room() -> bool {
    #[cfg(test)]
    if let Some(memory) = TEST_VIDEO_MEMORY.with(std::cell::Cell::get) {
        return root_reuse_room_at(Some(memory));
    }
    root_reuse_room_at(crate::host::video_memory())
}

fn root_reuse_room_at(memory: Option<[(f64, f64); 2]>) -> bool {
    memory.is_some_and(|[(local, budget), (system, _)]| {
        system >= root_system_mb() && local + ROOT_LOCAL_HEADROOM_MB < budget
    })
}

// In the 120-second RX 5700 XT world run, nonlocal usage rose from 4623 MB
// to 4759 MB between safe-boundary samples. Waiting until the 4750 MB
// admission limit caused 21 root misses while the first trim was running.
const ROOT_PRESSURE_TRIM_LEAD_MB: f64 = 128.0;

fn pressure_trim_needed(total: u64, target: u64, memory: Option<[(f64, f64); 2]>) -> bool {
    total > target && memory.is_some_and(|[_, (system, _)]| {
        system >= root_system_mb() - ROOT_PRESSURE_TRIM_LEAD_MB
    })
}

/// Oldest loose windows past `target`, already removed. A window named this
/// frame stays. One something else still holds is put back.
fn trim_loose_to(target: u64, pressure: bool) -> Vec<Arc<Shadow>> {
    let mut skipped = std::collections::HashSet::new();
    let mut gone = Vec::new();
    // A window held by a list or descriptor must stay, but it must not use
    // up the target's candidate bytes on every attempt. Only the opt-in
    // pressure path looks past those held windows; the ordinary trim still
    // performs its original single pass.
    let passes = if pressure { 8 } else { 1 };
    for _ in 0..passes {
        let (freed, held) = trim_loose_pass(target, pressure, &skipped);
        gone.extend(freed);
        if !pressure || held.is_empty() || LOOSE_BYTES.load(std::sync::atomic::Ordering::Relaxed) <= target {
            break;
        }
        skipped.extend(held);
    }
    if !gone.is_empty() {
        let bytes: u64 = gone.iter().map(|s| s.len).sum();
        #[cfg(feature = "diagnostics")]
        for shadow in gone.iter().filter(|s| s.len > WINDOW) {
            use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
            static LARGE_EVICTIONS: AtomicU64 = AtomicU64::new(0);
            let count = LARGE_EVICTIONS.fetch_add(1, Relaxed) + 1;
            if count <= 32 || count.is_power_of_two() {
                tracing::info!(count, start = format_args!("{:#x}", shadow.start), len = shadow.len,
                    used = shadow.used(), required_at = shadow.required_at(),
                    priority_multi = shadow.priority_multi.load(Relaxed),
                    "large loose copy given up by trim");
            }
        }
        if pressure {
            tracing::info!(
                copies = gone.len(),
                mb = format_args!("{:.1}", bytes as f64 / 1e6),
                target_mb = target >> 20,
                "loose copies given up under nonlocal pressure"
            );
        } else if bytes >= 1 << 20 {
            tracing::info!(
                copies = gone.len(),
                mb = format_args!("{:.1}", bytes as f64 / 1e6),
                "loose copies given up to stay within the write-back budget"
            );
        }
    }
    gone
}

/// Picks old, quiescent windows up to the requested bytes. `skipped` holds
/// windows a previous pass found still referenced by a list or descriptor.
fn select_trim_candidates<T>(
    ranked: impl IntoIterator<Item = T>,
    total: u64,
    target: u64,
    frame: u64,
    skipped: &std::collections::HashSet<(u64, u64)>,
    key_and_used: impl Fn(&T) -> ((u64, u64), u64),
    quiescent: impl Fn(&T) -> bool,
) -> Vec<T> {
    let mut over = total.saturating_sub(target);
    let mut chosen = Vec::new();
    for candidate in ranked {
        if over == 0 {
            break;
        }
        let (key, used) = key_and_used(&candidate);
        if skipped.contains(&key) || used >= frame || !quiescent(&candidate) {
            continue;
        }
        over = over.saturating_sub(key.1);
        chosen.push(candidate);
    }
    chosen
}

/// Protect the newest required large draw copies within a fixed byte cap.
/// The cap bounds memory retained ahead of ordinary loose windows even if
/// the title streams thousands of distinct vertex buffers.
fn required_retention(
    entries: impl IntoIterator<Item = ((u64, u64), u64)>,
    cap: u64,
) -> std::collections::HashSet<(u64, u64)> {
    let mut ranked: Vec<_> = entries.into_iter().filter(|(_, at)| *at > 0).collect();
    ranked.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.1));
    let mut left = cap;
    let mut keep = std::collections::HashSet::new();
    for (key, _) in ranked {
        if key.1 <= left {
            left -= key.1;
            keep.insert(key);
        }
    }
    keep
}

/// One selection and hold check. Returns the windows truly evicted and the
/// keys that had to be put back, so a pressure trim can try younger windows.
fn trim_loose_pass(
    target: u64,
    pressure: bool,
    skipped: &std::collections::HashSet<(u64, u64)>,
) -> (Vec<Arc<Shadow>>, Vec<(u64, u64)>) {
    use std::sync::atomic::Ordering::Relaxed;
    // Most submissions are below the cap. Avoid walking and sorting every
    // loose window for each of the world's many submissions per frame.
    if LOOSE_BYTES.load(Relaxed) <= target {
        return (Vec::new(), Vec::new());
    }
    let frame = crate::queue::presented();
    let chosen = {
        let map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
        let total = LOOSE_BYTES.load(Relaxed);
        if total <= target {
            return (Vec::new(), Vec::new());
        }
        let mut ranked: Vec<Arc<Shadow>> = map.values().cloned().collect();
        drop(map);
        ranked.sort_by_key(|s| s.used());
        let protected = if pressure {
            required_retention(
                ranked.iter().filter(|s| s.len > WINDOW)
                    .map(|s| ((s.start, s.len), s.required_at())),
                REQUIRED_DRAW_RETAIN_CAP,
            )
        } else {
            std::collections::HashSet::new()
        };
        #[cfg(feature = "diagnostics")]
        if !protected.is_empty() {
            use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
            static RETAIN_PASSES: AtomicU64 = AtomicU64::new(0);
            let passes = RETAIN_PASSES.fetch_add(1, Relaxed) + 1;
            if passes <= 4 || passes.is_power_of_two() {
                tracing::info!(passes, copies = protected.len(), bytes = protected.iter().map(|key| key.1).sum::<u64>(),
                    cap_bytes = REQUIRED_DRAW_RETAIN_CAP, "required large draw copies protected from pressure trim");
            }
        }
        select_trim_candidates(
            ranked.into_iter().filter(|s| !protected.contains(&(s.start, s.len))),
            total, target, frame, skipped,
            |shadow| ((shadow.start, shadow.len), shadow.used()),
            |shadow| shadow.quiescent(),
        )
    };
    if chosen.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut gone = Vec::new();
    {
        let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
        for shadow in chosen {
            let key = (shadow.start, shadow.len);
            // Recording threads may have named a candidate after ranking.
            // Check its current use and pending writes while the map entry
            // still belongs to this copy.
            if map.get(&key).is_some_and(|c| {
                Arc::ptr_eq(c, &shadow) && shadow.used() < frame && shadow.quiescent()
            }) {
                map.remove(&key);
                loose_count_removed(&shadow);
                gone.push(shadow);
            }
        }
    }
    if gone.is_empty() {
        return (Vec::new(), Vec::new());
    }
    // Page tables drop their reference before the hold check. A list or a
    // descriptor that still has the copy wins: it goes back, one copy.
    changed();
    crate::memory::drop_gone();
    let mut put_back = Vec::new();
    let mut held = Vec::new();
    gone.retain(|shadow| {
        if Arc::strong_count(shadow) > 1 {
            held.push((shadow.start, shadow.len));
            put_back.push(shadow.clone());
            false
        } else {
            true
        }
    });
    if !put_back.is_empty() {
        let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
        for shadow in put_back {
            if let std::collections::btree_map::Entry::Vacant(entry) = map.entry((shadow.start, shadow.len)) {
                loose_count_added(&shadow);
                entry.insert(shadow);
            }
        }
        drop(map);
        changed();
        crate::memory::drop_gone();
    }
    (gone, held)
}

/// The ordinary loose-copy cap is unchanged when pressure trimming is off.
fn trim_loose() -> Vec<Arc<Shadow>> {
    trim_loose_to(LOOSE_CAP, false)
}

/// A slice of the caches, rotated, so a frame does not query every copy.
/// `open` is how much of that slice has nothing committed.
fn scan_freed() -> (Vec<Arc<Shadow>>, u64, u64) {
    let loose_keys: Vec<(u64, u64)> = LOOSE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect();
    let registered_keys: Vec<(u64, usize)> = buffers()
        .iter()
        .filter(|(_, e)| e.shadow.is_some())
        .map(|(k, _)| *k)
        .collect();
    let total = loose_keys.len() + registered_keys.len();
    if total == 0 {
        return (Vec::new(), 0, 0);
    }
    static CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let start = CURSOR.load(std::sync::atomic::Ordering::Relaxed) % total;
    let mut chosen = Vec::new();
    let (mut open_n, mut open_b) = (0u64, 0u64);
    let mut looked = 0usize;
    while looked < total && looked < REAP_LOOK {
        let index = (start + looked) % total;
        looked += 1;
        let shadow = if index < loose_keys.len() {
            let key = loose_keys[index];
            LOOSE.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned()
        } else {
            let key = registered_keys[index - loose_keys.len()];
            buffers().get(&key).and_then(|e| e.shadow.clone())
        };
        let Some(shadow) = shadow else {
            continue;
        };
        match title_memory(shadow.start, shadow.len) {
            TitleMemory::Free if shadow.quiescent() => chosen.push(shadow),
            TitleMemory::Uncommitted => {
                open_n += 1;
                open_b += shadow.len;
            }
            _ => {}
        }
    }
    CURSOR.store((start + looked) % total, std::sync::atomic::Ordering::Relaxed);
    (chosen, open_n, open_b)
}

/// Nulls views of `gone`, drops them from the page tables, and frees the
/// write-back ranges nothing else holds.
fn release_copies(gone: Vec<Arc<Shadow>>) {
    if gone.is_empty() {
        return;
    }
    let raws: std::collections::HashSet<usize> = gone
        .iter()
        .map(|s| windows::core::Interface::as_raw(&s.buffer) as usize)
        .collect();
    crate::descriptor::evict_buffers(&raws);
    changed();
    // Tables drop their view of the copy before the submission reads them.
    // Lists that already named it keep their own reference until they reset.
    crate::memory::drop_gone();
    drop(gone);
    crate::placement::reap_writeback();
}

/// `chosen` still free and still the cached copy: removed. A registered
/// buffer's entry stays, with no copy, until the title releases the buffer.
fn take_freed(chosen: Vec<Arc<Shadow>>) -> Vec<Arc<Shadow>> {
    let mut left = chosen;
    let mut gone = Vec::new();
    {
        let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
        left.retain(|shadow| {
            let key = (shadow.start, shadow.len);
            let same = map.get(&key).is_some_and(|c| Arc::ptr_eq(c, shadow));
            if same && shadow.quiescent() && matches!(title_memory(shadow.start, shadow.len), TitleMemory::Free) {
                map.remove(&key);
                loose_count_removed(shadow);
                gone.push(shadow.clone());
                false
            } else {
                true
            }
        });
    }
    let mut map = buffers();
    for shadow in left {
        if !(shadow.quiescent() && matches!(title_memory(shadow.start, shadow.len), TitleMemory::Free)) {
            continue;
        }
        let keys: Vec<(u64, usize)> = map
            .iter()
            .filter(|(_, e)| e.shadow.as_ref().is_some_and(|c| Arc::ptr_eq(c, &shadow)))
            .map(|(k, _)| *k)
            .collect();
        if keys.is_empty() {
            continue;
        }
        for key in keys {
            if let Some(entry) = map.get_mut(&key) {
                entry.shadow = None;
            }
        }
        gone.push(shadow);
    }
    gone
}

/// The copy a buffer view of `len` bytes at `address`, whose elements are
/// `element` bytes, reads: the innermost title buffer's ([`find`]), unless
/// the view does not start on an element of that copy. A host view counts
/// its first element from its buffer's start; a console view starts at any
/// address, and the title places structured views at ring positions inside
/// its big GPU heap (the meshlet culling's 456-byte constants, the colour
/// bakes' parameter blocks: refused, their slots read zeros, every meshlet
/// culled). Such a view gets a copy of its own bytes, starting at its
/// address (`true` with it), which lives while a heap slot made from the
/// view or a list holds it.
#[track_caller]
pub(crate) fn for_view(address: u64, len: u64, element: u64) -> Option<(Arc<Shadow>, bool)> {
    let shadow = find(address, len)?;
    if element == 0 || shadow.offset(address).is_multiple_of(element) {
        return Some((shadow, false));
    }
    #[cfg(feature = "diagnostics")]
    VIEWS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    exact(address, len, element).map(|s| (s, true))
}

/// Asks for, and copies made of, views off their copy's elements, the
/// copies' bytes and the largest (diagnostics).
#[cfg(feature = "diagnostics")]
static VIEWS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Logs [`VIEWS`] per frame over `frames`.
#[cfg(feature = "diagnostics")]
pub(crate) fn log_views(frames: f64) {
    use std::sync::atomic::Ordering::Relaxed;
    let [asks, made, bytes, largest] = [0, 1, 2, 3].map(|i| VIEWS[i].swap(0, Relaxed));
    let live = EXACT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .1
        .values()
        .filter(|w| w.strong_count() > 0)
        .count();
    let (chunks, held) = {
        let a = arena();
        // SAFETY: live buffers.
        let size = |c: &(ID3D12Resource, usize, u64, BTreeMap<u64, u64>)| unsafe { c.0.GetDesc() }.Width;
        let held: u64 = a.chunks.iter().map(|c| size(c) - c.3.values().sum::<u64>()).sum();
        (a.chunks.len(), held)
    };
    if asks > 0 {
        tracing::info!(
            "buffer views off their copy's elements: {:.0} asked for/frame, {:.0} copies made/frame ({:.1} KB), largest {largest} bytes, {live} live in {chunks} shared buffers ({:.1} MB held)",
            asks as f64 / frames,
            made as f64 / frames,
            bytes as f64 / frames / 1e3,
            held as f64 / 1e6
        );
    }
    let freed_n = COPY_CENSUS[0].swap(0, Relaxed);
    let freed_b = COPY_CENSUS[1].swap(0, Relaxed);
    let [live_n, live_b, open_n, open_b] = [2, 3, 4, 5].map(|i| COPY_CENSUS[i].load(Relaxed));
    if live_n + open_n + freed_n > 0 {
        tracing::info!(
            "title-memory copies: {live_n} live ({:.0} MB), {open_n} with nothing committed ({:.0} MB); gave up {freed_n} whose memory was freed ({:.0} MB)",
            live_b as f64 / 1e6,
            open_b as f64 / 1e6,
            freed_b as f64 / 1e6
        );
    }
    let queried = COPY_ROOM_STATS[0].swap(0, Relaxed);
    let skipped = COPY_ROOM_STATS[1].swap(0, Relaxed);
    if queried + skipped > 0 {
        tracing::info!(queried, skipped, "root-copy nonlocal admission checks");
    }
}

/// Copies of single views' bytes ([`for_view`]), by (start, length), held
/// by the heap slots made from them and the lists that use them: a copy is
/// found again for as long as a slot shows it, so draws refresh the copy
/// the GPU reads (a capped cache dropped copies slots still showed, and a
/// new copy of the same bytes took the refreshes). With the size the map
/// is next pruned of dead entries at.
#[allow(clippy::type_complexity)]
static EXACT: Mutex<(usize, BTreeMap<(u64, u64, u64), std::sync::Weak<Shadow>>)> = Mutex::new((0, BTreeMap::new()));

fn exact(address: u64, len: u64, element: u64) -> Option<Arc<Shadow>> {
    let mut guard = EXACT.lock().unwrap_or_else(|e| e.into_inner());
    let (prune_at, map) = &mut *guard;
    if let Some(shadow) = map.get(&(address, len, element)).and_then(std::sync::Weak::upgrade) {
        return Some(shadow);
    }
    // Ring positions change every frame: entries die by the hundred.
    if map.len() >= *prune_at {
        map.retain(|_, w| w.strong_count() > 0);
        *prune_at = (2 * map.len()).max(1024);
    }
    // A run of a shared buffer, its start a whole number of elements in.
    let (buffer, data, gpu, slice) = arena().take(len, element)?;
    let shadow = Shadow::new_at(buffer, data, gpu, address, len, Some(slice));
    map.insert((address, len, element), Arc::downgrade(&shadow));
    #[cfg(feature = "diagnostics")]
    {
        use std::sync::atomic::Ordering::Relaxed;
        VIEWS[1].fetch_add(1, Relaxed);
        VIEWS[2].fetch_add(len, Relaxed);
        VIEWS[3].fetch_max(len, Relaxed);
    }
    Some(shadow)
}

/// A run of an [`Arena`] buffer: the buffer, where the run starts, its
/// bytes.
#[derive(Clone, Copy)]
struct Slice {
    chunk: usize,
    offset: u64,
    len: u64,
}

/// Shared buffers for the copies of single views ([`exact`]): they are
/// small (~2 KB on average) and come and go by the hundred a frame, and a
/// buffer of their own each cost a kernel call to make and one to free on
/// the title's threads and the submitting one (300-820 a frame), and 64 KiB
/// of memory each (~5,000 live).
#[derive(Default)]
struct Arena {
    /// Each buffer, its mapping and host address, and its free runs
    /// (offset → bytes).
    chunks: Vec<(ID3D12Resource, usize, u64, BTreeMap<u64, u64>)>,
}

/// Bytes of each arena buffer (a larger copy gets one of its size).
const CHUNK: u64 = 16 << 20;

static ARENA: Mutex<Arena> = Mutex::new(Arena { chunks: Vec::new() });

fn arena() -> std::sync::MutexGuard<'static, Arena> {
    ARENA.lock().unwrap_or_else(|e| e.into_inner())
}

impl Arena {
    /// A run of `len` bytes starting at a multiple of `element` (and of 256)
    /// in a shared buffer: the buffer, the run's mapping and host address.
    fn take(&mut self, len: u64, element: u64) -> Option<(ID3D12Resource, *mut u8, u64, Slice)> {
        let gcd = |mut a: u64, mut b: u64| {
            while b != 0 {
                (a, b) = (b, a % b);
            }
            a
        };
        let align = 256 / gcd(256, element.max(1)) * element.max(1);
        let len = len.max(1).next_multiple_of(4);
        let fits = |free: &BTreeMap<u64, u64>| {
            free.iter().find_map(|(&at, &n)| {
                let start = at.next_multiple_of(align);
                (start + len <= at + n).then_some((at, n, start))
            })
        };
        let found = self.chunks.iter().enumerate().find_map(|(c, chunk)| fits(&chunk.3).map(|f| (c, f)));
        let (chunk, (at, n, start)) = match found {
            Some(found) => found,
            None => {
                let size = CHUNK.max((len + align).next_multiple_of(1 << 16));
                let (buffer, data) = host()?.shared_buffer(size)?;
                crate::host::name(&buffer, || format!("title views {size:#x}"));
                // SAFETY: a live buffer.
                let gpu = unsafe { buffer.GetGPUVirtualAddress() };
                let free = BTreeMap::from([(0, size)]);
                self.chunks.push((buffer, data as usize, gpu, free));
                let c = self.chunks.len() - 1;
                (c, fits(&self.chunks[c].3)?)
            }
        };
        let (buffer, data, gpu, free) = &mut self.chunks[chunk];
        free.remove(&at);
        if start > at {
            free.insert(at, start - at);
        }
        if at + n > start + len {
            free.insert(start + len, at + n - start - len);
        }
        let slice = Slice { chunk, offset: start, len };
        Some((buffer.clone(), (*data + start as usize) as *mut u8, *gpu + start, slice))
    }

    /// A run goes back, joined to its free neighbours.
    fn give_back(&mut self, slice: Slice) {
        let Some((_, _, _, free)) = self.chunks.get_mut(slice.chunk) else { return };
        let (mut at, mut n) = (slice.offset, slice.len);
        if let Some((&before, &m)) = free.range(..at).next_back()
            && before + m == at
        {
            free.remove(&before);
            (at, n) = (before, n + m);
        }
        if let Some(m) = free.remove(&(at + n)) {
            n += m;
        }
        free.insert(at, n);
    }
}

/// Every host copy of title memory made so far (buffers and windows).
pub(crate) fn all() -> Vec<Arc<Shadow>> {
    let mut all: Vec<Arc<Shadow>> = buffers().values().filter_map(|e| e.shadow.clone()).collect();
    all.extend(LOOSE.lock().unwrap_or_else(|e| e.into_inner()).values().cloned());
    all
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // These tests write the same process-global GPU mark bitmaps. Running
    // their mark/copy-back sequences at once can send one test's page through
    // the other's withholding policy.
    static BITMAP_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn loose_views_reuse_a_copy_starting_in_an_earlier_window() {
        let mut map = BTreeMap::new();
        let w = WINDOW;
        map.insert((4 * w, 2 * w), ());
        assert_eq!(covering_loose_key(&map, 4 * w, w), Some((4 * w, 2 * w)));
        assert_eq!(covering_loose_key(&map, 5 * w, w), Some((4 * w, 2 * w)));
        assert_eq!(covering_loose_key(&map, 6 * w, w), None);
        map.insert((5 * w, w), ());
        assert_eq!(covering_loose_key(&map, 5 * w, w), Some((5 * w, w)));
        assert_eq!(covering_loose_key(&map, 5 * w, 2 * w), None);
        map.insert((4 * w, 4 * w), ());
        assert_eq!(covering_loose_key(&map, 5 * w, 2 * w), Some((4 * w, 4 * w)));
    }

    #[test]
    fn pressure_trim_starts_before_root_admission_closes_and_only_above_target() {
        let target = 750 << 20;
        let memory = |system| Some([(4334.0, 7741.0), (system, 16333.0)]);
        let lead = root_system_mb() - ROOT_PRESSURE_TRIM_LEAD_MB;
        assert!(!pressure_trim_needed(894 << 20, target, memory(lead - 1.0)));
        assert!(pressure_trim_needed(894 << 20, target, memory(lead)));
        assert!(pressure_trim_needed(894 << 20, target, memory(root_system_mb())));
        assert!(!pressure_trim_needed(target, target, memory(root_system_mb() + 100.0)));
        assert!(!pressure_trim_needed(894 << 20, target, None));
    }

    #[test]
    fn reused_root_range_keeps_local_headroom_and_requires_nonlocal_pressure() {
        let memory = |local, system| Some([(local, 7741.0), (system, 16333.0)]);
        assert!(!root_reuse_room_at(None));
        assert!(!root_reuse_room_at(memory(4000.0, root_system_mb() - 1.0)));
        assert!(root_reuse_room_at(memory(4000.0, root_system_mb())));
        assert!(!root_reuse_room_at(memory(7741.0 - ROOT_LOCAL_HEADROOM_MB, root_system_mb())));
    }

    #[test]
    fn combined_required_span_rejects_any_registered_partial_overlap() {
        let mut map = BTreeMap::new();
        map.insert((0x10_0000, 1), Entry { end: 0x20_0000, shadow: None });
        // A long earlier registration still counts when newer nested buffers
        // end before the combined span.
        for i in 0..70 {
            let at = 0x11_0000 + i * 0x100;
            map.insert((at, i as usize + 2), Entry { end: at + 0x80, shadow: None });
        }
        assert!(registered_intersects_map(&map, 0x18_0000, 0x1c_0000));
        assert!(registered_intersects_map(&map, 0x1f_0000, 0x21_0000));
        assert!(!registered_intersects_map(&map, 0x20_0000, 0x24_0000));
    }

    #[test]
    fn pressure_trim_retries_after_the_oldest_window_is_held() {
        // The oldest window consumes the entire gap, but a list still holds
        // it. On the next pass it is skipped and the next window is tried.
        let ranked = [(1u64, WINDOW, 0u64), (2, WINDOW, 1), (3, WINDOW, 2)];
        let mut held = std::collections::HashSet::new();
        let first = select_trim_candidates(
            ranked, 3 * WINDOW, 2 * WINDOW, 3, &held,
            |c| ((c.0, c.1), c.2), |_| true,
        );
        assert_eq!(first, vec![ranked[0]]);
        held.insert((ranked[0].0, ranked[0].1));
        let second = select_trim_candidates(
            ranked, 3 * WINDOW, 2 * WINDOW, 3, &held,
            |c| ((c.0, c.1), c.2), |_| true,
        );
        assert_eq!(second, vec![ranked[1]]);
    }

    #[test]
    fn required_draw_retention_keeps_recent_large_copies_within_cap() {
        let mib = 1 << 20;
        let keys = [((1, 2 * mib), 20), ((2, mib), 19), ((3, mib), 18), ((4, mib), 0)];
        let keep = required_retention(keys, 3 * mib);
        assert!(keep.contains(&keys[0].0) && keep.contains(&keys[1].0));
        assert!(!keep.contains(&keys[2].0) && !keep.contains(&keys[3].0));
        assert_eq!(keep.iter().map(|key| key.1).sum::<u64>(), 3 * mib);
        // One oversize old copy does not stop a smaller newer one fitting.
        let keep = required_retention([((5, 4 * mib), 7), ((6, mib), 6)], 2 * mib);
        assert_eq!(keep, std::collections::HashSet::from([(6, mib)]));
    }

    #[test]
    fn required_draw_multiwindow_reserve_is_bounded_separately() {
        assert!(required_draw_reserve_room(LOOSE_CAP, 0, 5 * WINDOW));
        assert!(required_draw_reserve_room(LOOSE_CAP, REQUIRED_DRAW_PRIORITY_CAP - 5 * WINDOW, 5 * WINDOW));
        assert!(!required_draw_reserve_room(LOOSE_CAP, REQUIRED_DRAW_PRIORITY_CAP - 4 * WINDOW, 5 * WINDOW));
        assert!(!required_draw_reserve_room(LOOSE_CAP, 0, REQUIRED_DRAW_MAX_SPAN + WINDOW));
        assert!(!required_draw_reserve_room(ROOT_LOOSE_CAP, 0, WINDOW));
        assert!(required_draw_reserve_room(LOOSE_CAP, REQUIRED_DRAW_PRIORITY_CAP, WINDOW),
            "single root-size windows retain the existing root reserve");
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn table_union_denial_reports_the_original_loose_overlap() {
        if host().is_none() {
            return;
        }
        let layout = std::alloc::Layout::from_size_align((2 * WINDOW) as usize, WINDOW as usize).unwrap();
        // SAFETY: the aligned live title range is intentionally kept while
        // the global loose cache can still reference it after this test.
        let title = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!title.is_null());
        let base = title as u64;
        with_test_video_memory([(1000.0, 8000.0), (3600.0, 16000.0)], || {
            assert!(find_root(base + WINDOW, 1).is_some());
            let (found, reason) = diagnose_required_table_union(base + WINDOW - 256, 512, true);
            assert!(found.is_none(), "a second overlapping copy must fail closed");
            assert!(matches!(reason, Some(TableUnionDenial::LooseOverlap)),
                "report the original overlap, not its subsequent negative-cache hit: {reason:?}");
        });
    }

    #[test]
    fn table_union_charges_only_new_priority_loose_copies() {
        if host().is_none() {
            return;
        }
        let layout = std::alloc::Layout::from_size_align((4 * WINDOW) as usize, WINDOW as usize).unwrap();
        // SAFETY: the live title allocation intentionally outlives loose
        // cache entries which may retain copies after the test returns.
        let title = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!title.is_null());
        let base = title as u64;
        register(base as usize, base, 2 * WINDOW);
        with_test_video_memory([(1000.0, 8000.0), (3600.0, 16000.0)], || {
            let (registered, charged) = find_required_union_with_room(base + WINDOW - 256, 512, false)
                .expect("an existing registered buffer needs no table reserve");
            assert!(!charged);
            assert_eq!(registered.range(), (base, 2 * WINDOW));

            let loose_at = base + 3 * WINDOW - 256;
            assert!(find_required_union_with_room(loose_at, 512, false).is_none(),
                "a new loose copy must respect a full table reserve");
            let (priority, charged) = find_required_union_with_room(loose_at, 512, true)
                .expect("the root reserve can admit this two-window copy");
            assert!(charged);
            assert_eq!(priority.range(), (base + 2 * WINDOW, 2 * WINDOW));
            assert!(!find_required_union_with_room(loose_at, 512, false)
                .expect("an existing priority copy can be reused above the cap").1);
        });
        unregister(base as usize, base);
    }

    #[test]
    fn mesh_root_views_have_a_bounded_nonlocal_reserve() {
        let memory = |local, system| Some([(local, 7741.0), (system, 16333.0)]);
        assert!(!copy_room_at(memory(3706.0, 3515.0), false));
        assert!(copy_room_at(memory(3706.0, 3515.0), true));
        assert!(!copy_room_at(memory(3706.0, ROOT_SYSTEM_MB), true));
        assert!(!copy_room_at(memory(7300.0, 3515.0), true));
        assert!(copy_room_at(None, true));
        assert!(root_reserve_room(ROOT_LOOSE_CAP - WINDOW, WINDOW));
        assert!(!root_reserve_room(ROOT_LOOSE_CAP, WINDOW));
        assert!(!root_reserve_room(LOOSE_CAP, WINDOW * 2));
    }

    #[test]
    fn copy_room_denial_queries_once_per_reap_without_caching_success() {
        use std::cell::Cell;
        let gate = CopyRoomGate::new();
        let queries = Cell::new(0);
        let memory = |system| {
            queries.set(queries.get() + 1);
            Some([(1000.0, 7741.0), (system, 16333.0)])
        };
        assert!(!gate.allowed(true, || memory(50_000.0)));
        assert!(!gate.allowed(true, || memory(0.0)));
        assert_eq!(queries.get(), 1, "denial avoids repeated DXGI queries");
        assert!(gate.allowed(false, || memory(0.0)), "root denial does not block ordinary copies");
        assert!(gate.allowed(false, || memory(0.0)), "success is rechecked as budget can fall");
        assert_eq!(queries.get(), 3);
        gate.after_reap();
        assert!(gate.allowed(true, || memory(0.0)), "safe boundary retries after copies may be released");
        assert_eq!(queries.get(), 4);
    }

    /// A shader's read mark of heap slot `slot` in the bitmap of shaders
    /// that use title memory (`memory`) or of the others.
    pub(crate) fn mark_read(memory: bool, slot: u32) {
        let b = bitmap(memory).expect("a host GPU");
        let bit = victory_shader::emit::VIEW_READS + slot;
        // SAFETY: a bit of the mapped bitmap; no submission runs (the caller
        // holds the submission lock).
        unsafe { *(b.data as *mut u32).add((bit / 32) as usize) |= 1 << (bit % 32) };
    }

    /// The title writes a constant under a range an earlier list of the
    /// submission declared as written by shaders: the refresh before the
    /// submission runs still brings it to the copy, a shader's write still
    /// goes back, and a title write made after the refresh is not undone.
    #[test]
    fn title_writes_under_a_busy_range_are_copied() {
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        register(p, p as u64, len as u64);
        let Some(copy) = find(p as u64, len as u64) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        let title = |at: usize| (p + at) as *mut u8;
        let host = |at: usize| copy.read_host((p + at) as u64 & !3, 1)[0].to_le_bytes()[at % 4];
        copy.update(p, len);
        // An earlier list declares all of it as written by shaders.
        copy.writing(p, len, false);
        // SAFETY: (all title reads and writes below) bytes of our pool.
        unsafe { title(PAGE + 20).write_volatile(0x55) };
        assert!(copy.update(p + PAGE + 16, 16) > 0, "the title's write is copied");
        assert_eq!(host(PAGE + 20), 0x55, "the title's constant reaches the copy");
        // The shaders run: one byte written. The title writes meanwhile.
        copy.write_host((p + 10) as u64, &[0xaa]);
        // SAFETY: a byte of our pool.
        unsafe { title(PAGE + 20).write_volatile(0x66) };
        copy.copy_back(p, len);
        // SAFETY: bytes of our pool.
        unsafe {
            assert_eq!(title(10).read_volatile(), 0xaa, "the shader's write goes back");
            assert_eq!(title(PAGE + 20).read_volatile(), 0x66, "the title's later write stays");
        }
        unregister(p, p as u64);
    }

    /// A title buffer over a pool and, after it, a plain allocation (memory
    /// that is not the runtime's): the pool is tracked and guarded; the
    /// plain allocation's bytes are copied at each use and never guarded,
    /// so a value the title keeps there reaches the copy, and again after
    /// the title changes it.
    #[test]
    fn bytes_off_the_runtimes_memory_are_copied_at_each_use() {
        use crate::watch::tests::{MEM_WRITE_WATCH, free};
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_READONLY, PAGE_READWRITE,
            VirtualAlloc, VirtualFree, VirtualQuery,
        };
        // A pool page and a plain one right after it, in two pages found free
        // (other tests allocate meanwhile: tried again if taken).
        let (pool, plain) = (0..8)
            .find_map(|_| {
                // SAFETY: a fresh reservation, released at once; then
                // allocations where it was, freed below.
                unsafe {
                    let p = VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE, PAGE_READWRITE) as usize;
                    VirtualFree(p as *mut _, 0, MEM_RELEASE);
                    let at = |q: usize, kind: u32| {
                        VirtualAlloc(q as *const _, PAGE, MEM_RESERVE | MEM_COMMIT | kind, PAGE_READWRITE) as usize
                    };
                    let (pool, plain) = (at(p, MEM_WRITE_WATCH), at(p + PAGE, 0));
                    if p != 0 && pool == p && plain == p + PAGE {
                        return Some((pool, plain));
                    }
                    [pool, plain].into_iter().filter(|&q| q != 0).for_each(free);
                    None
                }
            })
            .expect("two free pages");
        let len = 2 * PAGE;
        register(pool, pool as u64, len as u64);
        let Some(copy) = find(pool as u64, len as u64) else {
            unregister(pool, pool as u64);
            [pool, plain].into_iter().for_each(free);
            return; // no host GPU here
        };
        let word = (plain + 64) as *mut u32;
        let protection = |p: usize| {
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
            info.Protect
        };
        // SAFETY: a word of our plain allocation.
        unsafe { word.write_volatile(0x1234) };
        copy.update(pool, len);
        assert_eq!(copy.read_host(word as u64, 1), [0x1234]);
        assert!(!copy.clean(), "never clean: part of it is not tracked");
        // SAFETY: as above.
        unsafe { word.write_volatile(0x5678) };
        copy.update(pool, len);
        assert_eq!(copy.read_host(word as u64, 1), [0x5678], "the title's change reaches the copy");
        assert_eq!(protection(pool), PAGE_READONLY, "the pool is guarded");
        assert_eq!(protection(plain), PAGE_READWRITE, "the plain allocation is not");
        unregister(pool, pool as u64);
        // The pages stay allocated (as the other tests' pools): a copy some
        // page table still holds must not name memory reused meanwhile.
    }

    /// Runs of the shared buffers start a whole number of elements (and of
    /// 256 bytes) in, do not overlap, and join again when given back.
    #[test]
    fn arena_runs_are_aligned_to_their_elements_and_join_again() {
        if host().is_none() {
            return; // no host GPU here
        }
        let mut arena = Arena::default();
        let (_, _, _, a) = arena.take(100, 456).unwrap();
        let (_, _, _, b) = arena.take(5000, 16).unwrap();
        let (_, _, _, c) = arena.take(4, 4).unwrap();
        assert!(a.offset % 456 == 0 && a.offset % 256 == 0 && b.offset % 256 == 0);
        let mut runs = [a, b, c].map(|s| (s.offset, s.offset + s.len));
        runs.sort_unstable();
        assert!(runs.windows(2).all(|w| w[0].1 <= w[1].0), "{runs:?}");
        for s in [b, a, c] {
            arena.give_back(s);
        }
        assert_eq!(arena.chunks.len(), 1);
        assert_eq!(arena.chunks[0].3.len(), 1, "one free run again");
        // Larger than a buffer: one of its own.
        let (_, _, _, d) = arena.take(CHUNK + 1, 4).unwrap();
        assert_eq!((arena.chunks.len(), d.chunk), (2, 1));
    }

    /// Ranges shaders write far apart are kept apart (not the copy between
    /// them); what the GPU wrote goes back once, whichever overlapping
    /// ranges declared it, and nothing else does.
    #[test]
    fn snapshots_cover_the_declared_ranges_and_writes_go_back_once() {
        let p = crate::watch::tests::pool(4);
        let len = 4 * PAGE;
        register(p, p as u64, len as u64);
        let Some(copy) = find(p as u64, len as u64) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        copy.update(p, len);
        let bytes = |s: &Shadow| s.before.lock().unwrap().0.iter().map(|(_, b)| b.len()).sum::<usize>();
        copy.writing(p + 16, 64, false);
        copy.writing(p + 3 * PAGE, 100, false);
        assert_eq!(bytes(&copy), 164, "two segments");
        copy.writing(p + PAGE, 2 * PAGE, false);
        copy.writing(p + PAGE, PAGE, false);
        copy.writing(p + 80, 8, false);
        assert_eq!(bytes(&copy), 72 + 2 * PAGE + 100, "merged where they overlap or touch");
        // The GPU writes inside two ranges.
        copy.write_host((p + 20) as u64, &[1, 2, 3]);
        copy.write_host((p + PAGE + 5) as u64, &[9]);
        assert_eq!(copy.copy_back(p + 16, 64), 64, "the range's part of the piece");
        assert_eq!(copy.copy_back(p + PAGE, 2 * PAGE), PAGE, "the piece written");
        assert_eq!(copy.copy_back(p + PAGE, PAGE), 0, "compared once");
        assert_eq!(copy.copy_back(p + 3 * PAGE, 100), 0);
        assert_eq!(copy.copy_back(p + 80, 8), 0);
        // SAFETY: our pool.
        let title = |at: usize| unsafe { (at as *const u8).read_volatile() };
        assert_eq!([title(p + 20), title(p + 21), title(p + 22), title(p + PAGE + 5)], [1, 2, 3, 9]);
        assert_eq!(bytes(&copy), 0, "the snapshot goes with the last range");
        unregister(p, p as u64);
    }

    /// Pages shaders marked (`copy_back_marks`, `victory_shader::emit`
    /// "Write marks") go back by the bytes the GPU changed. The title writes
    /// a marked page after the copy took it (the guard saves its bytes from
    /// before) and a page nobody marked: the shader's byte and the title's
    /// both stay in the first; the second keeps the title's, and the copy's
    /// byte there (no mark) is not put back. The bits are cleared.
    #[test]
    fn marked_pages_go_back_by_the_bytes_the_gpu_changed() {
        let _bitmap_guard = BITMAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p = crate::watch::tests::pool(3);
        let len = 3 * PAGE;
        register(p, p as u64, len as u64);
        let (Some(copy), Some(b)) = (find(p as u64, len as u64), bitmap(false)) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        let base = copy.marks.expect("tracked memory gets mark bits") as usize;
        assert_eq!(copy.mark_entry(p as u64 + PAGE as u64 + 8), Some((base as u32 + 1, 8)));
        copy.update(p, len);
        copy.writing(p, len, true);
        assert!(copy.before.lock().unwrap().0.is_empty(), "no snapshot");
        // The shaders write a byte in each page; the title writes pages 1
        // and 2 meanwhile.
        copy.write_host((p + 10) as u64, &[0xaa]);
        copy.write_host((p + PAGE + 20) as u64, &[0xbb]);
        copy.write_host((p + 2 * PAGE + 30) as u64, &[0xcc]);
        let title = |at: usize| (p + at) as *mut u8;
        // SAFETY: (title writes and reads below) bytes of our pool.
        unsafe {
            title(PAGE + 40).write_volatile(0x11);
            title(2 * PAGE + 50).write_volatile(0x22);
        }
        // The shaders marked pages 0 and 1.
        let bits = b.data as *mut u32;
        for n in [base, base + 1] {
            // SAFETY: a dword of the mapped bitmap.
            unsafe { *bits.add(n / 32) |= 1 << (n % 32) };
        }
        copy_back_marks(&[], false);
        // SAFETY: bytes of our pool.
        let read = |at: usize| unsafe { title(at).read_volatile() };
        assert_eq!(read(10), 0xaa, "the shader's byte, page 0");
        assert_eq!((read(PAGE + 20), read(PAGE + 40)), (0xbb, 0x11), "both writers' bytes, page 1");
        assert_eq!((read(2 * PAGE + 30), read(2 * PAGE + 50)), (0, 0x22), "page 2 not marked");
        // SAFETY: dwords of the mapped bitmap.
        let left: u32 = (base / 32..=(base + 1) / 32).map(|w| unsafe { *bits.add(w) }).fold(0, |a, w| a | w);
        assert_eq!(left, 0, "bits cleared");
        assert_eq!(copy.copy_back(p, len), 0, "nothing kept to compare");
        unregister(p, p as u64);
    }

    /// Marks of shaders using title memory stay out of title memory while
    /// their results are held back, and are cleared; the other shaders'
    /// marks of the same submission go back.
    #[test]
    fn marks_of_shaders_using_title_memory_are_held_back() {
        let _bitmap_guard = BITMAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        register(p, p as u64, len as u64);
        let (Some(copy), Some(others), Some(memory)) = (find(p as u64, len as u64), bitmap(false), bitmap(true)) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        let base = copy.marks.expect("tracked memory gets mark bits") as usize;
        copy.update(p, len);
        copy.writing(p, len, true);
        copy.write_host((p + 10) as u64, &[0xaa]);
        copy.write_host((p + PAGE + 10) as u64, &[0xbb]);
        // Page 0 marked by another shader, page 1 by one using title memory.
        for (b, n) in [(others, base), (memory, base + 1)] {
            // SAFETY: a dword of the mapped bitmap.
            unsafe { *(b.data as *mut u32).add(n / 32) |= 1 << (n % 32) };
        }
        copy_back_marks(&[], true);
        // SAFETY: bytes of our pool.
        let read = |at: usize| unsafe { ((p + at) as *const u8).read_volatile() };
        assert_eq!((read(10), read(PAGE + 10)), (0xaa, 0), "page 1 held back");
        // SAFETY: a dword of the mapped bitmap.
        assert_eq!(unsafe { *(memory.data as *const u32).add((base + 1) / 32) } & 1 << ((base + 1) % 32), 0, "cleared");
        unregister(p, p as u64);
    }

    /// What a frame withheld leaves every host copy of its memory at the
    /// frame's end (run textures9's shape): in frame N a program using title
    /// memory writes a record into the copies and the frame misses, so it
    /// stays out of title memory but in the copies for the GPU; in frame
    /// N+1, which misses nothing, a program reading the record must see
    /// title memory's bytes, not frame N's, in a title buffer's copy and in
    /// an inner buffer's copy of the same pages alike.
    #[test]
    fn what_a_frame_withholds_leaves_the_copies_at_its_end() {
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        // SAFETY: a byte of our pool.
        unsafe { ((p + 16) as *mut u8).write_volatile(0x11) };
        register(p, p as u64, len as u64);
        register(p + 1, p as u64, PAGE as u64);
        let (Some(outer), Some(inner)) = (find(p as u64, len as u64), find(p as u64 + 16, 16)) else {
            unregister(p + 1, p as u64);
            unregister(p, p as u64);
            return; // no host GPU here
        };
        assert!(!Arc::ptr_eq(&outer, &inner));
        let byte = |c: &Shadow| c.read_host(p as u64 + 16, 1)[0] & 0xff;
        for c in [&outer, &inner] {
            let (start, n) = c.range();
            c.update(start as usize, n as usize);
            c.writing(start as usize, n as usize, false);
            c.write_host(p as u64 + 16, &[0x22]);
            c.keep_writes(start as usize, n as usize);
        }
        crate::memory::drop_withheld(p + 16, 4);
        assert_eq!((byte(&outer), byte(&inner)), (0x22, 0x22), "frame N: the GPU's copies keep it");
        crate::memory::log_frame();
        for c in [&outer, &inner] {
            let (start, n) = c.range();
            c.update(start as usize, n as usize);
        }
        assert_eq!((byte(&outer), byte(&inner)), (0x11, 0x11), "frame N+1: title memory's bytes");
        // SAFETY: as above.
        assert_eq!(unsafe { ((p + 16) as *const u8).read_volatile() }, 0x11, "title memory untouched");
        unregister(p + 1, p as u64);
        unregister(p, p as u64);
    }

    /// A partial refresh of an armed page updates its saved bytes too (both
    /// under and beside a pending marked range): copy-back neither puts the
    /// refreshed byte back over a later title write nor drops a shader write
    /// back to the pre-fault value.
    #[test]
    fn partial_refresh_under_marked_range_updates_saved_bytes() {
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        register(p, p as u64, len as u64);
        let Some(copy) = find(p as u64, len as u64) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        let write = |at: usize, v: u8| {
            // SAFETY: a byte of our pool.
            unsafe { ((p + at) as *mut u8).write_volatile(v) };
        };
        copy.update(p, len);
        // Arms both pages: page 0 is wholly busy; page 1 past +16 is free.
        copy.writing(p, PAGE + 16, true);
        for (at, v) in [(20, 0x55), (24, 0x44), (40, 0x11), (PAGE + 20, 0x55)] {
            write(at, v);
        }
        assert!(copy.update(p + 16, 16) > 0 && copy.update(p + PAGE + 16, 16) > 0);
        copy.write_host((p + 10) as u64, &[0xaa]);
        copy.write_host((p + 24) as u64, &[0x00]);
        copy.write_host((p + PAGE + 10) as u64, &[0xbb]);
        write(20, 0x66);
        write(PAGE + 20, 0x77);
        let mut title = vec![0u8; PAGE];
        copy.copy_back_marked(p, &mut title);
        copy.copy_back_marked(p + PAGE, &mut title);
        // SAFETY: bytes of our pool.
        let read = |at: usize| unsafe { ((p + at) as *const u8).read_volatile() };
        assert_eq!(
            [read(10), read(20), read(24), read(40)],
            [0xaa, 0x66, 0x00, 0x11]
        );
        assert_eq!([read(PAGE + 10), read(PAGE + 20)], [0xbb, 0x77]);
        copy.copy_back(p, PAGE + 16);
        unregister(p, p as u64);
    }

    /// Mark bits are handed out in runs and come back joined.
    #[test]
    fn mark_runs_are_reused_and_joined() {
        let mut m = Marks::default();
        assert_eq!((m.take(3), m.take(2)), (Some(0), Some(3)));
        m.give_back(0, 3);
        assert_eq!(m.take(1), Some(0), "first fit");
        m.give_back(3, 2);
        assert_eq!(m.free.iter().collect::<Vec<_>>(), [(&1, &4)], "3-4 joins the free 1-2");
        m.give_back(0, 1);
        assert_eq!(m.free.iter().collect::<Vec<_>>(), [(&0, &5)], "and 0 joins them");
        assert_eq!(m.take(5), Some(0));
        assert_eq!(pages(0x1_fff0, 0x20), 2, "a range across a page boundary");
        assert_eq!(pages(0x2_0000, 0x1_0000), 1);
    }

    /// A committed page is live, a reservation is not, and releasing the
    /// span makes it free.
    #[test]
    fn released_memory_is_free_and_a_committed_page_is_not() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        // SAFETY: a null address asks Windows to choose each fresh page range.
        let commit = unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as u64;
        // SAFETY: as above, this reserves a distinct page range without committing it.
        let reserve = unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_RESERVE, PAGE_READWRITE) } as u64;
        assert_ne!(commit, 0);
        assert_ne!(reserve, 0);
        assert!(matches!(title_memory(commit, 4096), TitleMemory::Live));
        assert!(matches!(title_memory(reserve, 4096), TitleMemory::Uncommitted));
        // SAFETY: both nonzero pointers came from VirtualAlloc above; size
        // zero and MEM_RELEASE release each complete reservation.
        unsafe {
            assert_ne!(VirtualFree(commit as *mut _, 0, MEM_RELEASE), 0);
            assert_ne!(VirtualFree(reserve as *mut _, 0, MEM_RELEASE), 0);
        }
        assert!(matches!(title_memory(commit, 4096), TitleMemory::Free));
        assert!(matches!(title_memory(reserve, 4096), TitleMemory::Free));
    }

    /// A loose window and a registered buffer of committed memory survive a
    /// pass; once that memory is released, the old copies go. Another test
    /// may immediately reuse the virtual address, so a fresh lookup there
    /// cannot establish whether the old allocation was copied again.
    #[test]
    fn freed_title_memory_drops_its_copy_and_a_live_one_stays() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        if host().is_none() {
            return;
        }
        let bytes = WINDOW as usize;
        // SAFETY: one reservation big enough for two aligned windows.
        let reserve = unsafe { VirtualAlloc(std::ptr::null(), bytes * 4, MEM_RESERVE, PAGE_READWRITE) } as usize;
        assert_ne!(reserve, 0, "reserve");
        let loose_at = reserve.next_multiple_of(bytes);
        let registered_at = loose_at + bytes;
        // SAFETY: each requested address is page-aligned inside the live
        // reservation above, and Windows commits only that span.
        let commit = |at: usize| unsafe { VirtualAlloc(at as *const _, bytes, MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_eq!(commit(loose_at), loose_at);
        assert_eq!(commit(registered_at), registered_at);
        // SAFETY: a live allocation, released below.
        let live = unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(live, 0, "live");
        register(registered_at, registered_at as u64, bytes as u64);
        let loose_copy = find(loose_at as u64, 64);
        let registered_copy = find(registered_at as u64, 64);
        let live_copy = find(live as u64, 64);
        let Some(loose_copy) = loose_copy else {
            unregister(registered_at, registered_at as u64);
            // SAFETY: both pointers identify live VirtualAlloc reservations;
            // size zero with MEM_RELEASE releases their whole ranges.
            unsafe {
                VirtualFree(reserve as *mut _, 0, MEM_RELEASE);
                VirtualFree(live as *mut _, 0, MEM_RELEASE);
            }
            return;
        };
        let registered_copy = registered_copy.expect("registered copy");
        let live_copy = live_copy.expect("live copy");
        let accounted = || {
            let cache = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                LOOSE_BYTES.load(std::sync::atomic::Ordering::Relaxed),
                cache.values().map(|copy| copy.len).sum::<u64>()
            );
        };
        accounted();
        assert_eq!(loose_copy.range(), (loose_at as u64, bytes as u64));
        assert_eq!(registered_copy.range().0, registered_at as u64);
        assert!(!Arc::ptr_eq(&loose_copy, &registered_copy));
        reap_free();
        let still = all();
        assert!(still.iter().any(|s| Arc::ptr_eq(s, &loose_copy)), "committed loose memory stays");
        assert!(still.iter().any(|s| Arc::ptr_eq(s, &registered_copy)), "committed buffer stays");
        assert!(still.iter().any(|s| Arc::ptr_eq(s, &live_copy)), "the other allocation stays");
        // SAFETY: `reserve` is the live reservation created above.
        unsafe { assert_ne!(VirtualFree(reserve as *mut _, 0, MEM_RELEASE), 0) };
        reap_free();
        let still = all();
        accounted();
        assert!(still.iter().all(|s| !Arc::ptr_eq(s, &loose_copy)), "freed loose memory is given up");
        assert!(still.iter().all(|s| !Arc::ptr_eq(s, &registered_copy)), "freed buffer copy is given up");
        assert!(still.iter().any(|s| Arc::ptr_eq(s, &live_copy)), "live memory stays");
        unregister(registered_at, registered_at as u64);
        // SAFETY: `live` is the remaining live VirtualAlloc reservation.
        unsafe { VirtualFree(live as *mut _, 0, MEM_RELEASE) };
        drop((loose_copy, registered_copy, live_copy));
        reap_free();
    }
}
