//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;

pub(crate) struct BufferRanges<T> {
    passes: u64,
    /// Where the next pass's share of all slots starts (a title slot).
    cursor: u32,
    pub(crate) by_slot: std::collections::HashMap<u32, (T, u64, u64)>,
}

/// The directly indexed buffer views of one heap, with the host copies
/// they retain between passes. Dropping the heap releases those copies.
pub(crate) struct BufferViews<T> {
    slots: Mutex<std::collections::BTreeSet<u32>>,
    changed: Mutex<std::collections::HashSet<u32>>,
    ranges: Mutex<BufferRanges<T>>,
}

impl<T> BufferViews<T> {
    fn new() -> Self {
        Self {
            slots: Mutex::default(),
            changed: Mutex::default(),
            ranges: Mutex::new(BufferRanges {
                passes: 0,
                cursor: 0,
                by_slot: Default::default(),
            }),
        }
    }

    fn written(&self, index: u32, buffer: bool) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if buffer {
            slots.insert(index);
        } else if !slots.remove(&index) {
            return;
        }
        self.changed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(index);
    }

    pub(crate) fn begin_pass(&self) -> (Vec<u32>, std::sync::MutexGuard<'_, BufferRanges<T>>) {
        let mut kept = self.ranges.lock().unwrap_or_else(|e| e.into_inner());
        // Its own statement: `written` locks `slots`, then `changed`.
        let changed = std::mem::take(&mut *self.changed.lock().unwrap_or_else(|e| e.into_inner()));
        // A view may go stale without a write. All slots are revisited in a
        // fresh heap; after that, each pass revisits the next sixteenth of
        // them in turn (each slot about every 16 passes; a little later
        // while slots are added ahead of the turn), with the slots written
        // since. It was all of them every 16th pass: 70,000 slots, some
        // 73 ms of one frame in 16 on the thread recording the title's list,
        // where the others took under 2 ms. The entries the pass keeps stay:
        // it drops each slot's own before making it again.
        let all = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let slots = if kept.passes == 0 {
            kept.by_slot.clear();
            all.iter().copied().collect()
        } else {
            let cursor = kept.cursor;
            let mut slots: Vec<u32> = all
                .range(cursor..)
                .chain(all.range(..cursor))
                .take(all.len().div_ceil(16))
                .copied()
                .collect();
            drop(all);
            if let Some(&last) = slots.last() {
                kept.cursor = last.wrapping_add(1);
            }
            slots.extend(changed);
            slots.sort_unstable();
            slots.dedup();
            slots
        };
        kept.passes = kept.passes.wrapping_add(1);
        (slots, kept)
    }
}

pub(crate) struct HostHeap {
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count`: [`DYNAMIC`] in view heaps, none in
    /// sampler heaps. (Upstream falls back to 2^16 on a capped host heap;
    /// ours keeps 2^18 there: 2^16 and 2^17 both ran out in this title.)
    dynamic: u32,
    sampler: bool,
    /// The record (view index and generation) each slot was last made from.
    made: Mutex<Vec<u64>>,
    /// Last descriptor headers seen in a placed heap, including API writes.
    copied: Option<Mutex<Vec<[u32; 3]>>>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    kept: Mutex<Vec<Option<ID3D12Resource>>>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
    /// How many dynamic slots are free (changed under `next`'s lock): with
    /// fewer than a table needs, the search is not made. Once the world
    /// had taken them all, every later table of the frame searched every
    /// slot before it was refused.
    free: std::sync::atomic::AtomicU32,
    /// The shortest table that found no run of free slots since slots were
    /// last given back (changed under `next`'s lock; `u32::MAX`: none).
    /// Until something is given back the free runs only shrink, so a table
    /// of that length or longer is refused without the search. Enough
    /// slots were free, but in pieces, and every such table walked the
    /// whole ring before it was refused: 0.33 ms each with 131,072 slots,
    /// 1.0 s of the recording threads in a frame that refused 3,143 (run
    /// 45), under the lock all of them need. (A free run that lies across
    /// the place the search starts from is seen by neither.)
    too_long: std::sync::atomic::AtomicU32,
    /// View heaps: the bytes per element of the buffer view at each title
    /// slot, for shaders that pick a buffer by an index they compute
    /// (`victory_shader::emit::SLOT_STRIDES`): an upload buffer of
    /// `SLOT_STRIDES_COUNT` dwords, its GPU address and its mapping.
    strides: Option<(ID3D12Resource, u64, usize)>,
    pub(crate) buffers: BufferViews<std::sync::Arc<crate::shadow::Shadow>>,
    /// The frame of this heap's last pass over those views (`draw.rs`
    /// makes one per frame and heap).
    pub(crate) buffers_frame: std::sync::atomic::AtomicU64,
    /// Title slots holding a view of a texture the title may fill itself (a
    /// placed texture, a texture object), with the frame from which they
    /// are due: such a texture is uploaded when a draw reads it through a
    /// table; one a shader picks by a computed index is brought up to date
    /// from here ([`HostHeap::texture_slots_due`]).
    textures: Mutex<TextureQueue>,
    /// Per title slot, the frame (plus one; 0: never) in which a draw last
    /// read it through a table, or made a table it lies in (with
    /// [`TABLED_READ`] set if the draw read it). The draw that
    /// reads such a slot brings its texture up to date itself and the slot
    /// then leaves `textures`; a write of a slot that was a table's within
    /// the last [`TABLED_FRAMES`] frames is due only [`TABLED_GRACE`]
    /// frames later, by when its draw has read it. A title that builds a
    /// table per draw writes thousands of texture views a frame into slots
    /// only tables read (the title: some 2,500 a frame in its
    /// opening cutscene, a few hundred textures over and over), and every
    /// one of them stood in the queue of textures picked by index, ahead
    /// of or behind the ones shaders do pick by index.
    tabled: Vec<std::sync::atomic::AtomicU32>,
    /// The title memory each placed texture view of this heap describes,
    /// by slot and by address ([`Spans`]): which slots a newer view of
    /// other memory there makes stale, and which to look at again when the
    /// runtime maps, unmaps or writes memory ([`remapped`]).
    placed: Mutex<Spans>,
}

/// Title memory by the placed texture views that describe it: per span of
/// memory (its start, its end at full size) the title slots holding a
/// view of a texture that takes exactly that span. A console title need
/// not clear a descriptor when it streams a texture out, so a newer view
/// that cannot be there together with an older one is all that says the
/// older one is stale ([`Rule`] says when that is). Views of one span with
/// different descriptions (the formats of one family, say) are one
/// texture's.
///
/// Spans may overlap: a texture's smallest levels lie at the start of its
/// memory, and a title that holds only those puts the next texture where
/// the larger ones would be (the title: three 512x256 textures
/// of 0x2c000 bytes each whose starts are 0x2000 and 0x4000 apart, all
/// three read by the same frame's draws, run 188). What is there of a
/// texture ends where the next one starts.
/// No host heap is needed, so it is tested without one.
#[derive(Default)]
pub(crate) struct Spans {
    /// When a newer view makes an older one stale.
    rule: Rule,
    by_start: std::collections::BTreeMap<u64, (u64, std::collections::BTreeSet<u32>)>,
    by_slot: std::collections::HashMap<u32, u64>,
    /// The bytes a placed texture takes, by resource description: worked
    /// out once for each (a title writes the views of a few thousand
    /// descriptions, thousands of times a frame).
    sizes: std::collections::HashMap<[u32; 14], Option<u64>>,
}

/// When a view the title writes makes an older view of other memory stale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Rule {
    /// When it starts where the older one starts: at the same address
    /// with another length, or within the older one's first [`HEAD`]
    /// bytes (or the older within its own). No two textures share the
    /// block of their smallest levels. A view that only reaches into the
    /// address range of an older texture's larger levels says nothing.
    #[default]
    Start,
    /// Whenever their spans at full size overlap (the rule until run
    /// 188, where it made null the slots of textures packed as above:
    /// each view written took its neighbours out of the queue and off the
    /// card, for good). `VICTORY_STALE_TEXTURES=overlap`.
    Overlap,
    /// Never; a view that would be stale under [`Rule::Overlap`] is left
    /// out of the spans instead. `VICTORY_STALE_TEXTURES=0`, as before.
    Never,
}

/// The first bytes of a placed texture that are its own under
/// [`Rule::Start`]: the 4 KiB block its smallest levels share. (Of 129
/// pairs of overlapping neighbours in the listings of runs 188 and 189,
/// none starts closer than 0x2000 to the one before it.)
const HEAD: u64 = 4096;

impl Spans {
    /// The bytes of title memory a placed texture with the resource
    /// description `desc` takes (`crate::resource::placed_bytes`).
    fn bytes(&mut self, desc: &[u32; 14]) -> Option<u64> {
        if let Some(&known) = self.sizes.get(desc) {
            return known;
        }
        let bytes = crate::resource::placed_bytes(desc);
        self.sizes.insert(*desc, bytes);
        bytes
    }

    /// The starts of the spans that overlap `[start, end)`, each taken to
    /// end where the next one starts: the last span that starts before
    /// `start`, if it reaches past it at full size, and every span that
    /// starts inside.
    fn overlapping(&self, start: u64, end: u64) -> Vec<u64> {
        let before = self
            .by_start
            .range(..start)
            .next_back()
            .filter(|(_, span)| span.0 > start)
            .map(|(at, _)| *at);
        before.into_iter().chain(self.by_start.range(start..end).map(|(at, _)| *at)).collect()
    }

    /// Slot `index` holds a view of a texture in `[start, end)`. `newer`:
    /// the title wrote it now, so the slots whose views cannot be there
    /// with it ([`Rule`]) are stale: they are returned, and forgotten.
    /// Otherwise (a draw made the slot from what it held), and under
    /// [`Rule::Never`], a view that clashes with other spans is left out.
    fn note(&mut self, index: u32, start: u64, end: u64, newer: bool) -> Vec<u32> {
        let same = |span: Option<&(u64, std::collections::BTreeSet<u32>)>| span.is_some_and(|s| s.0 == end);
        if self.by_slot.get(&index) == Some(&start) && same(self.by_start.get(&start)) {
            return Vec::new();
        }
        self.forget(index);
        if end <= start {
            return Vec::new();
        }
        let rule = self.rule;
        // The spans the view cannot be there with.
        let clashing: Vec<u64> = match rule {
            Rule::Start => self
                .by_start
                .range(start.saturating_sub(HEAD - 1)..start.saturating_add(HEAD))
                .map(|(at, _)| *at)
                .collect(),
            Rule::Overlap | Rule::Never => self.overlapping(start, end),
        };
        let mut stale = Vec::new();
        for at in clashing {
            if at == start && same(self.by_start.get(&at)) {
                continue;
            }
            if !newer || rule == Rule::Never {
                return stale;
            }
            if let Some((old_end, slots)) = self.by_start.remove(&at) {
                // The first of them are said with both spans, to tell a
                // replacement from a neighbour in one launch's log.
                static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 48 {
                    tracing::info!(
                        slot = index,
                        start = format_args!("{start:#x}"),
                        end = format_args!("{end:#x}"),
                        stale_start = format_args!("{at:#x}"),
                        stale_end = format_args!("{old_end:#x}"),
                        stale_slots = slots.len(),
                        rule = ?rule,
                        "a placed texture view makes older views of other memory stale"
                    );
                }
                for slot in slots {
                    self.by_slot.remove(&slot);
                    stale.push(slot);
                }
            }
        }
        self.by_start.entry(start).or_insert_with(|| (end, Default::default())).1.insert(index);
        self.by_slot.insert(index, start);
        stale
    }

    /// Slot `index` holds no placed texture view any more.
    fn forget(&mut self, index: u32) {
        let Some(start) = self.by_slot.remove(&index) else {
            return;
        };
        if let Some(span) = self.by_start.get_mut(&start) {
            span.1.remove(&index);
            if span.1.is_empty() {
                self.by_start.remove(&start);
            }
        }
    }

    /// The slots whose texture's memory overlaps `[start, end)`.
    fn slots_in(&self, start: u64, end: u64) -> Vec<u32> {
        self.overlapping(start, end)
            .iter()
            .filter_map(|at| self.by_start.get(at))
            .flat_map(|span| span.1.iter().copied())
            .collect()
    }
}

/// Title memory the runtime mapped, unmapped or is about to write since a
/// texture pass last took the ranges ([`remapped`]).
#[derive(Default)]
struct RemappedRanges {
    ranges: Vec<(u64, u64)>,
    full_rescan: bool,
}

const REMAPPED_LIMIT: usize = 1 << 15;
static REMAPPED: Mutex<RemappedRanges> = Mutex::new(RemappedRanges {
    ranges: Vec::new(),
    full_rescan: false,
});

/// The lowest start and the highest end of any span of [`Spans`] so far
/// (the start is `u64::MAX` until there is one): what the runtime does to
/// memory outside is none of the texture pass's business.
static SPANNED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(u64::MAX),
    std::sync::atomic::AtomicU64::new(0),
];

/// Since the start: slots whose view a newer view of other memory there
/// made stale; ranges of memory noted as mapped, unmapped or written;
/// ranges dropped because no pass took them; slots made due by such
/// ranges; and made slots whose memory was found unmapped (made null).
pub(crate) static STALE: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// When a view the title writes makes older views of other memory stale
/// ([`Rule`]): where it starts in the block of their smallest levels, by
/// default; `VICTORY_STALE_TEXTURES=0`: never, as before;
/// `VICTORY_STALE_TEXTURES=overlap`: wherever their spans overlap, the rule
/// until run 188.
fn stale_rule() -> Rule {
    static RULE: std::sync::OnceLock<Rule> = std::sync::OnceLock::new();
    *RULE.get_or_init(|| match std::env::var("VICTORY_STALE_TEXTURES").as_deref() {
        Ok("0") => Rule::Never,
        Ok("overlap") => Rule::Overlap,
        _ => Rule::Start,
    })
}

/// The three words of a descriptor header as read from title memory.
pub(crate) fn words_of(bytes: &[u8; 12]) -> [u32; 3] {
    [0, 4, 8].map(|at| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]))
}

/// The runtime mapped or unmapped title memory `[at, at + len)`, or is
/// about to write it through the kernel (`crate::watch`): the placed
/// textures there may have arrived, gone, or changed with no descriptor
/// written. The range is kept for the next texture pass
/// ([`HostHeap::texture_slots_remapped`]). Called with the runtime's own
/// locks held: this locks one mutex of its own and calls nothing.
pub(crate) fn remapped(at: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let (start, end) = (at as u64, at.saturating_add(len) as u64);
    if end <= SPANNED[0].load(Relaxed) || start >= SPANNED[1].load(Relaxed) {
        return;
    }
    let mut pending = REMAPPED.lock().unwrap_or_else(|e| e.into_inner());
    STALE[1].fetch_add(1, Relaxed);
    if pending.full_rescan {
        STALE[2].fetch_add(1, Relaxed);
        return;
    }
    // Pages are mapped one after the other.
    if let Some(last) = pending.ranges.last_mut()
        && last.1 == start
    {
        last.1 = end;
        return;
    }
    if pending.ranges.len() >= REMAPPED_LIMIT {
        // The next texture pass has to revisit every placed slot, including
        // textures that had already left the upload queue.
        STALE[2].fetch_add(pending.ranges.len() as u64 + 1, Relaxed);
        pending.ranges.clear();
        pending.full_rescan = true;
        return;
    }
    pending.ranges.push((start, end));
}

/// Frames for which a slot a draw read through a table counts as a table's
/// slot ([`HostHeap::tabled`]).
const TABLED_FRAMES: u64 = 240;

/// Frames a write of a table's slot waits in the queue for the draw that
/// reads it, before the slot is taken for one shaders pick by index.
/// Frames as `crate::frame::current` counts them: in its opening cutscene
/// The title begins two or three of them per frame it presents
/// (435 begun in the 353 s of run 52's, for some 140 presented).
const TABLED_GRACE: u64 = 8;

/// In [`HostHeap::tabled`]: a draw read the slot in that frame.
const TABLED_READ: u32 = 1 << 31;

/// Since the start: writes of texture views into slots that count as a
/// table's (due later), and slots that left the queue because a draw read
/// them through a table.
pub(crate) static TABLED_WRITES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether slots draws read through tables leave the queue of textures
/// picked by index; `VICTORY_TABLED_TEXTURES=1` keeps them in it as
/// before.
fn tabled_out() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TABLED_TEXTURES").is_none_or(|v| v != "1"))
}

/// What the queue of textures picked by index knows of a title slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Waiting {
    /// The frame from which the slot is due.
    due: u64,
    /// The frame it was written in.
    written: u64,
    /// Which write of the queue put it here: the pass hands it back with
    /// what it did, so a slot written again meanwhile stays as written.
    write: u64,
    /// Looks in a row that found nothing to upload (its memory not mapped,
    /// or no more of it than at the last upload).
    looks: u8,
    /// Bytes of its memory that were mapped when it was last uploaded
    /// without all of it there; 0: not uploaded yet.
    mapped: u64,
    /// Content checks already spent on this descriptor write.
    checks: u8,
}

/// Spacing in wall time so streaming still gets checked during slow frames.
const TEXTURE_CHECK_MS: [u64; 5] = [150, 400, 1_350, 4_000, 13_000];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextureRecheck {
    index: u32,
    write: u64,
    pub(crate) address: u64,
    pub(crate) bytes: u64,
    pub(crate) fingerprint: u64,
    checks: u8,
}

/// The queue of title slots whose texture view waits to be brought up to
/// date for shaders that pick textures by index: which are due, and in
/// which order (no host heap needed, so it is tested without one).
#[derive(Default)]
pub(crate) struct TextureQueue {
    slots: BTreeMap<u32, Waiting>,
    /// Ordered separately so `due()` only scans slots that need uploads.
    rechecks: BTreeMap<(Instant, u32), TextureRecheck>,
    deadlines: HashMap<u32, Instant>,
    last_recheck_frame: Option<u64>,
    writes: u64,
}

impl TextureQueue {
    /// A texture view was written into slot `index` in `frame`, due from
    /// frame `due`. `seen`: it was not written, a draw or the pass made
    /// the host slot from a view the queue may hold already, which then
    /// stays as it is.
    fn written(&mut self, index: u32, frame: u64, due: u64, seen: bool) {
        if seen && (self.slots.contains_key(&index) || self.deadlines.contains_key(&index)) {
            return;
        }
        self.cancel_recheck(index);
        self.writes += 1;
        let write = self.writes;
        self.slots.insert(index, Waiting { due, written: frame, write, looks: 0, mapped: 0, checks: 0 });
    }

    fn remove(&mut self, index: u32) -> bool {
        let pending = self.slots.remove(&index).is_some();
        self.cancel_recheck(index) || pending
    }

    fn cancel_recheck(&mut self, index: u32) -> bool {
        let Some(due) = self.deadlines.remove(&index) else {
            return false;
        };
        self.rechecks.remove(&(due, index)).is_some()
    }

    /// Up to `max` slots due at `frame`, each with its write number, in
    /// the order to serve them:
    /// - first the ones looked at before (their memory was not mapped, or
    ///   only part of it): the look is a query, and memory that has
    ///   arrived since is what the title streamed in last;
    /// - then the others, the newest write first and the oldest next, in
    ///   turns: slot order served the low slots of a backlog of
    ///   25,000-40,000 while the textures of the shot on screen waited
    ///   (run 52: 98% of the drawn pixels had no colour texture in the
    ///   first 3D frames).
    ///
    /// Also counts all slots, as the frame line needs the whole backlog:
    /// queued, due, waiting for memory, partly uploaded, and the age in
    /// frames of the oldest due write.
    fn due(&self, frame: u64, max: usize) -> (Vec<(u32, u64)>, [u64; 5]) {
        let mut counts = [self.slots.len() as u64, 0, 0, 0, 0];
        let mut again = Vec::new();
        let mut fresh = Vec::new();
        for (&index, w) in &self.slots {
            if w.mapped > 0 {
                counts[3] += 1;
            } else if w.looks > 0 {
                counts[2] += 1;
            }
            if w.due <= frame {
                counts[1] += 1;
                counts[4] = counts[4].max(frame.saturating_sub(w.written));
                if max > 0 {
                    let class = if w.looks > 0 || w.mapped > 0 {
                        &mut again
                    } else {
                        &mut fresh
                    };
                    class.push((w.write, index));
                }
            }
        }
        let retries = again.len().min(max);
        let first_retry = again.len() - retries;
        let by_write = |&(write, _): &(u64, u32)| write;
        if first_retry > 0 {
            again.select_nth_unstable_by_key(first_retry, by_write);
        }
        let again = &mut again[first_retry..];
        again.sort_unstable_by_key(by_write);
        let fresh_count = fresh.len().min(max - retries);
        let mut order = Vec::with_capacity(retries + fresh_count);
        order.extend(again.iter().rev().map(|&(write, index)| (index, write)));
        if fresh_count == 0 {
            return (order, counts);
        }

        // The write number gives the order. Only sort the ends we will
        // serve, leaving the middle of a large backlog unordered.
        let oldest_count = fresh_count / 2;
        let newest_count = fresh_count - oldest_count;
        let newest_start = fresh.len() - newest_count;
        if fresh_count == fresh.len() {
            fresh.sort_unstable_by_key(by_write);
        } else {
            fresh.select_nth_unstable_by_key(newest_start, by_write);
            if oldest_count > 0 {
                fresh[..newest_start].select_nth_unstable_by_key(oldest_count, by_write);
                fresh[..oldest_count].sort_unstable_by_key(by_write);
            }
            fresh[newest_start..].sort_unstable_by_key(by_write);
        }
        let mut oldest = fresh[..oldest_count].iter();
        for &(write, index) in fresh[newest_start..].iter().rev() {
            order.push((index, write));
            if let Some(&(write, index)) = oldest.next() {
                order.push((index, write));
            }
        }
        (order, counts)
    }

    /// The entry of `index` if it is still the write the pass was handed.
    fn same(&mut self, index: u32, write: u64) -> Option<&mut Waiting> {
        self.slots.get_mut(&index).filter(|w| w.write == write)
    }

    /// Slot `index` is up to date: out of the queue until written again.
    fn done(&mut self, index: u32, write: u64) {
        if self.same(index, write).is_some() {
            self.slots.remove(&index);
        }
    }

    /// Nothing to upload at slot `index` now: looked at again after 1, 2,
    /// 4, ... frames, at most `most`.
    fn later(&mut self, index: u32, write: u64, frame: u64, most: u64) {
        if let Some(w) = self.same(index, write) {
            w.due = frame + (1u64 << w.looks.min(16)).min(most.max(1));
            w.looks = w.looks.saturating_add(1);
        }
    }

    /// Slot `index` was uploaded with `mapped` bytes of its memory there,
    /// not all of it: looked at again from frame `due`.
    fn partly(&mut self, index: u32, write: u64, due: u64, mapped: u64) {
        if let Some(w) = self.same(index, write) {
            (w.due, w.looks, w.mapped) = (due, 0, mapped.max(1));
        }
    }

    /// An upload finished. Watch its source for late streaming writes, using
    /// the balance of this slot generation's original check budget.
    fn uploaded(&mut self, index: u32, write: u64, at: (u64, u64), fingerprint: u64, now: Instant) {
        let Some(waiting) = self.same(index, write).copied() else {
            return;
        };
        self.slots.remove(&index);
        let (address, bytes) = at;
        self.schedule_recheck(TextureRecheck {
            index, write, address, bytes, fingerprint, checks: waiting.checks,
        }, now);
    }

    fn schedule_recheck(&mut self, check: TextureRecheck, now: Instant) {
        if let Some(&ms) = TEXTURE_CHECK_MS.get(usize::from(check.checks)) {
            let due = now + Duration::from_millis(ms);
            self.deadlines.insert(check.index, due);
            self.rechecks.insert((due, check.index), check);
        }
    }

    fn due_rechecks(&mut self, frame: u64, now: Instant, max: usize) -> Vec<TextureRecheck> {
        if self.last_recheck_frame == Some(frame) {
            return Vec::new();
        }
        let due: Vec<_> = self.rechecks.range(..=(now, u32::MAX))
            .take(max).map(|(_, check)| *check).collect();
        if !due.is_empty() {
            self.last_recheck_frame = Some(frame);
        }
        due
    }

    /// Complete a sample taken without the lock. A descriptor rewrite or a
    /// new mapping cancels its old sample, even if the bytes changed.
    fn finish_recheck(&mut self, mut check: TextureRecheck, changed: bool, frame: u64, now: Instant) -> bool {
        let current = self.deadlines.get(&check.index)
            .and_then(|&due| self.rechecks.get(&(due, check.index)));
        if current != Some(&check) {
            return false;
        }
        self.cancel_recheck(check.index);
        check.checks += 1;
        if changed {
            // Queued as a write the title made now, the newest, which the
            // next pass serves first. Under the write of its view it sat
            // in the middle of the backlog, past the oldest and the newest
            // writes a pass serves, until the backlog drained to it; and a
            // pass still at work on the old write could take it out. The
            // checks already spent carry over.
            self.writes += 1;
            self.slots.insert(check.index, Waiting {
                due: frame, written: frame, write: self.writes, looks: 0, mapped: 0, checks: check.checks,
            });
        } else {
            self.schedule_recheck(check, now);
        }
        true
    }

    /// The memory slot `index`'s texture lies in was mapped, unmapped or
    /// written in `frame`: the slot is looked at from frame `due` as one
    /// written now, whatever was known of it (a pass at work on it does
    /// not take it out).
    fn changed(&mut self, index: u32, frame: u64, due: u64) {
        self.cancel_recheck(index);
        self.writes += 1;
        let write = self.writes;
        let due = self.slots.get(&index).map_or(due, |w| w.due.min(due));
        self.slots.insert(index, Waiting { due, written: frame, write, looks: 0, mapped: 0, checks: 0 });
    }

    /// Bytes of slot `index`'s memory that were mapped at its last upload
    /// (0: not uploaded, or all of it was), and the looks in a row that
    /// found nothing to upload.
    fn seen(&self, index: u32) -> (u64, u8) {
        self.slots.get(&index).map_or((0, 0), |w| (w.mapped, w.looks))
    }

    /// Notices of the runtime were dropped ([`remapped`]): the slots that
    /// wait on looks are due from frame `at`, as a notice would have made
    /// them (with `crate::draw::park_frames`, they could otherwise wait that
    /// long for memory that arrived). Fresh writes and table slots in their
    /// grace stay as they are.
    fn looked_due(&mut self, at: u64) {
        for w in self.slots.values_mut() {
            if w.looks > 0 || w.mapped > 0 {
                w.due = w.due.min(at);
                w.looks = 0;
            }
        }
    }
}

/// Whether `view` is of a texture the title may fill itself: a texture it
/// placed in memory, or a texture object.
pub(crate) fn is_texture_view(view: &View) -> bool {
    match view {
        View::Placed(false, resource, _) => resource[0] != 1,
        View::Shader(r, _) => crate::resource::buffer_bytes(*r as crate::object::This).is_none(),
        _ => false,
    }
}

/// Why a buffer view of the title's was left null ([`NULL_BUFFER_VIEWS`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NullView {
    /// No elements: the null view reads as the empty one would.
    Empty,
    /// A raw view that starts off 16 bytes, which the host refuses.
    RawStart,
    /// A raw view of another format than R32 typeless, or with a stride.
    RawFormat,
    /// A structured view with a format, or a stride the host has none for.
    Structured,
    /// A typed view of a typeless or unsized format.
    Typed,
    /// A view past its memory's end.
    PastEnd,
    /// A typed or structured view whose start is not on an element of its
    /// host copy.
    OffElement,
    /// Memory with no host copy.
    NoCopy,
    /// Unordered access to a host copy shaders cannot write.
    Unwritable,
}

/// How many kinds of [`NullView`] there are; the first six are the views
/// the host would refuse ([`buffer_view_refusal`]).
pub(crate) const NULL_VIEW_KINDS: usize = 9;

/// Buffer views left null, by [`NullView`], since the start (counted each
/// time a slot is made, not per distinct view).
pub(crate) static NULL_BUFFER_VIEWS: [std::sync::atomic::AtomicU64; NULL_VIEW_KINDS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; NULL_VIEW_KINDS];

/// A buffer view (an unordered one if `unordered`) left null for `why`:
/// counted; true the first time for that reason and kind of view (the
/// caller then logs the view).
fn left_null(why: NullView, unordered: bool) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    NULL_BUFFER_VIEWS[why as usize].fetch_add(1, Relaxed);
    static LOGGED: [[std::sync::atomic::AtomicBool; NULL_VIEW_KINDS]; 2] =
        [const { [const { std::sync::atomic::AtomicBool::new(false) }; NULL_VIEW_KINDS] }; 2];
    let logged = &LOGGED[usize::from(unordered)][why as usize];
    !logged.load(Relaxed) && !logged.swap(true, Relaxed)
}

/// Whether the host device is gone, and why (the first time it is asked
/// after that): for passes that make many views, to name the one that did
/// it.
pub(crate) fn device_removed() -> Option<String> {
    let host = host()?;
    // SAFETY: a plain query.
    let reason = unsafe { host.device.GetDeviceRemovedReason() };
    reason.err().map(|e| e.to_string())
}

/// Writes of title view-heap slots with a placed texture view and with a
/// view of a texture object, since the start (diagnostic counts).
pub(crate) static PLACED_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static OBJECT_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes per element of a buffer view, as the console's descriptor holds
/// them for indexed reads (structured: the stride; raw: 1, the index is the
/// byte; typed: the format's); `None` if `view` is not a buffer view.
fn view_stride(view: &View) -> Option<u32> {
    const BUFFER: u32 = 1;
    let (words, unordered): (&[u32; 10], bool) = match view {
        View::Shader(_, Some(d)) if d[1] == BUFFER => (d, false),
        View::Unordered(_, _, Some(d)) if d[1] == BUFFER => (d, true),
        View::Placed(unordered, resource, d) if resource[0] == BUFFER => {
            (d[..10].try_into().ok()?, *unordered)
        }
        _ => return None,
    };
    // SAFETY: buffer views' union members.
    let (stride, raw, format) = unsafe {
        if unordered {
            let d = uav_desc(words);
            let b = d.Anonymous.Buffer;
            (b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0, d.Format)
        } else {
            let d = srv_desc(words);
            let b = d.Anonymous.Buffer;
            (b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0, d.Format)
        }
    };
    Some(if stride > 0 {
        stride
    } else if raw {
        1
    } else {
        crate::resource::element(format.0 as u32).map_or(0, |(bytes, _)| bytes as u32)
    })
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!("{:#x} past its end (slot {})", va - start - bytes, (va - start) / step)
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// Asks the host, once, whether it caps view heaps at [`PORTABLE_VIEWS`],
/// and if so tells the translator now (`set_view_heap_limit`) instead of
/// when the title's heap is made: a shader keeps the limit it was
/// translated with, and the title makes 25 pipelines (their
/// shaders translated at once) before it makes its view heap. Those would
/// read a view index past the heap unclamped, the read that hangs NVIDIA.
/// Called before a shader is translated.
/// shortcut: the limit is the capped heap's last slot whatever size the
/// title's heap turns out to have; a heap that fits is not capped and
/// keeps no null slot there (an index past it is as undefined as before).
pub(crate) fn probe_view_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(host) = host() else { return };
        // SAFETY: plain heap creation; the heap is released at once.
        let made: windows::core::Result<ID3D12DescriptorHeap> = unsafe {
            host.device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: PORTABLE_VIEWS + 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
        };
        if made.is_err() {
            tracing::info!("the host caps view heaps at {PORTABLE_VIEWS}: view indices are clamped from the first shader on");
            victory_shader::emit::set_view_heap_limit(PORTABLE_VIEWS - 1);
        }
    });
}

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out in the world ("no free dynamic descriptor slots", run 11;
/// upstream PR #336 takes 2^18). On a host heap capped at 1,000,000 views
/// (NVIDIA) these come out of the title's slots, whose highest seen is
/// about 720,000: 2^17 left 868,000 of them, and ran out in the opening
/// cutscene (a frame asks for up to 16,000 tables while the frame before
/// still holds its own; a draw refused its table read slots never made
/// for it, and the card stopped within frames). 2^18 leaves 737,000; a
/// table past them is made afresh in these slots like any other.
pub(crate) const DYNAMIC: u32 = 1 << 18;

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        let extra = if sampler { 0 } else { DYNAMIC };
        let (heap, count, size) = match make(count + extra) {
            Ok(heap) => (heap, count, count + extra),
            Err(e) if !sampler && count + extra > PORTABLE_VIEWS => {
                // shortcut: descriptors past the host's limit are left out; a title that reaches
                // them needs remapping. The heap's last slot stays null, and shaders read it for
                // any view index from there on (`set_view_heap_limit`): a read past the heap's
                // end hung NVIDIA's GPU (the title's heap is a little over 1 000 000 views).
                tracing::warn!(error = %e, count, "host view heap capped at {PORTABLE_VIEWS}; views past it read a null slot");
                victory_shader::emit::set_view_heap_limit(PORTABLE_VIEWS - 1);
                (make(PORTABLE_VIEWS).ok()?, PORTABLE_VIEWS - extra - 1, PORTABLE_VIEWS)
            }
            Err(e) => {
                tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                return None;
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(count + extra) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let strides = (!sampler)
            .then(|| {
                let bytes = u64::from(victory_shader::emit::SLOT_STRIDES_DWORDS) * 4;
                let (buffer, data) = host.upload_buffer(bytes)?;
                // SAFETY: the buffer's whole mapping; a live buffer.
                let gpu = unsafe {
                    std::ptr::write_bytes(data, 0, bytes as usize);
                    buffer.GetGPUVirtualAddress()
                };
                crate::host::name(&buffer, || "element strides by view heap slot".to_owned());
                Some((buffer, gpu, data as usize))
            })
            .flatten();
        let h = HostHeap {
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made: Mutex::new(vec![0; count as usize]),
            copied: None,
            kept: Mutex::new(vec![None; (count + extra) as usize]),
            next: Mutex::new((0, vec![false; DYNAMIC as usize])),
            free: std::sync::atomic::AtomicU32::new(DYNAMIC),
            too_long: std::sync::atomic::AtomicU32::new(u32::MAX),
            strides,
            buffers: BufferViews::new(),
            buffers_frame: std::sync::atomic::AtomicU64::new(u64::MAX),
            textures: Mutex::default(),
            tabled: (0..if sampler { 0 } else { count })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            placed: Mutex::default(),
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, size) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(ms = started.elapsed().as_millis(), "host descriptor heap made null");
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: BLOCK,
                    ..Default::default()
                })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device.CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device.CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::DynamicAsked, 1);
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let enough = self.free.load(std::sync::atomic::Ordering::Relaxed) >= n
            && n < self.too_long.load(std::sync::atomic::Ordering::Relaxed);
        let n = n as usize;
        // A free run can straddle the cursor. Extend the second scan far
        // enough to finish a run that starts just before it.
        let cursor = *next as usize;
        let end = self.dynamic as usize;
        let second_end = (cursor + n - 1).min(end);
        let starts = (cursor..end).chain(0..second_end);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts.take(if enough { end + n - 1 } else { 0 }) {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            if enough {
                self.too_long.fetch_min(n as u32, std::sync::atomic::Ordering::Relaxed);
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(n, "no free dynamic descriptor slots: a table is not made"));
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::DynamicRefused, 1);
                add(C::DynamicNs, started.elapsed().as_nanos() as u64);
            }
            return None;
        };
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::DynamicNs, started.elapsed().as_nanos() as u64);
        taken[first..first + n].fill(true);
        self.free.fetch_sub(n as u32, std::sync::atomic::Ordering::Relaxed);
        *next = ((first + n) % end) as u32;
        Some(self.count + first as u32)
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// The GPU address of the table of element strides by title slot (view
    /// heaps).
    pub(crate) fn strides_gpu(&self) -> Option<u64> {
        self.strides.as_ref().map(|s| s.1)
    }

    /// The table's buffer, for a list to hold.
    pub(crate) fn strides_buffer(&self) -> Option<&ID3D12Resource> {
        self.strides.as_ref().map(|s| &s.0)
    }

    /// Up to `max` title slots whose texture view is due at `frame`, each
    /// with its write number, in service order. Also returns counts for
    /// the whole queue ([`TextureQueue::due`]).
    pub(crate) fn texture_slots_due(&self, frame: u64, max: usize) -> (Vec<(u32, u64)>, [u64; 5]) {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.due(frame, max)
    }

    /// Every title slot of the queue that is due at `frame`, and whether
    /// it was uploaded before (in part, or whole and waiting for another
    /// look): for the card accounting line
    /// (`crate::resource::card_accounting`).
    pub(crate) fn texture_slots_all_due(&self, frame: u64) -> Vec<(u32, bool)> {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures
            .slots
            .iter()
            .filter(|(_, w)| w.due <= frame)
            .map(|(&index, w)| (index, w.mapped > 0 || w.checks != 0))
            .collect()
    }

    /// The texture view of title slot `index` (as handed out with `write`)
    /// was brought up to date: due again from frame `again` (uploaded with
    /// that many bytes of its memory there, not all), or not until the
    /// slot is written again.
    pub(crate) fn texture_slot_done(&self, index: u32, write: u64, again: Option<(u64, u64)>) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match again {
            Some((frame, mapped)) => textures.partly(index, write, frame, mapped),
            None => textures.done(index, write),
        }
    }

    /// Title slot `index` was uploaded whole: see [`TextureQueue::uploaded`].
    pub(crate) fn texture_slot_uploaded(
        &self,
        index: u32,
        write: u64,
        at: (u64, u64),
        fingerprint: u64,
        now: Instant,
    ) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.uploaded(index, write, at, fingerprint, now);
    }

    pub(crate) fn texture_rechecks_due(&self, frame: u64, now: Instant, max: usize) -> Vec<TextureRecheck> {
        self.textures.lock().unwrap_or_else(|e| e.into_inner()).due_rechecks(frame, now, max)
    }

    pub(crate) fn finish_texture_recheck(&self, check: TextureRecheck, changed: bool, frame: u64, now: Instant) -> bool {
        self.textures.lock().unwrap_or_else(|e| e.into_inner()).finish_recheck(check, changed, frame, now)
    }

    /// Nothing to upload at title slot `index` now (its memory is not
    /// mapped, or no more of it than before): looked at again after 1, 2,
    /// 4, ... frames, at most `most`.
    pub(crate) fn texture_slot_later(&self, index: u32, write: u64, frame: u64, most: u64) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.later(index, write, frame, most);
    }

    /// Bytes of title slot `index`'s memory that were mapped when it was
    /// last uploaded in part (0: not uploaded, or whole), and the looks in a
    /// row at it that found nothing to upload (0 after a write or a notice).
    pub(crate) fn texture_slot_seen(&self, index: u32) -> (u64, u8) {
        self.textures.lock().unwrap_or_else(|e| e.into_inner()).seen(index)
    }

    /// Takes the ranges of title memory the runtime mapped, unmapped or
    /// wrote since the last call ([`remapped`]) and makes the slots whose
    /// placed texture lies there due from the next frame (by when a write
    /// under way has landed): memory that arrived is uploaded, memory that
    /// went makes the slot null ([`HostHeap::texture_slot_gone`]), and a
    /// texture streamed into the place of another of the same description
    /// is uploaded although no descriptor says so. Slots tables read are
    /// left to their draws. Returns how many slots it made due.
    /// shortcut: the ranges are kept in one place for all heaps; the heap
    /// whose pass runs first takes them (a title has one view heap).
    pub(crate) fn texture_slots_remapped(&self, frame: u64) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let pending = std::mem::take(&mut *REMAPPED.lock().unwrap_or_else(|e| e.into_inner()));
        if pending.ranges.is_empty() && !pending.full_rescan {
            return 0;
        }
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            if pending.full_rescan {
                placed.by_slot.keys().copied().collect()
            } else {
                pending.ranges.iter().flat_map(|&(start, end)| placed.slots_in(start, end)).collect()
            }
        };
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        if pending.full_rescan {
            if crate::draw::park_frames() > 0 {
                textures.looked_due(frame + 1);
            }
            tracing::info!(slots = slots.len(), "texture remap queue overflowed; rechecking placed textures");
        }
        for &slot in &slots {
            textures.changed(slot, frame, frame + 1);
        }
        STALE[3].fetch_add(slots.len() as u64, Relaxed);
        slots.len()
    }

    /// Whether host slot `index` is made from the very record the title
    /// has at `handle` now. Its host texture then exists (a slot keeps what
    /// it views), so bringing that texture up to date takes no memory of
    /// the card.
    pub(crate) fn texture_slot_made_from(&self, index: u32, handle: u64) -> bool {
        if index >= self.count {
            return false;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let key = u64::from(generation) << 32 | u64::from(i);
        if tag != VIEW_TAG {
            return false;
        }
        // And the slot holds a host resource: a slot is also noted as made
        // from a view the host could not make (`make` gave nothing, the
        // slot is null and keeps nothing). The host copy of such a
        // texture is held by no slot, may have been let go, and would be
        // made anew here, with the card full. `made` first, then `kept`,
        // as `mirror` takes them.
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        made.get(index as usize).is_some_and(|&made| made != 0 && made == key)
            && self
                .kept
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(index as usize)
                .is_some_and(Option::is_some)
    }

    /// The memory of the texture title slot `index` views is not mapped
    /// (any more): a host slot made from it becomes null again, so that
    /// the host copy can be let go (`crate::resource::let_go_idle`; a slot
    /// keeps what it views). True if the slot was made.
    pub(crate) fn texture_slot_gone(&self, index: u32) -> bool {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if made.get(index as usize).is_none_or(|&key| key == 0) {
            return false;
        }
        self.unmake(&mut made, index);
        STALE[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Makes host slot `index` null again, as it is before its view is
    /// made: a null view of the kind it held.
    fn unmake(&self, made: &mut [u64], index: u32) {
        let Some(key) = made.get_mut(index as usize).map(std::mem::take) else {
            return;
        };
        if key == 0 {
            return;
        }
        match view_at(key as u32, (key >> 32) as u32) {
            Some(view) => null_for(&view, self.cpu(index)),
            None => null_view(self.cpu(index)),
        }
        self.keep(index, None);
    }

    /// Title slot `index` is a table's ([`HostHeap::tabled`]). `read`: a
    /// draw read it through the table and brought what it views up to
    /// date, so the slot leaves the queue of textures picked by index; the
    /// caller makes the host slot in place ([`HostHeap::mirror`]), as the
    /// pass would have, for a shader that picks it by index all the same.
    pub(crate) fn texture_slot_tabled(&self, index: u32, read: bool) {
        self.tabled_in(index, read, crate::frame::current());
    }

    /// [`HostHeap::texture_slot_tabled`] in frame `frame`.
    fn tabled_in(&self, index: u32, read: bool, frame: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(stamp) = self.tabled.get(index as usize) else {
            return;
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        let old = stamp.load(Relaxed);
        if !read {
            if old & !TABLED_READ != now {
                stamp.store(now, Relaxed);
            }
            return;
        }
        // Once per slot and frame: tables are read by every draw.
        if old != now | TABLED_READ {
            stamp.store(now | TABLED_READ, Relaxed);
            if tabled_out() {
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                if textures.remove(index) {
                    TABLED_WRITES[1].fetch_add(1, Relaxed);
                }
            }
        }
    }

    /// Whether title slot `index` was a table's within the last
    /// [`TABLED_FRAMES`] frames before `frame` (never, if such slots are
    /// to be queued as any other).
    fn tabled_lately(&self, index: u32, frame: u64) -> bool {
        let stamp = self.tabled_stamp(index) & !TABLED_READ;
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        tabled_out() && stamp != 0 && u64::from(now.wrapping_sub(stamp)) <= TABLED_FRAMES
    }

    /// What `tabled` holds for title slot `index`; 0: never a table's.
    fn tabled_stamp(&self, index: u32) -> u32 {
        self.tabled
            .get(index as usize)
            .map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The queue's part of a write of title slot `index` in `frame`:
    /// `texture`, whether it now holds a view of a texture the title may
    /// fill; `seen` as for [`HostHeap::note_buffer`].
    fn queue_texture(&self, index: u32, texture: bool, seen: bool, frame: u64) {
        let stamp = if tabled_out() { self.tabled_stamp(index) } else { 0 };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        // A table's slot lately: the draw that reads it uploads its
        // texture, so its write waits for that draw.
        let lately = stamp != 0 && u64::from(now.wrapping_sub(stamp & !TABLED_READ)) <= TABLED_FRAMES;
        // Made now by a draw of this frame that read it through a table:
        // up to date.
        let read_now = seen && stamp == now | TABLED_READ;
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        if texture && !read_now {
            let due = if lately && !seen {
                TABLED_WRITES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                frame + TABLED_GRACE
            } else {
                0
            };
            textures.written(index, frame, due, seen);
        } else {
            textures.remove(index);
        }
    }

    /// Records what title slot `index` now holds for shaders that pick
    /// buffers from the heap: its element stride in the table, and whether
    /// it is a buffer view at all.
    /// `seen`: the slot was not written now, a draw or a pass made its
    /// host slot from what it held.
    /// Returns the title slots whose placed texture view the write made
    /// stale ([`Spans`]): out of the queue of textures already, their host
    /// slots are the caller's to make null (it holds `made`).
    fn note_buffer(&self, index: u32, view: Option<&View>, seen: bool) -> Vec<u32> {
        if self.sampler {
            return Vec::new();
        }
        // Measured for the texture side of the same gap (textures a shader
        // picks by a computed index are neither made, if placed, nor
        // uploaded): slots written with a placed texture view, and with a
        // view of a texture object.
        let texture = match view {
            Some(View::Placed(false, resource, _)) if resource[0] != 1 => {
                PLACED_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            Some(View::Shader(r, _)) if crate::resource::buffer_bytes(*r as crate::object::This).is_none() => {
                OBJECT_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        };
        self.queue_texture(index, texture, seen, crate::frame::current());
        // The memory a placed texture view describes. Not for a slot
        // tables read (thousands of writes a frame, each read by its
        // draw): no pass looks at it, and the same texture's view in a
        // slot shaders pick by index says what it says.
        let placed_view = match view {
            Some(View::Placed(false, resource, desc))
                if resource[0] != 1 && !self.tabled_lately(index, crate::frame::current()) =>
            {
                Some((resource, u64::from(desc[10]) | u64::from(desc[11]) << 32))
            }
            _ => None,
        };
        let stale = {
            use std::sync::atomic::Ordering::Relaxed;
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            let span = placed_view
                .and_then(|(resource, start)| Some((start, start.saturating_add(placed.bytes(resource)?))));
            match span {
                Some((start, end)) => {
                    SPANNED[0].fetch_min(start, Relaxed);
                    SPANNED[1].fetch_max(end, Relaxed);
                    placed.rule = stale_rule();
                    placed.note(index, start, end, !seen)
                }
                None => {
                    placed.forget(index);
                    Vec::new()
                }
            }
        };
        if !stale.is_empty() {
            STALE[0].fetch_add(stale.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            for &slot in &stale {
                textures.remove(slot);
            }
        }
        let stride = view.and_then(view_stride);
        if let Some((_, _, data)) = &self.strides
            && index < victory_shader::emit::SLOT_STRIDES_COUNT
        {
            // SAFETY: inside the mapping; shaders read it while we write (a
            // dword, either value is one the slot had).
            unsafe { (*data as *mut u32).add(index as usize).write_volatile(stride.unwrap_or(0)) };
            // The title address of the buffer view, after the strides
            // (`victory_shader::emit::SLOT_ADDRESSES`): low dword, then high.
            let address = view
                .and_then(buffer_range)
                .map_or(0, |range| range.0);
            // SAFETY: as above, two dwords inside the mapping.
            unsafe {
                let at = (*data as *mut u32).add((victory_shader::emit::SLOT_ADDRESSES + 2 * index) as usize);
                at.write_volatile(address as u32);
                at.add(1).write_volatile((address >> 32) as u32);
            }
        }
        self.buffers.written(index, stride.is_some());
        stale
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at + n).min(self.dynamic) as usize;
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            let given = slots.iter().filter(|taken| **taken).count() as u32;
            self.free.fetch_add(given, std::sync::atomic::Ordering::Relaxed);
            self.too_long.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
            slots.fill(false);
        }
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot.
    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let kept = make(view, self.cpu(index));
        if kept.is_none() {
            null_for(view, self.cpu(index));
        }
        self.keep(index, kept.flatten());
    }

    /// Uses the same unordered access view for both handles in a GPU clear.
    pub(crate) fn copy_unordered_at(
        &self,
        index: u32,
        view: &View,
        cpu: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Option<ID3D12Resource> {
        let host = host()?;
        let resource = make(view, cpu).flatten()?;
        // SAFETY: both slots are live and use the CBV/SRV/UAV heap type.
        unsafe {
            host.device.CopyDescriptorsSimple(
                1, self.cpu(index), cpu, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            );
        }
        self.keep(index, Some(resource.clone()));
        Some(resource)
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.keep(index, None);
    }

    /// Slot `index` now views `resource` (or nothing of the host's).
    fn keep(&self, index: u32, resource: Option<ID3D12Resource>) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = kept.get_mut(index as usize) {
            // A running submission may still read the old resource through
            // this slot.
            if let Some(old) = std::mem::replace(slot, resource) {
                crate::host::retire(old);
            }
        }
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view of `len` bytes
    /// of `buffer` from byte `offset` (a multiple of 16): a root SRV with a
    /// size, whose reads past it return zeros instead of reaching whatever
    /// the host placed after the buffer.
    pub(crate) fn raw_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index))
        };
        self.keep(index, Some(buffer.clone()));
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view that reads
    /// zeros.
    pub(crate) fn null_raw_at(&self, index: u32) {
        null_buffer_view(self.cpu(index));
        self.keep(index, None);
    }

    /// Makes slot `index` (a dynamic one) a raw unordered access view of
    /// `len` bytes of `buffer` from byte `offset` (a multiple of 16): a
    /// root UAV with a size, whose accesses past it do nothing instead of
    /// reaching whatever the host placed after the buffer.
    pub(crate) fn raw_uav_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device.CreateUnorderedAccessView(
                buffer,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
        self.keep(index, Some(buffer.clone()));
    }

    /// Makes slot `index` (a dynamic one) a raw unordered access view of
    /// nothing: reads give zeros, writes are dropped.
    pub(crate) fn null_raw_uav_at(&self, index: u32) {
        self.keep(index, None);
        let Some(host) = host() else { return };
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 1,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        // SAFETY: a slot of our heap; a null resource is allowed with a desc.
        unsafe {
            host.device.CreateUnorderedAccessView(
                None::<&ID3D12Resource>,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart().ptr };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start + u64::from(index) * u64::from(self.step),
        }
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory (the list's own, which it holds).
    pub(crate) fn constant_at(&self, index: u32, gpu: u64, size: u32) {
        self.keep(index, None);
        let Some(host) = host() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: constant_view_size(size),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    pub(crate) fn track_copied_records(&mut self) {
        self.copied = Some(Mutex::new(vec![[0; 3]; self.count as usize]));
    }

    /// Mirrors the changed headers (index, handle, words) of the kernel's snapshot of a heap
    /// page, and says how many changed. Most of a written page is as it was: those are
    /// found under one lock of the kept records, not two locks each.
    pub(crate) fn mirror_copied_run(&self, records: &[(u32, u64, [u32; 3])]) -> u64 {
        let changed: Vec<(u32, u64, [u32; 3])> = match &self.copied {
            Some(kept) => {
                let kept = kept.lock().unwrap_or_else(|e| e.into_inner());
                records
                    .iter()
                    .filter(|&&(index, _, words)| kept.get(index as usize).is_none_or(|k| *k != words))
                    .copied()
                    .collect()
            }
            None => records.to_vec(),
        };
        changed
            .into_iter()
            .map(|(index, handle, words)| u64::from(self.mirror_words(index, handle, words, false, true).1))
            .sum()
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        if index >= self.count {
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let words = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        self.mirror_words(index, handle, words, for_draw, false).0
    }

    fn mirror_words(
        &self,
        index: u32,
        handle: u64,
        words: [u32; 3],
        for_draw: bool,
        copied: bool,
    ) -> (Option<View>, bool) {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(records) = &self.copied {
            let mut records = records.lock().unwrap_or_else(|e| e.into_inner());
            let record = &mut records[index as usize];
            if copied && *record == words {
                return (None, false);
            }
            *record = words;
        }
        let [tag, i, generation] = words;
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        let Some(view) = view else {
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            if made[index as usize] != 0 {
                made[index as usize] = 0;
                self.make_null(index);
                let _ = self.note_buffer(index, None, false);
            } else if for_draw || copied {
                // Left for later and stale since: out of the sets and the stride table.
                let _ = self.note_buffer(index, None, false);
            }
            return (None, true);
        };
        if made[index as usize] != key {
            // Views of other textures in the memory this one describes
            // are stale: their host slots read nothing from here on, and
            // let go of the host copies they held.
            for stale in self.note_buffer(index, Some(&view), for_draw) {
                if stale != index {
                    self.unmake(&mut made, stale);
                }
            }
            // A view of a texture object that has no host copy yet waits
            // for it, as buffer views wait for a draw: the slot is made
            // when the texture is first used (`super::make_waiting`), or
            // here when a draw reads the slot through a table.
            let waiting = if for_draw { None } else { view.waiting_texture() };
            if let Some(texture) = waiting {
                super::wait_for(texture, handle);
            }
            let later = !for_draw && (view.is_buffer() || waiting.is_some());
            // Another view of the placed texture the slot shows already (a
            // title that streams a texture in writes its view again with
            // more mips each time some arrive): the slot goes on showing
            // the old view until a draw or the texture pass makes the new
            // one and uploads what it shows. Made null here, the texture
            // read as nothing for as long as it waited in the queue, every
            // time it got sharper.
            if later
                && keep_views()
                && view_at(made[index as usize] as u32, (made[index as usize] >> 32) as u32)
                    .is_some_and(|old| same_placed_texture(&old, &view))
            {
                KEPT_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return (Some(view), true);
            }
            made[index as usize] = if later { 0 } else { key };
            let kept = if later {
                None
            } else {
                make(&view, self.cpu(index))
            };
            if kept.is_none() && !self.sampler {
                null_for(&view, self.cpu(index));
            }
            self.keep(index, kept.flatten());
        }
        (Some(view), true)
    }

    /// The view ticket mirrored at `index` (tests).
    #[cfg(test)]
    pub(crate) fn made_key(&self, index: u32) -> Option<u64> {
        self.made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)
            .copied()
            .filter(|&key| key != 0)
    }

    /// Whether host slot `index` shows `buffer`; `None` if it shows no
    /// resource (diagnostics: a buffer view's slot must show the host copy
    /// that is brought up to date for it).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn shows(&self, index: u32, buffer: &ID3D12Resource) -> Option<bool> {
        let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let shown = kept.get(index as usize)?.as_ref()?;
        Some(windows::core::Interface::as_raw(shown) == windows::core::Interface::as_raw(buffer))
    }

    /// What the queue of textures picked by index knows of title slot
    /// `index` at `frame`, in a few words (diagnostics).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn texture_slot_state(&self, index: u32, frame: u64) -> String {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match textures.slots.get(&index) {
            None => "not queued".into(),
            Some(w) => format!(
                "queued due={} written@{} looks={} part_mapped={} recheck={}",
                if w.due <= frame { "now".to_string() } else { format!("@{}", w.due) },
                w.written,
                w.looks,
                w.mapped,
                w.checks != 0
            ),
        }
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self.made.lock().unwrap_or_else(|e| e.into_inner()).get(index as usize)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The title memory the host slot of buffer view `view` is made on
/// (address, bytes): the whole buffer the view is of, which is what
/// [`make`] finds the host copy by. [`buffer_range`] is the part of it the
/// view reads.
pub(crate) fn buffer_extent(view: &View) -> Option<(u64, u64)> {
    match view {
        View::Shader(r, Some(_)) | View::Unordered(r, _, Some(_)) => {
            let (data, len) = crate::resource::buffer_bytes(*r as crate::object::This)?;
            Some((data as u64, len))
        }
        View::Placed(_, resource, desc) if resource[0] == 1 => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            u64::from(resource[4]) | u64::from(resource[5]) << 32,
        )),
        _ => None,
    }
}

/// Whether a buffer view is brought up to date in the host copy its slot
/// shows ([`buffer_copy`]). On by default; `VICTORY_VIEW_EXTENT=0` looks the
/// copy up by the view's range alone, as before.
fn view_extent() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_EXTENT").is_none_or(|v| v != "0"))
}

/// The host copy buffer view `view` reads, and the title range (address,
/// bytes) it reads there; `None` for other views and empty ranges.
///
/// The copy is the one the view's slot shows: [`make`] finds it by the
/// whole buffer the view is of ([`buffer_extent`]). Looked up by the range
/// the view reads, as it was, the copy is another one wherever the two
/// lookups differ: for memory outside buffer objects, a view of a part of a
/// buffer that crosses a window line (the buffer's window is two cells, the
/// part's one), and for buffer objects, a part that also lies in a smaller
/// buffer object. The slot then showed a copy nothing brought up to date,
/// and what shaders wrote through it was looked for in the other copy.
pub(crate) fn buffer_copy(view: &View) -> Option<(std::sync::Arc<crate::shadow::Shadow>, u64, u64)> {
    let (address, len) = buffer_range(view)?;
    if len == 0 {
        return None;
    }
    if view_extent()
        && let Some((base, width)) = buffer_extent(view)
        // (The console is lax: a view may begin past its buffer's end.)
        && address >= base
        && address - base < width.max(1)
        && let Some(shadow) = crate::shadow::find(base, width)
    {
        return Some((shadow, address, len));
    }
    crate::shadow::find(address, len).map(|shadow| (shadow, address, len))
}

/// [`buffer_range`] of an unordered access view of memory at `base`.
fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The host resource a slot made by [`make`] views, if any.
type Kept = Option<ID3D12Resource>;

/// A structured buffer view (`first`, `count` elements of `stride` bytes,
/// `offset` bytes into its host copy) as a raw view of the same bytes:
/// its first dword and its dwords. Translated shaders read untyped buffers
/// as raw bytes at `element * stride + offset` (the stride from the
/// used-resource entry, or from the table by heap slot for a buffer picked
/// by index), and a raw read through a structured view gives zeros on
/// NVIDIA (reported upstream with PR #193; structured reads through raw
/// views were right there). So the host's view is raw whenever it can be:
/// the bytes must start on 16 (the host's raw view alignment) and be whole
/// dwords. A view that cannot stays structured.
fn raw_window(offset: u64, first: u64, count: u32, stride: u32) -> Option<(u64, u32)> {
    if stride == 0 {
        return None;
    }
    let start = first.checked_mul(u64::from(stride))?.checked_add(offset)?;
    let bytes = u64::from(count) * u64::from(stride);
    (start % 16 == 0 && bytes % 4 == 0)
        .then(|| u32::try_from(bytes / 4).ok().map(|dwords| (start / 4, dwords)))
        .flatten()
}

/// Why the host would refuse a buffer view with these fields of a buffer of
/// `width` bytes, if it would. It removes the device for one it does not
/// take (view creation returns nothing), and the console is laxer: the
/// title's heap holds views past their memory's end, of no elements, of
/// typeless formats without the raw flag, with strides the host has no
/// structured buffers for. Since every buffer view of the heap is made for
/// shaders that pick them by index, one such view ended the run
/// (2026-11-24, run 10). The reason tells a view that loses nothing (no
/// elements) from one whose data the shader does not see.
fn buffer_view_refusal(
    format: DXGI_FORMAT,
    first: u64,
    count: u32,
    stride: u32,
    raw: bool,
    width: u64,
) -> Option<NullView> {
    use crate::resource::family;
    // (first: an empty view loses nothing, whatever else it says)
    if count == 0 {
        return Some(NullView::Empty);
    }
    let f = format.0 as u32;
    let element = if raw {
        // raw: R32 typeless, counted in dwords, starting on 16 bytes
        if format != DXGI_FORMAT_R32_TYPELESS || stride != 0 {
            return Some(NullView::RawFormat);
        }
        if !first.is_multiple_of(4) {
            return Some(NullView::RawStart);
        }
        4
    } else if stride > 0 {
        // structured: no format, a stride the host takes
        if format != DXGI_FORMAT_UNKNOWN || stride > 2048 || !stride.is_multiple_of(4) {
            return Some(NullView::Structured);
        }
        u64::from(stride)
    } else {
        // typed: a format with a size that is not typeless
        let typeless = family(f) == f && family(f + 1) == f;
        match crate::resource::element(f) {
            Some((bytes, 1)) if f != 0 && !typeless && bytes > 0 => bytes,
            _ => return Some(NullView::Typed),
        }
    };
    let fits = first
        .checked_add(u64::from(count))
        .and_then(|end| end.checked_mul(element))
        .is_some_and(|end| end <= width);
    (!fits).then_some(NullView::PastEnd)
}

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<Kept> {
    // (first: an empty view loses nothing, whatever else it says)
    // SAFETY: a buffer view's union member.
    if unsafe { desc.Anonymous.Buffer }.NumElements == 0 {
        if left_null(NullView::Empty, false) {
            tracing::warn!(address, "empty buffer view left null");
        }
        return None;
    }
    let Some(shadow) = crate::shadow::find(address, len) else {
        if left_null(NullView::NoCopy, false) {
            tracing::warn!(address, len, "buffer view of memory with no host copy left null");
        }
        return None;
    };
    let offset = shadow.offset(address);
    // A structured view as a raw view of the same bytes (see `raw_window`).
    // SAFETY: a buffer view's union member.
    let structured = unsafe { desc.Anonymous.Buffer };
    let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
        .then(|| raw_window(offset, structured.FirstElement, structured.NumElements, structured.StructureByteStride))
        .flatten();
    if let Some((first, count)) = window {
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
        desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
            FirstElement: first,
            NumElements: count,
            StructureByteStride: 0,
            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
        };
    }
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    if window.is_none() {
        if element == 0 {
            if left_null(NullView::Typed, false) {
                tracing::warn!(format = desc.Format.0, "buffer view of a format with no element size left null");
            }
            return None;
        }
        if offset % element != 0 {
            if left_null(NullView::OffElement, false) {
                tracing::warn!(offset, element, "buffer view not on an element of its host copy left null");
            }
            return None;
        }
        buffer.FirstElement += offset / element;
    }
    let (first, count, stride) = (buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride);
    let raw = buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
    // SAFETY: a live buffer.
    let width = unsafe { shadow.buffer.GetDesc() }.Width;
    if let Some(why) = buffer_view_refusal(desc.Format, first, count, stride, raw, width) {
        if left_null(why, false) {
            tracing::warn!(
                ?why,
                format = desc.Format.0,
                first,
                count,
                stride,
                raw,
                width,
                "buffer view the host would refuse left null"
            );
        }
        return None;
    }
    // The console has no component mapping; one the host does not know is the default's.
    if desc.Shader4ComponentMapping != D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING {
        desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    Some(Some(shadow.buffer.clone()))
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
/// shortcut: what shaders write stays in the host copy; title memory (and
/// so the CPU, and copies run on the CPU) does not see it.
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<Kept> {
    // (first: an empty view loses nothing, whatever else it says)
    // SAFETY: a buffer view's union member.
    if unsafe { desc.Anonymous.Buffer }.NumElements == 0 {
        if left_null(NullView::Empty, true) {
            tracing::warn!(address, "empty unordered buffer view left null");
        }
        return None;
    }
    let Some(shadow) = crate::shadow::find(address, len) else {
        if left_null(NullView::NoCopy, true) {
            tracing::warn!(address, len, "unordered buffer view of memory with no host copy left null");
        }
        return None;
    };
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        if left_null(NullView::Unwritable, true) {
            tracing::warn!("unordered access to a buffer whose host copy shaders cannot write left null");
        }
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let offset = shadow.offset(address);
    // A structured view as a raw view of the same bytes (see `raw_window`).
    let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
        .then(|| raw_window(offset, buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride))
        .flatten();
    if let Some((first, count)) = window {
        buffer.FirstElement = first;
        buffer.NumElements = count;
        buffer.StructureByteStride = 0;
        buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    } else {
        if element == 0 {
            if left_null(NullView::Typed, true) {
                tracing::warn!(format = desc.Format.0, "unordered buffer view of a format with no element size left null");
            }
            return None;
        }
        if offset % element != 0 {
            if left_null(NullView::OffElement, true) {
                tracing::warn!(offset, element, "unordered buffer view not on an element of its host copy left null");
            }
            return None;
        }
        buffer.FirstElement += offset / element;
    }
    buffer.CounterOffsetInBytes = 0;
    let (first, count, stride) = (buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride);
    let raw = buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
    if window.is_some() {
        // (Left out at first: every structured unordered access view was then
        // refused by `buffer_view_refusal` and left null, runs 15 and 16.)
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
    }
    // SAFETY: a live buffer.
    let width = unsafe { shadow.buffer.GetDesc() }.Width;
    if let Some(why) = buffer_view_refusal(desc.Format, first, count, stride, raw, width) {
        if left_null(why, true) {
            tracing::warn!(
                ?why,
                format = desc.Format.0,
                first,
                count,
                stride,
                raw,
                width,
                "unordered buffer view the host would refuse left null"
            );
        }
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), at)
    };
    Some(Some(shadow.buffer.clone()))
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        if unordered {
            crate::resource::DEPTH_UNORDERED[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    // A view of a target whose format the host refuses, in the title's
    // format: in the stand-in the host copy has.
    if view != own && crate::resource::target_format(view) == own {
        if let Some(format) = format {
            *format = d.Format;
        }
        return true;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

/// The title's unordered access view of a depth texture, as a view of the
/// texture's colour twin `twin` ([`crate::resource::Twin`]): the same mip
/// and slices, in the colour format of the twin's family for a depth or
/// typeless format (R16_UNORM for 16-bit depth, R32_FLOAT for 32-bit; the
/// bits are the same). Without a description: mip 0 of every slice.
/// The twin of a stencil plane (R8) stands for the views of that plane
/// only, which become R8_UINT views of its one plane; `None` for a view of
/// the depth plane, which the twin does not stand for. The depth plane has
/// a twin of its own, viewed as a depth texture's without stencil: a view
/// in the format of the depth plane of a depth-stencil format (21, 46)
/// takes the twin's colour format too.
fn twin_uav(
    twin: &ID3D12Resource,
    desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
) -> Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> {
    // SAFETY: a live resource.
    let d = unsafe { twin.GetDesc() };
    let mut desc = desc.cloned().unwrap_or_else(|| {
        let mut all = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        if d.DepthOrArraySize > 1 {
            all.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            all.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: u32::from(d.DepthOrArraySize),
                ..Default::default()
            };
        }
        all
    });
    // A stencil plane's twin (R8 typeless, 60): the title's view is of the
    // stencil plane if it names plane 1 or a stencil view format (22, 47).
    if d.Format.0 == 60 {
        let mut plane = 0;
        match desc.ViewDimension {
            D3D12_UAV_DIMENSION_TEXTURE2D => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2D };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2D = view;
            }
            D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2DArray };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2DArray = view;
            }
            _ => {}
        }
        if plane != 1 && !matches!(desc.Format.0, 22 | 47) {
            return None;
        }
        desc.Format.0 = 62;
        return Some(desc);
    }
    // None, typeless and depth formats (R16: 0, 53, 55; R32: 0, 39, 40),
    // and those of a depth-stencil format's depth plane (19 to 21, 44 to
    // 46), for the twin of such a plane.
    desc.Format.0 = match (desc.Format.0, d.Format.0) {
        (0 | 53 | 55 | 19..=21 | 44..=46, 53) => 56,
        (0 | 39 | 40 | 19..=21 | 44..=46, 39) => 41,
        (other, _) => other,
    };
    Some(desc)
}

/// Size of a desktop constant-buffer view after applying its limit and alignment.
fn constant_view_size(size: u32) -> u32 {
    super::constant_staging_bytes(size).next_multiple_of(D3D12_CONSTANT_BUFFER_DATA_PLACEMENT_ALIGNMENT)
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            let size = constant_view_size(size);
            if address == 0 || size == 0 {
                return None;
            }
            let shadow = crate::shadow::find(address, u64::from(size))?;
            let gpu = shadow.gpu(address);
            if gpu % 256 != 0 {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return None;
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: size,
            };
            // SAFETY: a slot of our heap; the range lies in the host copy.
            unsafe { device.CreateConstantBufferView(Some(&desc), at) };
            Some(Some(shadow.buffer.clone()))
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(texture) = crate::resource::host_resource(resource) {
                // The title's views of textures with a stencil plane, as it
                // describes them (format, dimension, component mapping, a
                // pad, then first level, levels, plane): each distinct one
                // once. They say how its shaders read the stencil plane
                // (the lighting takes each pixel's material class and its
                // "inside a room" bit from it).
                #[cfg(feature = "diagnostics")]
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() }.Format.0;
                    if matches!(own, 19..=22 | 44..=47) {
                        static SEEN: Mutex<Vec<(i32, Option<[u32; 10]>)>> = Mutex::new(Vec::new());
                        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                        if seen.len() < 64 && !seen.contains(&(own, *desc)) {
                            seen.push((own, *desc));
                            tracing::info!(
                                ?resource,
                                texture_format = own,
                                view = ?desc,
                                "shader view of a depth-stencil texture"
                            );
                        }
                    }
                }
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some(Some(texture.clone()))
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                buffer_srv(data as u64, len, srv_desc(desc), at)
            } else {
                None
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at)
                    }
                    _ => None,
                };
            };
            // The first such views of depth textures are logged as the
            // title describes them (format, dimension, then mip, slices and
            // plane): they say which planes shaders write, and as what.
            if crate::resource::depth_and_unordered(resource) {
                static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 512 {
                    tracing::info!(
                        ?resource,
                        view = ?desc,
                        "unordered access view of a depth texture"
                    );
                }
            }
            let mut desc = desc.as_ref().map(uav_desc);
            // A depth texture's unordered access is its colour twin's: the
            // host gives a depth texture none.
            let twin = crate::resource::twin_resource(resource)
                .and_then(|twin| Some((twin, twin_uav(twin, desc.as_ref())?)))
                // A view of the depth plane of a texture with a stencil
                // plane: of that plane's own twin.
                .or_else(|| {
                    let twin = crate::resource::twin_plane_resource(resource)?;
                    Some((twin, twin_uav(twin, desc.as_ref())?))
                });
            let texture = match twin {
                Some((twin, view)) => {
                    desc = Some(view);
                    twin
                }
                None => texture,
            };
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some(Some(texture.clone()))
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at)
            } else {
                buffer_srv(address, len, srv_desc(words), at)
            }
        }
    }
}

/// Writes of a slot with another view of the placed texture it showed,
/// since the start: the slot kept showing the old view.
pub(crate) static KEPT_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether a slot written with another view of the placed texture it
/// shows keeps showing the old one until the new is made;
/// `VICTORY_KEEP_VIEWS=0` makes it null at once, as before.
fn keep_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_KEEP_VIEWS").is_none_or(|v| v != "0"))
}

/// Whether both are shader views of one texture the title placed in
/// memory: the same address and the same resource description, whatever
/// part of it each shows.
fn same_placed_texture(a: &View, b: &View) -> bool {
    match (a, b) {
        (View::Placed(false, resource, desc), View::Placed(false, other, other_desc)) => {
            resource[0] != 1 && resource == other && desc[10..12] == other_desc[10..12]
        }
        _ => false,
    }
}

/// A shader or unordered access view of a texture the title placed in
/// memory itself, as a view of the resource made for that memory (on first
/// use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    const TARGET_DEPTH_UNORDERED: u32 = 0x7;
    let &View::Placed(unordered, mut resource, ref desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let words = desc[..10].try_into().ok();
    if unordered {
        let texture = crate::resource::uav_texture_at(address, &resource, desc[0])?;
        Some(View::Unordered(texture as usize, 0, words))
    } else {
        // Shader views placed in memory take their texels either from title
        // memory or from `TARGETS_AT` (`resource::alias_of`): clear target
        // flags the title left on the resource description so this view
        // never replaces that target in `TARGETS_AT`.
        resource[12] &= !TARGET_DEPTH_UNORDERED;
        let texture = crate::resource::texture_at(address, &resource)?;
        Some(View::Shader(texture as usize, words))
    }
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        View::Constant(..) => {
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
            // SAFETY: a slot of our heap; a zero location makes a null view.
            return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
        }
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn constant_views_clamp_before_aligning() {
        for (size, expected) in [(0, 0), (1, 256), (320, 512), (65_535, 65_536), (u32::MAX, 65_536)] {
            assert_eq!(super::constant_view_size(size), expected);
        }
    }

    use super::*;

    #[test]
    fn directly_indexed_buffer_ranges_follow_the_heap_lifetime() {
        use std::sync::Arc;

        let mut views = Box::new(BufferViews::new());
        views.written(3, true);
        views.written(7, true);
        let (slots, mut pass) = views.begin_pass();
        assert_eq!(slots, [3, 7]);
        let shadow = Arc::new(());
        let weak = Arc::downgrade(&shadow);
        pass.by_slot.insert(7, (shadow, 16, 48));
        drop(pass);
        // After the first pass, the slots written since and a sixteenth of
        // all in turn: each slot again within 16 passes. The pass's entries
        // stay until a pass revisits their slot (it drops each before making
        // it again).
        let mut seen = std::collections::BTreeSet::new();
        for _ in 1..=16 {
            views.written(3, true);
            let (slots, _pass) = views.begin_pass();
            assert!(slots.contains(&3));
            seen.extend(slots);
        }
        assert_eq!(seen, std::collections::BTreeSet::from([3, 7]));
        assert!(weak.upgrade().is_some());
        let (_, mut pass) = views.begin_pass();

        let shadow = Arc::new(());
        let weak = Arc::downgrade(&shadow);
        pass.by_slot.insert(7, (shadow, 32, 48));
        drop(pass);
        // The new heap's tracking state occupies exactly the old address.
        *views = BufferViews::new();
        assert!(weak.upgrade().is_none());
        let (_, pass) = views.begin_pass();
        assert!(pass.by_slot.is_empty());
        assert_eq!(pass.passes, 1);
    }

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// Views of one placed texture are told from views of others by the
    /// address and the resource description, not by what they show.
    #[test]
    fn views_of_one_placed_texture_are_told_by_address_and_description() {
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], first_mip: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[4], desc[5], desc[10], desc[11]) = (98, 4, first_mip, 8 - first_mip, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let low = view(texture, 3, 0x1000_0000);
        // more mips of the same texture
        assert!(same_placed_texture(&low, &view(texture, 0, 0x1000_0000)));
        // another address, another description, a buffer, an unordered view
        assert!(!same_placed_texture(&low, &view(texture, 3, 0x2000_0000)));
        let mut wider = texture;
        wider[4] = 1024;
        assert!(!same_placed_texture(&low, &view(wider, 3, 0x1000_0000)));
        let mut buffer = texture;
        buffer[0] = 1;
        assert!(!same_placed_texture(&view(buffer, 3, 0x1000_0000), &view(buffer, 3, 0x1000_0000)));
        let View::Placed(_, resource, desc) = low.clone() else { unreachable!() };
        assert!(!same_placed_texture(&low, &View::Placed(true, resource, desc)));
        assert!(!same_placed_texture(&low, &View::Shader(0, None)));
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn a_structured_view_made_raw_passes_the_host_check() {
        // 100 elements of 20 bytes from element 8, in a copy that starts 32
        // bytes before the view's memory: bytes 192..2192.
        let (first, count) = raw_window(32, 8, 100, 20).unwrap();
        assert_eq!((first, count), (48, 500));
        assert_eq!(buffer_view_refusal(DXGI_FORMAT_R32_TYPELESS, first, count, 0, true, 4096), None);
        // the format a raw view must name; the structured one's is refused
        assert_eq!(
            buffer_view_refusal(DXGI_FORMAT_UNKNOWN, first, count, 0, true, 4096),
            Some(NullView::RawFormat)
        );
        // past the buffer, not on 16 bytes, no elements
        assert_eq!(
            buffer_view_refusal(DXGI_FORMAT_R32_TYPELESS, first, count, 0, true, 2000),
            Some(NullView::PastEnd)
        );
        assert!(raw_window(4, 0, 100, 20).is_none());
        assert_eq!(
            buffer_view_refusal(DXGI_FORMAT_R32_TYPELESS, 2, 184, 0, true, 4096),
            Some(NullView::RawStart)
        );
        // an empty view is empty first, whatever else it says
        assert_eq!(buffer_view_refusal(DXGI_FORMAT_R32_TYPELESS, 0, 0, 0, true, 4096), Some(NullView::Empty));
        assert_eq!(buffer_view_refusal(DXGI_FORMAT_UNKNOWN, 3, 0, 0, true, 0), Some(NullView::Empty));
        // a structured view the host takes as it is, and one with a stride it has none for
        assert_eq!(buffer_view_refusal(DXGI_FORMAT_UNKNOWN, 1, 10, 12, false, 4096), None);
        assert_eq!(buffer_view_refusal(DXGI_FORMAT_UNKNOWN, 1, 10, 6, false, 4096), Some(NullView::Structured));
    }

    #[test]
    fn a_texture_recheck_cannot_complete_a_rewritten_slot() {
        let mut queue = TextureQueue::default();
        let now = Instant::now();
        queue.written(7, 1, 1, false);
        let write = queue.due(1, 1).0[0].1;
        queue.uploaded(7, write, (0x10000, 4096), 123, now);
        let due = now + Duration::from_millis(TEXTURE_CHECK_MS[0]);
        let check = queue.due_rechecks(5, due, 1)[0];

        // Seeing the same slot does not invalidate the sample. A real write does.
        queue.written(7, 5, 5, true);
        assert!(queue.slots.is_empty());
        queue.changed(7, 5, 5);
        assert!(!queue.finish_recheck(check, true, 6, due));
        let next_write = queue.due(5, 1).0[0].1;
        assert_ne!(next_write, write);
        assert!(queue.rechecks.is_empty());
    }

    #[test]
    fn texture_rechecks_have_a_separate_deadline_and_frame_budget() {
        let mut queue = TextureQueue::default();
        let now = Instant::now();
        for index in 1..=3 {
            queue.written(index, 1, 1, false);
            let write = queue.due(1, 1).0[0].1;
            queue.uploaded(index, write, (0x10000, 4096), 123, now + Duration::from_millis(u64::from(index)));
        }
        queue.written(4, 2, 2, false);
        let deadline = now + Duration::from_millis(TEXTURE_CHECK_MS[0]);
        assert!(queue.due_rechecks(2, deadline, 2).is_empty());
        assert_eq!(queue.due(u64::MAX, 1).1, [1, 1, 0, 0, u64::MAX - 2]);
        assert_eq!(queue.due(2, 1).0[0].0, 4);

        let now = deadline + Duration::from_millis(10);
        let checks = queue.due_rechecks(2, now, 2);
        assert_eq!(checks.iter().map(|check| check.index).collect::<Vec<_>>(), [1, 2]);
        assert!(queue.due_rechecks(2, now, 2).is_empty());
        assert!(queue.finish_recheck(checks[0], false, 2, now));
        assert_eq!(queue.due_rechecks(3, now, 4).iter()
            .map(|check| check.index).collect::<Vec<_>>(), [2, 3]);
        assert!(queue.remove(2));
        assert!(!queue.finish_recheck(checks[1], true, 3, now));
        assert!(!queue.deadlines.contains_key(&2));
    }

    #[test]
    fn changing_contents_do_not_restart_the_texture_check_budget() {
        let mut queue = TextureQueue::default();
        let mut now = Instant::now();
        queue.written(7, 0, 0, false);
        let mut write = queue.due(0, 1).0[0].1;
        queue.uploaded(7, write, (0x10000, 4096), 1, now);
        for (step, ms) in TEXTURE_CHECK_MS.into_iter().enumerate() {
            let frame = step as u64 + 1;
            now += Duration::from_millis(ms);
            let check = queue.due_rechecks(frame, now, 1)[0];
            assert!(queue.finish_recheck(check, true, frame, now));
            // Handed out again as a newer write, with the checks spent.
            let (index, again) = queue.due(frame, 1).0[0];
            assert_eq!(index, 7);
            assert!(again > write);
            write = again;
            queue.uploaded(7, write, (0x10000, 4096), frame + 1, now);
        }
        assert!(queue.rechecks.is_empty());
        assert!(queue.deadlines.is_empty());
        assert!(queue.slots.is_empty());
    }

    /// A texture a content check finds changed is handed to the next pass
    /// first, as a write of the title that just happened. Under the write
    /// number of its view it waited in the middle of the backlog: a pass
    /// is handed the oldest and the newest writes, 2,048 of each at most.
    #[test]
    fn a_texture_found_changed_by_a_content_check_is_uploaded_next() {
        let mut queue = TextureQueue::default();
        let now = Instant::now();
        // 3,000 slots wait. Slot 7, written after them, is served alone
        // and uploaded whole; 3,000 more are written before its check.
        for index in 1000..4000 {
            queue.written(index, 1, 1, false);
        }
        queue.written(7, 1, 1, false);
        let (ready, _) = queue.due(1, 1);
        let (index, write) = ready[0];
        assert_eq!((ready.len(), index), (1, 7));
        queue.uploaded(7, write, (0x10000, 4096), 123, now);
        for index in 4000..7000 {
            queue.written(index, 2, 2, false);
        }
        // The check finds its memory changed.
        let at = now + Duration::from_millis(TEXTURE_CHECK_MS[0]);
        let check = queue.due_rechecks(3, at, 1)[0];
        assert_eq!(check.index, 7);
        assert!(queue.finish_recheck(check, true, 3, at));
        let (ready, counts) = queue.due(3, 4096);
        assert_eq!((ready.len(), counts[1]), (4096, 6001));
        let position = ready.iter().position(|&(index, _)| index == 7);
        let newer = queue.slots.values().filter(|w| w.write > queue.slots[&7].write).count();
        assert_eq!(position, Some(0), "slot 7, with {newer} newer writes due, handed out at {position:?}");
        // What a pass still at work on the old write says then changes nothing.
        queue.done(7, write);
        assert_eq!(queue.due(3, 1).0[0].0, 7);
    }

    /// The upload queue serves retries first, then alternates the newest and oldest writes.
    #[test]
    fn the_texture_queue_serves_the_newest_writes_first() {
        let mut q = TextureQueue::default();
        // Slots 10..18 written in frames 1..9, slot 5 last.
        for i in 0..8u32 {
            q.written(10 + i, u64::from(i) + 1, 0, false);
        }
        q.written(5, 9, 0, false);
        let order: Vec<u32> = q.due(9, 100).0.into_iter().map(|d| d.0).collect();
        // newest, oldest, newest, oldest, and so on
        assert_eq!(order, [5, 10, 17, 11, 16, 12, 15, 13, 14]);
        assert_eq!(q.due(9, 2).0.len(), 2);
        assert_eq!(q.due(9, 0).1, [9, 9, 0, 0, 8]);
        // Slot 17's memory is not mapped: again next frame, then in 2, 4, 4.
        let write = |q: &TextureQueue, index: u32| {
            q.due(u64::MAX, 100).0.into_iter().find(|d| d.0 == index).unwrap().1
        };
        let w17 = write(&q, 17);
        let mut frame = 9;
        for wait in [1, 2, 4, 4] {
            q.later(17, w17, frame, 4);
            assert!(!q.due(frame + wait - 1, 100).0.iter().any(|d| d.0 == 17));
            frame += wait;
            // Due again, and ahead of every slot never looked at.
            assert_eq!(q.due(frame, 100).0[0].0, 17);
        }
        assert_eq!(q.due(9, 0).1[2], 1);
        q.done(17, w17);
        assert!(!q.due(u64::MAX, 100).0.iter().any(|d| d.0 == 17));
        // Uploaded with part of its memory there: kept, with what was there.
        let w16 = write(&q, 16);
        q.partly(16, w16, 12, 4096);
        assert_eq!(q.seen(16), (4096, 0));
        assert_eq!(q.due(12, 100).0[0].0, 16);
        // The title writes slot 16 again while the pass works on it: what
        // the pass then says about the old write changes nothing.
        q.written(16, 13, 0, false);
        q.done(16, w16);
        q.later(16, w16, 13, 4);
        assert_eq!(q.seen(16).0, 0);
        assert_eq!(q.due(13, 100).0[0].0, 16);
        // A draw that makes the host slot (`seen`) keeps what the queue knows,
        let w15 = write(&q, 15);
        q.partly(15, w15, 20, 8192);
        q.written(15, 14, 0, true);
        assert_eq!(q.seen(15).0, 8192);
        // and queues a slot the queue did not hold.
        q.written(99, 14, 0, true);
        assert!(q.due(14, 100).0.iter().any(|d| d.0 == 99));
        // A write due later (a table's slot) is not handed out before.
        q.written(98, 14, 16, false);
        assert!(!q.due(15, 100).0.iter().any(|d| d.0 == 98));
        assert!(q.due(16, 100).0.iter().any(|d| d.0 == 98));
    }

    /// Dropped notices make the slots that wait on looks due at once (a
    /// texture looked at in vain, one uploaded in part), and leave fresh
    /// writes and table slots in their grace as they are.
    #[test]
    fn dropped_notices_make_the_slots_waiting_on_looks_due() {
        let mut q = TextureQueue::default();
        let write = |q: &TextureQueue, index: u32| {
            q.due(u64::MAX, 100).0.into_iter().find(|d| d.0 == index).unwrap().1
        };
        q.written(1, 0, 0, false);
        q.written(2, 0, 0, false);
        q.written(3, 0, 0, false);
        q.written(4, 0, 500, false);
        let (w2, w3) = (write(&q, 2), write(&q, 3));
        // Slot 2 looked at in vain, twice; slot 3 uploaded in part.
        q.later(2, w2, 0, 1024);
        q.later(2, w2, 1, 1024);
        assert_eq!(q.seen(2), (0, 2));
        q.partly(3, w3, 900, 4096);
        q.looked_due(10);
        let due: Vec<u32> = q.due(10, 100).0.into_iter().map(|d| d.0).collect();
        assert!(due.contains(&2) && due.contains(&3), "{due:?}");
        assert_eq!(q.seen(2), (0, 0));
        assert_eq!(q.seen(3), (4096, 0));
        // The fresh write is due as it was, the table slot not before its frame.
        assert!(due.contains(&1));
        assert!(!due.contains(&4));
    }

    #[test]
    fn a_limited_texture_pass_keeps_the_full_queue_priority_and_counts() {
        let mut q = TextureQueue::default();
        for (frame, slot) in [3, 12, 5, 8, 11, 2, 10, 6, 7, 4, 1, 9].into_iter().enumerate() {
            q.written(slot, frame as u64, 0, false);
        }
        q.later(5, 3, 11, 4);
        q.partly(11, 5, 12, 4096);
        q.later(6, 8, 11, 4);
        q.partly(10, 7, 13, 4096);
        q.written(4, 11, 14, false);

        // Three retries first, newest write first. The remaining seven
        // alternate between newest and oldest, with no slot repeated.
        let priority = [
            (6, 8),
            (11, 5),
            (5, 3),
            (9, 12),
            (3, 1),
            (1, 11),
            (12, 2),
            (7, 9),
            (8, 4),
            (2, 6),
        ];
        for max in 0..=priority.len() + 1 {
            let (ready, counts) = q.due(12, max);
            assert_eq!(ready, priority[..max.min(priority.len())], "max={max}");
            assert_eq!(counts, [12, 10, 2, 2, 12], "max={max}");
        }
    }

    /// Textures of which only the smallest levels are held, packed into
    /// one another's address range (the three of run 188: 0x2c000 bytes
    /// each at full size, their starts 0x2000 and 0x4000 apart): writing
    /// any of them, in any order and again, leaves the others.
    #[test]
    fn packed_neighbours_stay() {
        let mut spans = Spans::default();
        assert_eq!(spans.rule, Rule::Start);
        let at = 0xb_86bf_8000u64;
        let (a, b, c) = (at, at + 0x2000, at + 0x6000);
        assert!(spans.note(105_998, a, a + 0x2c000, true).is_empty());
        assert!(spans.note(106_004, b, b + 0x2c000, true).is_empty());
        assert!(spans.note(108_521, c, c + 0x2c000, true).is_empty());
        // The first written again (as a title does when levels arrive),
        // from another slot too: its span reaches over the other two.
        assert!(spans.note(7, a, a + 0x2c000, true).is_empty());
        assert!(spans.note(106_004, b, b + 0x2c000, true).is_empty());
        let mut all = spans.slots_in(0, u64::MAX);
        all.sort_unstable();
        assert_eq!(all, [7, 105_998, 106_004, 108_521]);
        // A draw makes a slot of the second texture: it is one of them.
        assert!(spans.note(9, b, b + 0x2c000, false).is_empty());
        assert_eq!(spans.by_slot.get(&9), Some(&b));
    }

    /// A texture written where another starts replaces it: at the same
    /// address with another length, or inside the block of its smallest
    /// levels. The packed neighbours of the one replaced stay.
    #[test]
    fn a_texture_written_at_the_start_of_another_replaces_it() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        // Another length from the same start: slot 1 is stale, slot 2 not.
        assert_eq!(spans.note(3, at, at + 0xb000, true), [1]);
        // A start inside the first 4 KiB of slot 3's texture: stale too.
        assert_eq!(spans.note(4, at + 0x800, at + 0x1800, true), [3]);
        // And the other way: a texture that starts just before one.
        assert_eq!(spans.note(5, at + 0x1800, at + 0x4800, true), [2]);
        let mut left = spans.slots_in(0, u64::MAX);
        left.sort_unstable();
        assert_eq!(left, [4, 5]);
        // Exactly 4 KiB apart: both there.
        assert!(spans.note(6, at + 0x2800, at + 0x3800, true).is_empty());
        // A draw's view that clashes is left out, and nothing is stale.
        assert!(spans.note(8, at + 0x900, at + 0x2900, false).is_empty());
        assert_eq!(spans.by_slot.get(&8), None);
        assert_eq!(spans.by_slot.len(), 3);
    }

    /// A slot written with another texture leaves its old span; the same
    /// view written again changes nothing.
    #[test]
    fn a_slot_rewritten_moves_to_its_new_span() {
        let mut spans = Spans::default();
        assert!(spans.note(1, 0x10000, 0x3c000, true).is_empty());
        assert!(spans.note(2, 0x12000, 0x3e000, true).is_empty());
        assert!(spans.note(1, 0x10000, 0x3c000, true).is_empty());
        assert_eq!(spans.slots_in(0x10000, 0x10001), [1]);
        // Slot 1 now views a texture far away.
        assert!(spans.note(1, 0x90000, 0x91000, true).is_empty());
        assert_eq!(spans.slots_in(0x10000, 0x10001), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x12000, 0x12001), [2]);
        assert_eq!(spans.slots_in(0x90000, 0x90001), [1]);
        spans.forget(1);
        spans.forget(2);
        assert!(spans.by_start.is_empty() && spans.by_slot.is_empty());
    }

    /// Memory the runtime maps, unmaps or writes names the textures that
    /// are there: among packed neighbours the one whose held levels the
    /// range lies in (it ends where the next one starts), and the ones
    /// that start inside the range.
    #[test]
    fn memory_changed_under_packed_neighbours_names_the_ones_there() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        assert!(spans.note(3, at + 0x6000, at + 0x32000, true).is_empty());
        // Inside the first one's held levels.
        assert_eq!(spans.slots_in(at + 0x1000, at + 0x2000), [1]);
        // Inside the second's: not the first, which ends where it starts.
        assert_eq!(spans.slots_in(at + 0x3000, at + 0x4000), [2]);
        // Past the last start: the last one.
        assert_eq!(spans.slots_in(at + 0x20000, at + 0x21000), [3]);
        // A range over all of them.
        assert_eq!(spans.slots_in(at, at + 0x8000), [1, 2, 3]);
        // Past the end of all: none.
        assert_eq!(spans.slots_in(at + 0x32000, at + 0x33000), [] as [u32; 0]);
    }

    /// `VICTORY_STALE_TEXTURES=0`: nothing is ever stale, and a view that
    /// overlaps others is left out of the spans, as before.
    #[test]
    fn never_stale_leaves_overlapping_views_out() {
        let mut spans = Spans { rule: Rule::Never, ..Default::default() };
        assert!(spans.note(1, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(2, 0x2000, 0x4000, true).is_empty());
        assert!(spans.note(3, 0x1000, 0x2000, true).is_empty());
        assert_eq!(spans.slots_in(0, u64::MAX), [1]);
    }

    /// The rule until run 188 (`VICTORY_STALE_TEXTURES=overlap`): a view
    /// written over memory that older views describe otherwise makes
    /// those stale; views of the same span are one texture's.
    #[test]
    fn a_newer_view_of_other_memory_makes_the_older_stale() {
        let mut spans = Spans { rule: Rule::Overlap, ..Default::default() };
        // Three textures side by side, the first in two slots.
        assert!(spans.note(1, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(2, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(3, 0x3000, 0x4000, true).is_empty());
        assert!(spans.note(4, 0x8000, 0x9000, true).is_empty());
        // The same slot written with the same span again: nothing changes.
        assert!(spans.note(2, 0x1000, 0x3000, true).is_empty());
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        assert_eq!(spans.slots_in(0x4000, 0x8000), [] as [u32; 0]);
        // A draw makes a slot whose view overlaps them: left out, nothing stale.
        assert!(spans.note(9, 0x2000, 0x3800, false).is_empty());
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        // The title writes a texture over the end of the first and all of
        // the second: their three slots are stale.
        let mut stale = spans.note(5, 0x2000, 0x4000, true);
        stale.sort_unstable();
        assert_eq!(stale, [1, 2, 3]);
        assert_eq!(spans.slots_in(0, u64::MAX), [5, 4]);
        // Another start in the same memory: stale in turn. Another length
        // from the same start too.
        assert_eq!(spans.note(6, 0x2800, 0x3000, true), [5]);
        assert_eq!(spans.note(7, 0x2800, 0x2c00, true), [6]);
        // A slot that moves to another texture leaves its span behind.
        assert!(spans.note(7, 0x8000, 0x9000, true).is_empty());
        assert_eq!(spans.slots_in(0x2800, 0x2c00), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x8000, 0x8001), [4, 7]);
        spans.forget(4);
        spans.forget(7);
        assert!(spans.by_start.is_empty() && spans.by_slot.is_empty());
    }

    /// Memory the runtime maps under a slot's texture makes the slot due
    /// again from the next frame; a stale slot leaves the queue.
    #[test]
    fn remapped_memory_makes_its_texture_slots_due() {
        let _guard = REMAP_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let slots = |at: u64| -> Vec<u32> { heap.texture_slots_due(at, 8).0.into_iter().map(|d| d.0).collect() };
        // 64x64 BC7, one mip, at an address no test and no title uses.
        let at = 0x7fee_0000_0000u64;
        let view = |address: u64, width: u32| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            View::Placed(false, [3, 0, 0, 0, width, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0], desc)
        };
        let len = crate::resource::placed_bytes(&[3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0]).unwrap();
        assert!(heap.note_buffer(1, Some(&view(at, 64)), false).is_empty());
        let (queued, write) = heap.texture_slots_due(u64::MAX, 8).0[0];
        assert_eq!(queued, 1);
        heap.texture_slot_done(1, write, None);
        assert_eq!(slots(u64::MAX), []);
        // Memory elsewhere: nothing. Under the texture: due from the next frame.
        remapped((at + 2 * len) as usize, 0x1000);
        remapped(at as usize, 0x1000);
        assert_eq!(heap.texture_slots_remapped(50), 1);
        assert_eq!(slots(50), []);
        assert_eq!(slots(51), [1]);
        // A wider texture written at the same address: slot 1 is stale.
        assert_eq!(heap.note_buffer(2, Some(&view(at, 128)), false), [1]);
        assert_eq!(slots(u64::MAX), [2]);
    }

    static REMAP_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn overflowing_remaps_requeue_a_texture_that_had_finished() {
        let _guard = REMAP_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let at = 0x7fed_0000_0000u64;
        for (slot, address) in [(2, at - 0x20000), (3, at)] {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            let view = View::Placed(false, [3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0], desc);
            assert!(heap.note_buffer(slot, Some(&view), false).is_empty());
        }
        let queued = heap.texture_slots_due(u64::MAX, 8).0;
        assert_eq!(queued.len(), 2);
        for (slot, write) in queued {
            heap.texture_slot_done(slot, write, None);
        }
        assert!(heap.texture_slots_due(u64::MAX, 8).0.is_empty());

        // The only matching range is recorded before overflow. The other
        // ranges do not touch the texture and cannot requeue it on their own.
        remapped(at as usize, 4096);
        for _ in 0..REMAPPED_LIMIT {
            remapped((at - 0x10000) as usize, 4096);
        }
        assert!(REMAPPED.lock().unwrap().full_rescan);
        assert_eq!(heap.texture_slots_remapped(50), 2);
        assert!(heap.texture_slots_due(50, 8).0.is_empty());
        let mut slots: Vec<_> = heap.texture_slots_due(51, 8).0.into_iter().map(|(slot, _)| slot).collect();
        slots.sort_unstable();
        assert_eq!(slots, [2, 3]);
        assert_eq!(heap.texture_slots_remapped(52), 0);
    }

    /// A slot a draw read through a table leaves the queue; the title's
    /// next write of it waits for its draw, and is due after the grace
    /// frames if none reads it.
    #[test]
    fn slots_read_through_tables_leave_the_texture_queue() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let frame = 100;
        let slots = |at: u64| -> Vec<u32> { heap.texture_slots_due(at, 8).0.into_iter().map(|d| d.0).collect() };
        let texture = View::Placed(false, [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 28, 1, 0, 0x105, 0, 0], [0; 16]);
        assert!(is_texture_view(&texture));
        heap.queue_texture(2, true, false, frame);
        heap.queue_texture(3, true, false, frame);
        assert_eq!(slots(frame), [3, 2]);
        heap.tabled_in(3, true, frame);
        assert_eq!(slots(frame), [2]);
        // Made in place by the draw that read it: still out.
        heap.queue_texture(3, true, true, frame);
        assert_eq!(slots(frame), [2]);
        // Written again in the next frame: due once the grace frames have
        // passed without a draw that reads it.
        heap.queue_texture(3, true, false, frame + 1);
        assert_eq!(slots(frame + TABLED_GRACE), [2]);
        assert_eq!(slots(frame + 1 + TABLED_GRACE), [3, 2]);
        // The draw comes after all: out again.
        heap.tabled_in(3, true, frame + 1);
        assert_eq!(slots(u64::MAX), [2]);
        // A slot of a table no draw read stays queued; its next write waits.
        heap.tabled_in(2, false, frame + 1);
        assert_eq!(slots(frame + 1), [2]);
        heap.queue_texture(2, true, false, frame + 2);
        assert_eq!(slots(frame + 2), []);
        // Long after the last table, a write is due at once.
        heap.queue_texture(2, true, false, frame + 3 + TABLED_FRAMES);
        assert_eq!(slots(frame + 3 + TABLED_FRAMES), [2]);
    }

    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    #[test]
    fn dynamic_allocation_finds_a_free_range_across_the_cursor() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let left = heap.dynamic(6).unwrap();
        let right = heap.dynamic(6).unwrap();
        assert_eq!(heap.dynamic(heap.dynamic - 12), Some(right + 6));
        heap.release(left, 6);
        assert_eq!(heap.dynamic(6), Some(left));
        // The cursor is between the two blocks, with no free slots elsewhere.
        heap.release(left, 6);
        heap.release(right, 6);
        assert_eq!(heap.dynamic(10), Some(left));
    }

    /// Placed unordered texture views (2D and 3D) mirror onto host unordered
    /// access textures when written into a shader-visible heap, reuse an
    /// existing target at the same address, and feed shader views placed at
    /// that address in another format through `texture::alias`.
    #[test]
    fn placed_unordered_texture_view_mirrors_and_aliases_to_shader_view() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 8) else {
            return;
        };

        // 1. 3D volume UAV (4x4x4 R32_UINT, resource flags = 0) mirrored via
        // `HostHeap::mirror(..., for_draw = false)` as a shader-visible heap
        // copy does.
        let vol_at = 0x7fed_1000_0000u64;
        let vol_res = [4, 0, 0, 0, 4, 0, 4, 0x1_0004, 42, 1, 0, 0, 0, 0];
        let mut vol_uav = [0u32; 16];
        (vol_uav[0], vol_uav[1], vol_uav[4]) = (42, 8, 4);
        (vol_uav[10], vol_uav[11]) = (vol_at as u32, (vol_at >> 32) as u32);
        let slot_mem = [0u64; 4];
        let slot_handle = slot_mem.as_ptr() as u64;
        super::super::create_placed_uav(std::ptr::null(), &vol_res, &vol_uav, slot_handle);
        assert!(heap.mirror(0, slot_handle, false).is_some());
        {
            let kept = heap.kept.lock().unwrap();
            let tex = kept[0].as_ref().expect("3D placed UAV must mirror immediately");
            // SAFETY: live D3D12 resource.
            let d = unsafe { tex.GetDesc() };
            assert_eq!(d.Dimension, D3D12_RESOURCE_DIMENSION_TEXTURE3D);
            assert_ne!(d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0, 0);
        }
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());

        // Another placed UAV at the same address with compatible format reuses
        // the resource already recorded in `TARGETS_AT`.
        let Some(View::Unordered(first_vol, _, _)) =
            placed_texture(&View::Placed(true, vol_res, vol_uav))
        else {
            panic!("expected View::Unordered");
        };
        let mut explicit_flags = vol_res;
        explicit_flags[12] = 0x4;
        let Some(View::Unordered(second_vol, _, _)) =
            placed_texture(&View::Placed(true, explicit_flags, vol_uav))
        else {
            panic!("expected View::Unordered");
        };
        assert_eq!(first_vol, second_vol, "must reuse existing UAV texture at address");

        // 2. Cross-format aliasing: a 4x4 R9G9B9E5_SHAREDEXP (67) description
        // written through an R32_UINT (42) placed UAV and then read through an
        // R9G9B9E5_SHAREDEXP placed SRV (even if `ALLOW_UNORDERED_ACCESS` was
        // left on the SRV's resource description) copies the GPU texels across.
        let alias_at = 0x7fed_2000_0000u64;
        let rgb9e5_res = [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 67, 1, 0, 0, 0x4, 0];
        let mut uav_desc = [0u32; 16];
        (uav_desc[0], uav_desc[1]) = (42, 4);
        (uav_desc[10], uav_desc[11]) = (alias_at as u32, (alias_at >> 32) as u32);
        let uav_placed = View::Placed(true, rgb9e5_res, uav_desc);
        let Some(View::Unordered(uav_obj, _, _)) = placed_texture(&uav_placed) else {
            panic!("expected placed UAV texture");
        };
        assert!(make(&uav_placed, heap.cpu(1)).flatten().is_some());

        let mut srv_words = [0u32; 16];
        (srv_words[0], srv_words[1], srv_words[2], srv_words[5]) =
            (67, 4, D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, 1);
        (srv_words[10], srv_words[11]) = (alias_at as u32, (alias_at >> 32) as u32);
        let srv_placed = View::Placed(false, rgb9e5_res, srv_words);
        let Some(View::Shader(srv_obj, _)) = placed_texture(&srv_placed) else {
            panic!("expected placed SRV texture");
        };
        assert!(make(&srv_placed, heap.cpu(2)).flatten().is_some());
        assert_eq!(
            crate::resource::alias_of(srv_obj as crate::object::This),
            Some(uav_obj as crate::object::This),
            "placed SRV must find the placed UAV texture in TARGETS_AT"
        );

        // Upload a known R32_UINT pattern into `uav_obj` on the GPU, run
        // `texture::alias` into `srv_obj`, and read `srv_obj` back.
        let uav_tex = crate::resource::host_resource(uav_obj as crate::object::This).unwrap();
        let srv_tex = crate::resource::host_resource(srv_obj as crate::object::This).unwrap();
        let pattern = 0x7c40_8210u32;
        let make_buf = |heap_type, state| {
            let hp = D3D12_HEAP_PROPERTIES {
                Type: heap_type,
                ..Default::default()
            };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 1024,
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
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: valid buffer description.
            unsafe {
                host.device
                    .CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &rd, state, None, &mut out)
                    .unwrap();
            }
            out.unwrap()
        };
        let upload = make_buf(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        let readback = make_buf(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
        // SAFETY: 1024-byte upload buffer mapped on CPU.
        unsafe {
            let mut ptr = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut ptr)).unwrap();
            std::slice::from_raw_parts_mut(ptr.cast::<u32>(), 256).fill(pattern);
            upload.Unmap(0, None);
        }

        let list = host.new_list().unwrap();
        crate::list::ensure_state(&list, uav_obj as crate::object::This, 0x400); // COPY_DEST
        let buf_loc = |buf: &ID3D12Resource, format| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed pointer not released by the copy location.
            pResource: unsafe { std::mem::transmute_copy(buf) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: 4,
                        Height: 4,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        let tex_loc = |tex: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed pointer not released by the copy location.
            pResource: unsafe { std::mem::transmute_copy(tex) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        // SAFETY: open list and live resources in matching copy states.
        unsafe {
            list.list.CopyTextureRegion(
                &tex_loc(uav_tex),
                0,
                0,
                0,
                &buf_loc(&upload, DXGI_FORMAT_R32_UINT),
                None,
            );
        }
        crate::list::ensure_state(&list, uav_obj as crate::object::This, 0x8); // UNORDERED_ACCESS
        assert!(crate::texture::alias(
            &list,
            srv_obj as crate::object::This,
            uav_obj as crate::object::This
        ));
        crate::list::ensure_state(&list, srv_obj as crate::object::This, 0x800); // COPY_SOURCE
        // SAFETY: open list and `srv_tex` transitioned to `COPY_SOURCE`.
        unsafe {
            list.list.CopyTextureRegion(
                &buf_loc(&readback, DXGI_FORMAT_R9G9B9E5_SHAREDEXP),
                0,
                0,
                0,
                &tex_loc(srv_tex),
                None,
            );
        }
        list.close();
        host.execute(&[&list]);

        // SAFETY: GPU execution finished; readback holds 1024 bytes.
        let got = unsafe {
            let mut ptr = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut ptr)).unwrap();
            let word = *ptr.cast::<u32>();
            readback.Unmap(0, None);
            word
        };
        assert_eq!(got, pattern);
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
}
