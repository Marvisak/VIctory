//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (Grand Theft
/// Auto VI's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

/// Whether the host hands this request a write-watched reservation: it does
/// for the reservations it places itself (`constrained`), except placeholders,
/// whose physical-page views the GPU side copies whole. Reads the decision,
/// not the flags: they travel in `kind` today and in `alloc_kind` once lazy
/// commit lands (#3438), so keying off the bits would stop tracking silently.
fn host_write_watches(constrained: bool, placeholder: bool) -> bool {
    constrained && !placeholder
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (Grand Theft Auto VI's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let lazy_eligible = !placeholder && lazy_commit::eligible(size, kind, protect);
    let alloc_kind = if lazy_eligible {
        kind & !MEM_COMMIT
    } else {
        kind
    };
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    let mut alloc_kind = alloc_kind;
    if constrained {
        alloc_kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if alloc_kind & MEM_RESERVE_PLACEHOLDER == 0 {
            alloc_kind |= MEM_WRITE_WATCH;
        }
    }
    if lazy_eligible
        && !constrained
        && kind & MEM_RESERVE == 0
        && (base as usize).is_multiple_of(lazy_commit::PAGE)
        && size.is_multiple_of(lazy_commit::PAGE)
        && lazy_commit::is_non_placeholder_reservation(base as usize, size)
    {
        // Committing into an existing non-placeholder reservation: mark exact
        // 64 KiB pages lazy without committing upfront or touching placeholders.
        lazy_commit::mark_range(base as usize, size);
        return base;
    }
    let eager_recommit = !constrained && kind & MEM_COMMIT != 0 && !base.is_null();
    if eager_recommit {
        // If an eager commit (e.g. non-RW protect or < 4 MiB) overlaps a lazy
        // 64 KiB block partially, commit boundary blocks first so sibling 4 KiB
        // pages stay live while VirtualAlloc2 applies `protect` to `[base, base+size)`.
        lazy_commit::commit_partial_boundaries(base as usize, size);
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let allocate = |parameter: &mut MEM_EXTENDED_PARAMETER| unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            if constrained { alloc_kind } else { kind },
            protect,
            if constrained {
                parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    let mut at = allocate(&mut parameter);
    // A 32-bit request that found no room below 4 GiB: the guards there
    // make way for it.
    if at.is_null() && constrained && highest < FOUR_GIB {
        at = low_guards::retry(size, || allocate(&mut parameter));
    }
    if !at.is_null() && eager_recommit {
        lazy_commit::unmark_aligned_interior(base as usize, size);
    }
    if !at.is_null() && constrained && !placeholder {
        lazy_commit::record_reservation(at as usize, size);
        if lazy_eligible {
            lazy_commit::mark_range(at as usize, size);
        }
    }
    if !at.is_null() && host_write_watches(constrained, placeholder) {
        touch_diag::track(at as usize, size);
    }
    if at.is_null() {
        let error = std::io::Error::last_os_error();
        if kind & MEM_COMMIT != 0 {
            commit_refused(size, "virtual memory", error);
        } else {
            tracing::warn!(
                size,
                kind = format_args!("{kind:#x}"),
                flags = format_args!("{flags:#x}"),
                %error,
                "XMemVirtualAlloc refused"
            );
        }
    }
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    at
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (Grand Theft Auto VI's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        if info.host.State == MEM_RESERVE {
            let query_base = info.host.BaseAddress as usize;
            let region_end = query_base.saturating_add(info.host.RegionSize);
            if let Some((marked, run_end)) = lazy_commit::query_run(query_base, region_end) {
                info.host.RegionSize = run_end.saturating_sub(query_base);
                if marked {
                    info.host.State = MEM_COMMIT;
                    info.host.Protect = PAGE_READWRITE;
                }
            }
        }
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (Grand Theft Auto VI does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (Grand Theft Auto VI uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        tracing::warn!(size, "XMemAllocDefault: the host heap is out of memory");
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            let addr = address as usize;
            let span = query_allocation_span(addr);
            if VirtualFree(address, 0, MEM_RELEASE) != 0 {
                on_virtual_free_release(addr, span);
            }
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

struct Section(HANDLE);

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.0) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

#[derive(Default)]
struct Physical {
    next_page: usize,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Where each slice (section, page index) is mapped: the addresses of
    /// its page in every view showing it (the GPU side's change tracking
    /// follows writes through one view to the others, `aliases`).
    shown: HashMap<(usize, usize), Vec<usize>>,
}

impl Slice {
    /// The key of page `k` of the slices from this one on in `shown`.
    fn key(&self, k: usize) -> (usize, usize) {
        (Arc::as_ptr(&self.section) as usize, self.index + k)
    }
}

/// Every address of a title view showing a page that title memory at
/// `[at, at + len)` shows, except the page's own address: pushed to `out`
/// as (page address, other address). The GPU side's change tracking
/// (`victory_gpu::watch`) marks writes there too.
pub fn aliases(at: usize, len: usize, out: &mut Vec<(usize, usize)>) {
    let physical = physical();
    let end = at.saturating_add(len);
    let first = physical
        .views
        .range(..=at)
        .next_back()
        .filter(|(base, (size, _))| *base + size > at)
        .map(|(base, _)| *base);
    let starts = first.into_iter().chain(physical.views.range(at.saturating_add(1)..end).map(|(b, _)| *b));
    for base in starts.collect::<Vec<_>>() {
        let (size, slice) = &physical.views[&base];
        let (from, to) = (at.max(base), end.min(base + size));
        for page in (from / PAGE..to.div_ceil(PAGE)).map(|p| p * PAGE) {
            let k = (page - base) / PAGE;
            if let Some(addresses) = physical.shown.get(&slice.key(k)) {
                out.extend(addresses.iter().filter(|&&a| a != page).map(|&a| (page, a)));
            }
        }
    }
}

/// The addresses of title views showing `pages` (page numbers): writes into
/// them through our own views must be reported there.
fn shown_at(pages: &[usize]) -> Vec<usize> {
    let physical = physical();
    pages
        .iter()
        .filter_map(|p| physical.pages.get(p))
        .filter_map(|slice| physical.shown.get(&slice.key(0)))
        .flatten()
        .copied()
        .collect()
}

/// Reports writes into physical pages through our own views at every title
/// view showing them (DirectStorage's page destinations).
fn report_written(shown: &[usize]) {
    for &page in shown {
        #[cfg(test)]
        REPORTED.with(|r| r.borrow_mut().push(page));
        drop(victory_gpu::kernel_will_write(page, PAGE));
    }
}

#[cfg(test)]
thread_local! {
    /// The title addresses this thread's writes into physical pages reported.
    static REPORTED: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        // Page numbers are opaque to titles; 0 stays unused.
        next_page: 0x10_0000,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    // A pagefile-backed section of `size` bytes, its pages committed when
    // first mapped or written (`commit`): Grand Theft Auto VI asks for 6.1
    // GiB at once, which a busy host often cannot commit (error 1450).
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    let section = Arc::new(Section(handle));
    let mut physical = physical();
    let first = physical.next_page;
    physical.next_page += n * FRAMES;
    for index in 0..n {
        let slice = Slice {
            section: section.clone(),
            index,
        };
        let number = first + index * FRAMES;
        physical.pages.insert(number, slice);
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(number) };
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: Grand Theft Auto VI keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// How many of `pages` are no allocated page (diagnostics: a DirectStorage
/// destination that could not be written).
pub(crate) fn unknown_pages(pages: &[usize]) -> usize {
    let physical = physical();
    pages.iter().filter(|p| !physical.pages.contains_key(p)).count()
}

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.pages.remove(&unsafe { *pages.add(i) });
    }
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.0,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            return None;
        }
        let view = View { at, len };
        commit(at.Value as usize, len).then_some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table, which the
/// title's own mapping calls share with every DirectStorage worker. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let physical = physical();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let slice = physical.pages.get(page)?;
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let Some(mut view) = View::map(slice, *n) else {
            return false;
        };
        let k = (n * PAGE - within).min(data.len() - done);
        view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        (within, done) = (0, done + k);
    }
    // Where the pages show once written (see `PageWindow`).
    report_written(&shown_at(pages.get(offset / PAGE..(offset + data.len()).div_ceil(PAGE)).unwrap_or(&[])));
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: View,
    within: usize,
    len: usize,
    /// The pages (page numbers) the bytes reach: the title views showing
    /// them when the window is done are told. Looked up then, not when the
    /// window opens: a decode takes milliseconds, and the title may map a
    /// page at another address meanwhile (its GPU heap); a copy that took
    /// the page there before the bytes landed would never hear of them. An
    /// address that stops showing a page meanwhile was told by the unmap.
    pages: Vec<usize>,
}

impl Drop for PageWindow {
    fn drop(&mut self) {
        report_written(&shown_at(&self.pages));
    }
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        &mut self.view.bytes()[within..within + len]
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    Some(PageWindow {
        view: View::map(slice, *n)?,
        within: offset % PAGE,
        len,
        pages: pages.get(offset / PAGE..(offset + len).div_ceil(PAGE)).unwrap_or(&[]).to_vec(),
    })
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    let mut physical = physical();
    unmap(&mut physical, base, n * PAGE);
    if pages.is_null() {
        return address;
    }
    // SAFETY: the title passes `n` page numbers.
    let pages = unsafe { std::slice::from_raw_parts(pages, n) };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return std::ptr::null_mut();
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + pages[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (Grand Theft Auto VI passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    for i in 0..n {
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        let (address, page) = unsafe {
            (
                *addresses.add(i),
                if pages.is_null() || *pages.add(i) == 0 {
                    std::ptr::null()
                } else {
                    pages.add(i)
                },
            )
        };
        if XMemMapPhysicalPages(address, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`): Grand Theft Auto
/// VI maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let mut physical = physical();
    unmap(&mut physical, to, len);
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let at = from + k;
        let Some(slice) = physical
            .views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
        else {
            return false;
        };
        if !map(&mut physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    // SAFETY: splits the placeholder around the range (failing harmlessly
    // when the range is the whole placeholder), then maps the section
    // there; both act only on placeholders.
    let view = unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        );
        MapViewOfFile3(
            slice.section.0,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value as usize != base {
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    let has_alias = (0..len / PAGE).any(|k| {
        physical
            .shown
            .get(&slice.key(k))
            .is_some_and(|addrs| !addrs.is_empty())
    });
    if lazy_commit::physical_enabled() && !on_wine() && !has_alias {
        lazy_commit::mark_range(base, len);
    } else {
        if has_alias {
            for k in 0..len / PAGE {
                if let Some(addrs) = physical.shown.get(&slice.key(k)) {
                    for &other in addrs {
                        lazy_commit::ensure(other, PAGE);
                    }
                }
            }
        }
        if !commit(base, len) {
            // SAFETY: the view just mapped, back to a placeholder.
            unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
            return false;
        }
    }
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    for k in 0..len / PAGE {
        let shown = physical.shown.entry(slice.key(k)).or_default();
        // Pages shown elsewhere already: guarded here too if they are there.
        victory_gpu::title_memory_aliased(base + k * PAGE, shown);
        shown.push(base + k * PAGE);
    }
    physical.views.insert(base, (len, slice));
    true
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    commit_refused(len, "physical pages", std::io::Error::last_os_error());
    false
}

/// Reports memory the host would not give: at the Windows commit limit to
/// `crashes.log`, since the title usually faults soon after on the null it
/// gets; otherwise as a warning.
fn commit_refused(len: usize, what: &str, error: std::io::Error) {
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        victory_loader::log_crash(&commit_limit_message(
            len,
            what,
            &error,
            system_commit().as_ref(),
        ));
    } else {
        tracing::warn!(len, %error, "committing {what} failed");
    }
}

/// Reports to `crashes.log` that the host allocator refused `len_bytes` of
/// APU heap memory. There is no Windows error code to give: `std::alloc`
/// allocates with `HeapAlloc`, which does not set one when it fails.
pub(crate) fn report_allocator_refusal(len_bytes: usize) {
    victory_loader::log_crash(&allocator_refusal_message(
        len_bytes,
        system_commit().as_ref(),
    ));
}

/// The system commit, if Windows reports it.
fn system_commit() -> Option<PERFORMANCE_INFORMATION> {
    let mut info = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
    (unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0).then_some(info)
}

fn commit_limit_message(
    len: usize,
    what: &str,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    refusal_message(
        format_args!(
            "Windows commit limit reached while committing {len} bytes of {what}: {error}"
        ),
        info,
    )
}

fn allocator_refusal_message(len_bytes: usize, info: Option<&PERFORMANCE_INFORMATION>) -> String {
    refusal_message(
        format_args!("the host allocator refused {len_bytes} bytes of APU heap memory"),
        info,
    )
}

/// What the host refused, then the system commit and what the user can do.
fn refusal_message(
    refusal: std::fmt::Arguments<'_>,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!("VIctory: {refusal}.\n");
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        for k in 0..size / PAGE {
            let key = slice.key(k);
            if let Some(addresses) = physical.shown.get_mut(&key) {
                addresses.retain(|&a| a != at + k * PAGE);
                if addresses.is_empty() {
                    physical.shown.remove(&key);
                }
            }
        }
        lazy_commit::unmark_range(at, size);
        victory_gpu::title_memory_remapped(at, size);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn touch_diag_keys_off_the_hosts_decision_not_the_flags() {
        // The host makes a request a write-watched reservation when it picks
        // the address itself (`constrained`), unless it is a placeholder.
        // Recompute the bits the host sets today (`kind`) and check the
        // diagnostic's guard agrees, so moving them (to `alloc_kind`, #3438)
        // cannot silently stop the tracking.
        for constrained in [false, true] {
            for placeholder in [false, true] {
                let mut kind = 0u32;
                if placeholder {
                    kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
                }
                if constrained {
                    kind |= MEM_RESERVE;
                    if kind & MEM_RESERVE_PLACEHOLDER == 0 {
                        kind |= MEM_WRITE_WATCH;
                    }
                }
                assert_eq!(
                    host_write_watches(constrained, placeholder),
                    kind & MEM_RESERVE != 0 && kind & MEM_WRITE_WATCH != 0,
                    "constrained {constrained}, placeholder {placeholder}"
                );
            }
        }
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, "physical pages", &error, Some(&info));
        assert!(report.contains("4194304 bytes of physical pages"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, "virtual memory", &error, None);
        assert!(fallback.contains("4194304 bytes of virtual memory"));
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn allocator_refusal_report_names_apu_heap_and_system_commit() {
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = allocator_refusal_message(4 * MIB, Some(&info));
        assert!(report.contains("host allocator refused 4194304 bytes of APU heap memory"));
        assert!(report.contains("System commit: 2 MiB of 3 MiB in use"));
        assert!(report.contains("Windows paging file size"));
        let fallback = allocator_refusal_message(4 * MIB, None);
        assert!(fallback.contains("4194304 bytes of APU heap memory"));
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Each mapping knows the other (the GPU side's change tracking).
        let others = |p: *mut c_void| {
            let mut out = Vec::new();
            aliases(p as usize, PAGE, &mut out);
            out.into_iter().map(|(_, a)| a).collect::<Vec<_>>()
        };
        assert_eq!(others(at(a, 2)), [at(b, 1) as usize]);
        assert_eq!(others(at(b, 1)), [at(a, 2) as usize]);
        assert!(others(a).is_empty());
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        let mut both = others(at(a, 2));
        both.sort_unstable();
        assert_eq!(both, [b as usize, at(b, 1) as usize]);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(others(at(a, 2)), [at(b, 1) as usize], "unmapped views are forgotten");
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = physical().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// A page mapped at a second address while a window writes it is
    /// reported there too when the window is done (the title maps streamed
    /// pages into its GPU heap while their reads decode); an address that
    /// stopped showing it meanwhile is not.
    #[test]
    fn page_windows_report_where_the_pages_show_when_done() {
        let pages = allocate(1);
        let reserve = || XMemVirtualAlloc(std::ptr::null_mut(), PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 1, pages.as_ptr()), a);
        REPORTED.with(|r| r.borrow_mut().clear());
        let mut window = page_window(&pages, 0, 16).expect("one run");
        // Mapped at `b`, unmapped from `a`, while the bytes are written.
        assert_eq!(XMemMapPhysicalPages(b, 1, pages.as_ptr()), b);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        window.bytes().fill(3);
        drop(window);
        assert_eq!(REPORTED.with(|r| r.borrow().clone()), [b as usize]);
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 3);
        // `write_physical` the same way.
        REPORTED.with(|r| r.borrow_mut().clear());
        assert!(write_physical(&pages, 0, &[4]));
        assert_eq!(REPORTED.with(|r| r.borrow().clone()), [b as usize]);
        assert_eq!(XMemMapPhysicalPages(b, 1, std::ptr::null()), b);
        assert_eq!(XMemFreePhysicalPages(1, pages.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }
}

/// The space below 4 GiB, which the title's 32-bit memory (`XMemVirtualAlloc`
/// with the 32-bit address flag) has to fit in.
const FOUR_GIB: usize = 1 << 32;

/// Guards over the space below 4 GiB. Windows places allocations bottom up,
/// so the host's own memory (the driver's, heaps, the shader and pipeline
/// caches read at startup) filled it first: with a large pipeline cache only
/// 22 MB was left, in pieces, when Grand Theft Auto VI asked for a 185 MB
/// 32-bit placeholder at startup, the request was refused
/// (`ERROR_NOT_ENOUGH_MEMORY`) and the title stopped at an assertion. The
/// debug layer's memory did the same. So the free space below 4 GiB is
/// reserved at startup, past [`low_guards::SLACK`] left for the host, and a
/// 32-bit request that finds no room releases guards and tries again.
pub mod low_guards {
    use std::sync::Mutex;

    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualAlloc,
        VirtualFree, VirtualQuery,
    };

    use super::FOUR_GIB;

    /// Free space left to the host below 4 GiB (the lowest free space).
    pub(crate) const SLACK: usize = 256 << 20;
    /// The allocation granularity of Windows.
    const GRANULE: usize = 64 << 10;

    /// The guards, `(base, bytes)`.
    static GUARDS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

    fn guards() -> std::sync::MutexGuard<'static, Vec<(usize, usize)>> {
        GUARDS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The free runs in `[from, to)`, granule-aligned.
    fn free_runs(from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut runs = Vec::new();
        let mut at = from.max(GRANULE);
        while at < to {
            // SAFETY: plain data filled in by the query.
            let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            if unsafe { VirtualQuery(at as *const _, &mut m, size_of_val(&m)) } == 0 {
                break;
            }
            let base = m.BaseAddress as usize;
            let end = (base + m.RegionSize).min(to);
            if m.State == MEM_FREE {
                let (start, end) = (
                    base.max(from).next_multiple_of(GRANULE),
                    end & !(GRANULE - 1),
                );
                if end > start {
                    runs.push((start, end - start));
                }
            }
            at = base + m.RegionSize;
        }
        runs
    }

    /// Reserves `[base, base + bytes)` as a guard; false if something took it.
    fn guard(base: usize, bytes: usize) -> bool {
        // SAFETY: a reservation of free address space; nothing refers to it.
        let at = unsafe { VirtualAlloc(base as *const _, bytes, MEM_RESERVE, PAGE_NOACCESS) };
        at as usize == base
    }

    /// Reserves the free space below 4 GiB but the lowest [`SLACK`]. Call
    /// once, before the host allocates much (the GPU device, the caches).
    pub fn install() {
        let mut guards = guards();
        let mut slack = SLACK;
        for (mut base, mut bytes) in free_runs(0, FOUR_GIB) {
            let left = slack.min(bytes);
            (base, bytes, slack) = (base + left, bytes - left, slack - left);
            if bytes > 0 && guard(base, bytes) {
                guards.push((base, bytes));
            }
        }
        let total: usize = guards.iter().map(|g| g.1).sum();
        tracing::info!(
            guards = guards.len(),
            mb = total >> 20,
            "space below 4 GiB kept for the title"
        );
    }

    /// Try the smallest fitting guards first, then try smaller guards in
    /// descending size order. A replacement guard cannot be retried here.
    pub(crate) fn retry(
        bytes: usize,
        mut allocate: impl FnMut() -> *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void {
        use windows_sys::Win32::Foundation::{GetLastError, SetLastError};

        // Preserve the allocation error across address queries and reservations.
        // SAFETY: reads this thread's last-error value.
        let mut error = unsafe { GetLastError() };
        let mut guards = guards();
        let mut candidates = guards.clone();
        candidates.sort_unstable_by_key(|&(_, size)| {
            let fits = size >= bytes;
            (!fits, if fits { size } else { usize::MAX - size })
        });
        for (base, size) in candidates {
            // SAFETY: our own reservation, with no allocations inside it.
            if unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) } == 0 {
                continue;
            }
            guards.retain(|&(b, _)| b != base);
            let at = allocate();
            // SAFETY: reads this thread's last-error value.
            error = unsafe { GetLastError() };
            for (start, len) in free_runs(base, base + size) {
                if guard(start, len) {
                    guards.push((start, len));
                }
            }
            if !at.is_null() {
                return at;
            }
        }
        // SAFETY: sets this thread's last-error value.
        unsafe { SetLastError(error) };
        std::ptr::null_mut()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        static TEST: Mutex<()> = Mutex::new(());

        #[test]
        fn a_refused_retry_leaves_the_guards_in_place() {
            let _one = TEST.lock().unwrap();
            // SAFETY: a fresh reservation, released at once.
            let hole =
                unsafe { VirtualAlloc(std::ptr::null(), 4 * GRANULE, MEM_RESERVE, PAGE_NOACCESS) };
            assert!(!hole.is_null());
            let first = hole as usize;
            // SAFETY: ours.
            assert_ne!(unsafe { VirtualFree(hole, 0, MEM_RELEASE) }, 0);
            assert!(guard(first, 4 * GRANULE));
            guards().push((first, 4 * GRANULE));
            let mut attempts = 0;
            let result = retry(4 * GRANULE, || {
                attempts += 1;
                std::ptr::null_mut()
            });
            assert!(result.is_null());
            assert_eq!(attempts, 1);
            assert!(guards().contains(&(first, 4 * GRANULE)));
            guards().retain(|&(base, _)| base != first);
            // SAFETY: the guard the retry put back, ours.
            assert_ne!(unsafe { VirtualFree(hole, 0, MEM_RELEASE) }, 0);
        }

        /// A released guard is guarded again around what took part of it.
        #[test]
        fn a_released_guard_is_guarded_again_around_what_took_it() {
            let _one = TEST.lock().unwrap();
            // A known hole of four granules: reserved, then freed.
            // SAFETY: a fresh reservation, released at once.
            let hole =
                unsafe { VirtualAlloc(std::ptr::null(), 4 * GRANULE, MEM_RESERVE, PAGE_NOACCESS) }
                    as usize;
            assert_ne!(hole, 0);
            // SAFETY: ours.
            unsafe { VirtualFree(hole as *mut _, 0, MEM_RELEASE) };
            assert!(guard(hole, 4 * GRANULE));
            guards().push((hole, 4 * GRANULE));
            let taken = retry(GRANULE, || {
                // SAFETY: a reservation of the first granule at a known address.
                unsafe { VirtualAlloc(hole as *const _, GRANULE, MEM_RESERVE, PAGE_NOACCESS) }
            }) as usize;
            assert_eq!(taken, hole);
            assert!(guards().contains(&(hole + GRANULE, 3 * GRANULE)));
            guards().retain(|&(b, _)| b != hole + GRANULE);
            // SAFETY: ours.
            unsafe {
                VirtualFree(taken as *mut _, 0, MEM_RELEASE);
                VirtualFree((hole + GRANULE) as *mut _, 0, MEM_RELEASE);
            }
        }
    }
}

/// Ensures any demand-committed lazy pages in `[address, address + len)` are
/// committed before a kernel syscall (`ReadFile`, `fread`, DirectStorage,
/// `VirtualProtect`) accesses them.
pub(crate) fn ensure_committed(address: usize, len: usize) {
    lazy_commit::ensure(address, len);
}

/// Queries the byte span of the allocation starting at `address` before a
/// `VirtualFree(MEM_RELEASE)` or whole-allocation `VirtualFree(address, 0, MEM_DECOMMIT)`.
pub(crate) fn query_allocation_span(address: usize) -> usize {
    lazy_commit::allocation_span(address)
}

/// Clears lazy-commit tracking and reservation metadata after a successful
/// `VirtualFree(..., MEM_RELEASE)`.
pub(crate) fn on_virtual_free_release(address: usize, span: usize) {
    lazy_commit::forget_reservation(address);
    if span > 0 {
        lazy_commit::unmark_range(address, span);
    }
}

/// Clears lazy-commit tracking after a successful whole-allocation
/// `VirtualFree(address, 0, MEM_DECOMMIT)`.
pub(crate) fn on_virtual_free_decommit_all(address: usize, span: usize) {
    if span > 0 {
        lazy_commit::unmark_range(address, span);
    }
}

/// Commits any partially overlapped 64 KiB lazy boundary blocks ahead of a
/// sub-64 KiB `MEM_DECOMMIT` or eager `MEM_COMMIT` so surviving sibling 4 KiB
/// pages remain committed.
pub(crate) fn commit_partial_boundaries(address: usize, len: usize) {
    lazy_commit::commit_partial_boundaries(address, len);
}

/// Clears full 64 KiB lazy blocks strictly inside `[address, address + len)`
/// after a successful `MEM_DECOMMIT`.
pub(crate) fn on_virtual_free_decommit_interior(address: usize, len: usize) {
    lazy_commit::unmark_aligned_interior(address, len);
}

/// Opt-in demand-commit (`VICTORY_XMEM_LAZY_COMMIT=1`, off by default) for large
/// (`>= 4 MiB`) writable `XMemVirtualAlloc` allocations, plus opt-in demand-commit
/// (`VICTORY_XMEM_LAZY_PHYSICAL=1`, off by default) for non-aliased mapped
/// `SEC_RESERVE` physical-page views: reserves without committing upfront and
/// commits 64 KiB pages on first CPU touch via a lock-free atomic-state vectored
/// exception handler, or ahead of kernel syscalls via [`ensure_committed`].
mod lazy_commit {
    use std::collections::BTreeMap;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex, Once, RwLock};

    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_POINTERS,
    };
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ,
        PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY,
        PAGE_READWRITE, PAGE_WRITECOPY, VirtualAlloc, VirtualQueryEx,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) const PAGE: usize = 0x1_0000;
    const MIN_BYTES: usize = 4 << 20;
    const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    // 2-bit per-page state packed 32 pages per AtomicU64:
    // 0 = COMMITTED / unmarked, 1 = LAZY, 2 = COMMITTING (single-writer claim).
    const STATE_COMMITTED: u64 = 0;
    const STATE_LAZY: u64 = 1;
    const STATE_COMMITTING: u64 = 2;

    #[derive(PartialEq, Eq)]
    enum CommitClaim {
        Claimed,
        AlreadyCommitted,
    }

    /// One contiguous lazy-tracked range `[base, end)` with 2 atomic state bits
    /// per 64 KiB page. The VEH only ever takes `REGIONS.read()` and performs
    /// atomic state transitions in-place, never allocating or freeing heap memory.
    struct LazyRegion {
        base: usize,
        end: usize,
        states: Box<[AtomicU64]>,
    }

    impl LazyRegion {
        fn new_marked(base: usize, end: usize) -> (Self, usize) {
            let pages = (end - base) / PAGE;
            let words = pages.div_ceil(32);
            let mut vec = Vec::with_capacity(words);
            for w in 0..words {
                let rem = (pages - w * 32).min(32);
                let mut val = 0u64;
                for i in 0..rem {
                    val |= STATE_LAZY << (i * 2);
                }
                vec.push(AtomicU64::new(val));
            }
            (
                Self {
                    base,
                    end,
                    states: vec.into_boxed_slice(),
                },
                pages,
            )
        }

        fn slot(&self, page: usize) -> (&AtomicU64, u32) {
            let idx = (page - self.base) / PAGE;
            (&self.states[idx / 32], ((idx % 32) * 2) as u32)
        }

        fn is_marked(&self, page: usize) -> bool {
            if page < self.base || page >= self.end {
                return false;
            }
            let (word, shift) = self.slot(page);
            ((word.load(Ordering::Acquire) >> shift) & 0b11) == STATE_LAZY
        }

        /// Atomically claims `LAZY -> COMMITTING` so at most one thread calls
        /// `VirtualAlloc` for `page`, spinning while another thread finishes.
        fn begin_commit(&self, page: usize) -> CommitClaim {
            let (word, shift) = self.slot(page);
            let mask = 0b11u64 << shift;
            loop {
                let cur = word.load(Ordering::Acquire);
                match (cur >> shift) & 0b11 {
                    STATE_LAZY => {
                        let next = (cur & !mask) | (STATE_COMMITTING << shift);
                        if word
                            .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            return CommitClaim::Claimed;
                        }
                    }
                    STATE_COMMITTING => std::hint::spin_loop(),
                    _ => return CommitClaim::AlreadyCommitted,
                }
            }
        }

        fn finish_commit(&self, page: usize) {
            let (word, shift) = self.slot(page);
            let mask = 0b11u64 << shift;
            word.fetch_and(!mask, Ordering::Release);
        }

        fn abort_commit(&self, page: usize) {
            let (word, shift) = self.slot(page);
            let mask = 0b11u64 << shift;
            loop {
                let cur = word.load(Ordering::Acquire);
                let next = (cur & !mask) | (STATE_LAZY << shift);
                if word
                    .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return;
                }
            }
        }

        fn set_lazy(&self, page: usize) -> bool {
            let (word, shift) = self.slot(page);
            let mask = 0b11u64 << shift;
            loop {
                let cur = word.load(Ordering::Acquire);
                let st = (cur >> shift) & 0b11;
                if st == STATE_LAZY {
                    return false;
                }
                if st == STATE_COMMITTING {
                    std::hint::spin_loop();
                    continue;
                }
                let next = (cur & !mask) | (STATE_LAZY << shift);
                if word
                    .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return true;
                }
            }
        }

        fn clear_lazy(&self, page: usize) -> bool {
            let (word, shift) = self.slot(page);
            let mask = 0b11u64 << shift;
            loop {
                let cur = word.load(Ordering::Acquire);
                let st = (cur >> shift) & 0b11;
                if st == STATE_COMMITTED {
                    return false;
                }
                if st == STATE_COMMITTING {
                    std::hint::spin_loop();
                    continue;
                }
                let next = cur & !mask;
                if word
                    .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return true;
                }
            }
        }

        fn clear_range(&self, start: usize, end: usize) -> usize {
            let from = start.max(self.base);
            let to = end.min(self.end);
            let mut cleared = 0usize;
            let mut p = from;
            while p < to {
                if self.clear_lazy(p) {
                    cleared += 1;
                }
                p += PAGE;
            }
            cleared
        }

        fn set_range(&self, start: usize, end: usize) -> usize {
            let from = start.max(self.base);
            let to = end.min(self.end);
            let mut added = 0usize;
            let mut p = from;
            while p < to {
                if self.set_lazy(p) {
                    added += 1;
                }
                p += PAGE;
            }
            added
        }

        fn has_any_bit(&self) -> bool {
            self.states.iter().any(|w| w.load(Ordering::Acquire) != 0)
        }
    }

    static REGIONS: LazyLock<RwLock<Vec<LazyRegion>>> = LazyLock::new(|| RwLock::new(Vec::new()));
    static REGIONS_NONEMPTY: AtomicBool = AtomicBool::new(false);
    static LAZY_COUNT: AtomicUsize = AtomicUsize::new(0);
    /// Non-placeholder `XMemVirtualAlloc` reservations (`base -> end`) eligible
    /// for two-step `MEM_COMMIT` lazy marking.
    static RESERVATIONS: LazyLock<Mutex<BTreeMap<usize, usize>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));

    fn enabled_for(val: Option<&str>) -> bool {
        val.is_some_and(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0"
        })
    }

    fn enabled() -> bool {
        static ON: LazyLock<bool> = LazyLock::new(|| {
            !cfg!(test) && enabled_for(std::env::var("VICTORY_XMEM_LAZY_COMMIT").ok().as_deref())
        });
        *ON
    }

    pub(super) fn physical_enabled() -> bool {
        static ON: LazyLock<bool> = LazyLock::new(|| {
            !cfg!(test)
                && enabled_for(std::env::var("VICTORY_XMEM_LAZY_PHYSICAL").ok().as_deref())
        });
        *ON
    }

    pub(super) fn eligible(size: usize, kind: u32, protect: u32) -> bool {
        enabled() && eligible_policy(size, kind, protect)
    }

    fn eligible_policy(size: usize, kind: u32, protect: u32) -> bool {
        size >= MIN_BYTES && kind & MEM_COMMIT != 0 && protect == PAGE_READWRITE
    }

    pub(super) fn record_reservation(base: usize, len: usize) {
        if base == 0 || len == 0 {
            return;
        }
        let end = base.saturating_add(len.next_multiple_of(PAGE));
        RESERVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(base, end);
    }

    pub(super) fn forget_reservation(base: usize) {
        if base == 0 {
            return;
        }
        RESERVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&base);
    }

    pub(super) fn is_non_placeholder_reservation(base: usize, len: usize) -> bool {
        let Some(end) = base.checked_add(len) else {
            return false;
        };
        let tracked = {
            let g = RESERVATIONS.lock().unwrap_or_else(|e| e.into_inner());
            g.range(..=base)
                .next_back()
                .is_some_and(|(_, &res_end)| res_end >= end)
        };
        if !tracked {
            return false;
        }
        is_reserved_span(base, len)
    }

    pub(super) fn is_reserved_span(base: usize, len: usize) -> bool {
        let Some(end) = base.checked_add(len) else {
            return false;
        };
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: querying VA metadata only.
        let n = unsafe {
            VirtualQueryEx(
                GetCurrentProcess(),
                base as *const c_void,
                &mut mbi,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        let mbi_end = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize);
        n != 0 && mbi.State == MEM_RESERVE && mbi_end >= end
    }

    fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: `handler` is a valid VEH callback.
            unsafe {
                AddVectoredExceptionHandler(1, Some(handler));
            }
        });
    }

    unsafe fn allows(access: usize, at: usize) -> bool {
        let mut m = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: querying current process VA metadata only.
        let n = unsafe {
            VirtualQueryEx(
                GetCurrentProcess(),
                at as *const c_void,
                &mut m,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 || m.State != MEM_COMMIT || m.Protect & PAGE_GUARD != 0 {
            return false;
        }
        let writes = PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
        let reads = writes | PAGE_READONLY | PAGE_EXECUTE_READ;
        match access {
            0 => m.Protect & reads != 0,
            1 => m.Protect & writes != 0,
            _ => false,
        }
    }

    unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        if !REGIONS_NONEMPTY.load(Ordering::Acquire) {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        // SAFETY: Windows passes a valid EXCEPTION_POINTERS with ExceptionRecord.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let fault_addr = r.ExceptionInformation[1];
        let page = fault_addr & !(PAGE - 1);

        let action = {
            let g = REGIONS.read().unwrap_or_else(|e| e.into_inner());
            let idx = g.partition_point(|reg| reg.end <= page);
            if idx >= g.len() || page < g[idx].base {
                None
            } else {
                match g[idx].begin_commit(page) {
                    CommitClaim::Claimed => {
                        // SAFETY: `page` is a 64 KiB reserved page owned by the process.
                        let committed = unsafe {
                            VirtualAlloc(page as *const c_void, PAGE, MEM_COMMIT, PAGE_READWRITE)
                        };
                        if committed.is_null() {
                            g[idx].abort_commit(page);
                            Some(Err(()))
                        } else {
                            g[idx].finish_commit(page);
                            LAZY_COUNT.fetch_sub(1, Ordering::Release);
                            Some(Ok(true))
                        }
                    }
                    CommitClaim::AlreadyCommitted => Some(Ok(false)),
                }
            }
        };

        match action {
            Some(Ok(true)) => {
                victory_gpu::title_memory_committed(page, PAGE);
                EXCEPTION_CONTINUE_EXECUTION
            }
            Some(Ok(false)) => {
                // SAFETY: verifying a concurrent commit inside our own LazyRegion.
                if unsafe { allows(r.ExceptionInformation[0], fault_addr) } {
                    EXCEPTION_CONTINUE_EXECUTION
                } else {
                    EXCEPTION_CONTINUE_SEARCH
                }
            }
            // No heap allocation or I/O in the VEH even on commit failure.
            Some(Err(())) | None => EXCEPTION_CONTINUE_SEARCH,
        }
    }

    pub(super) fn mark_range(start: usize, len: usize) {
        if len == 0 {
            return;
        }
        install();
        let first = start & !(PAGE - 1);
        let end = start.saturating_add(len).div_ceil(PAGE).saturating_mul(PAGE);
        if end <= first {
            return;
        }
        // Pre-allocate the common non-overlapping region before acquiring the write lock.
        let (candidate, candidate_pages) = LazyRegion::new_marked(first, end);
        let mut to_drop: Vec<LazyRegion> = Vec::new();
        {
            let mut g = REGIONS.write().unwrap_or_else(|e| e.into_inner());
            let mut idx = g.partition_point(|r| r.end <= first);
            let overlaps = idx < g.len() && g[idx].base < end;
            if !overlaps {
                g.insert(idx, candidate);
                LAZY_COUNT.fetch_add(candidate_pages, Ordering::Release);
            } else {
                to_drop.push(candidate);
                let mut cur = first;
                while cur < end {
                    if idx < g.len() && g[idx].base < end {
                        let r_base = g[idx].base;
                        let r_end = g[idx].end;
                        if cur < r_base {
                            let gap_end = r_base.min(end);
                            let (gap, pages) = LazyRegion::new_marked(cur, gap_end);
                            g.insert(idx, gap);
                            LAZY_COUNT.fetch_add(pages, Ordering::Release);
                            idx += 1;
                            cur = gap_end;
                            continue;
                        }
                        let seg_end = r_end.min(end);
                        let added = g[idx].set_range(cur, seg_end);
                        LAZY_COUNT.fetch_add(added, Ordering::Release);
                        cur = seg_end;
                        idx += 1;
                    } else {
                        let (tail, pages) = LazyRegion::new_marked(cur, end);
                        g.insert(idx, tail);
                        LAZY_COUNT.fetch_add(pages, Ordering::Release);
                        break;
                    }
                }
            }
            REGIONS_NONEMPTY.store(!g.is_empty(), Ordering::Release);
        }
        drop(to_drop);
    }

    pub(super) fn unmark_range(start: usize, len: usize) {
        if len == 0 || !REGIONS_NONEMPTY.load(Ordering::Acquire) {
            return;
        }
        let first = start & !(PAGE - 1);
        let end = start.saturating_add(len).div_ceil(PAGE).saturating_mul(PAGE);
        let mut removed: Vec<LazyRegion> = Vec::new();
        {
            let mut g = REGIONS.write().unwrap_or_else(|e| e.into_inner());
            let mut i = g.partition_point(|r| r.end <= first);
            while i < g.len() && g[i].base < end {
                let cleared = g[i].clear_range(first, end);
                if cleared > 0 {
                    LAZY_COUNT.fetch_sub(cleared, Ordering::Release);
                }
                if !g[i].has_any_bit() {
                    removed.push(g.remove(i));
                } else {
                    i += 1;
                }
            }
            REGIONS_NONEMPTY.store(!g.is_empty(), Ordering::Release);
        }
        drop(removed);
    }

    pub(super) fn allocation_span(base: usize) -> usize {
        if base == 0 || !REGIONS_NONEMPTY.load(Ordering::Acquire) {
            return 0;
        }
        let mut span = 0usize;
        let mut cur = base;
        let mut m = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: querying VA metadata only.
        while unsafe {
            VirtualQueryEx(
                GetCurrentProcess(),
                cur as *const c_void,
                &mut m,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        } != 0
            && m.AllocationBase as usize == base
            && m.RegionSize > 0
        {
            span = span.saturating_add(m.RegionSize);
            match cur.checked_add(m.RegionSize) {
                Some(next) => cur = next,
                None => break,
            }
        }
        span
    }

    pub(super) fn commit_partial_boundaries(start: usize, len: usize) {
        if len == 0 || LAZY_COUNT.load(Ordering::Acquire) == 0 {
            return;
        }
        let end = start.saturating_add(len);
        if !start.is_multiple_of(PAGE) {
            ensure(start & !(PAGE - 1), PAGE);
        }
        if !end.is_multiple_of(PAGE) {
            let tail = end & !(PAGE - 1);
            if tail != (start & !(PAGE - 1)) || start.is_multiple_of(PAGE) {
                ensure(tail, PAGE);
            }
        }
    }

    pub(super) fn unmark_aligned_interior(start: usize, len: usize) {
        if len == 0 || !REGIONS_NONEMPTY.load(Ordering::Acquire) {
            return;
        }
        let end = start.saturating_add(len);
        let first_full = start.div_ceil(PAGE).saturating_mul(PAGE);
        let last_full = end & !(PAGE - 1);
        if last_full > first_full {
            unmark_range(first_full, last_full - first_full);
        }
    }

    #[cfg(test)]
    pub(super) fn unmark_allocation(base: usize) {
        let span = allocation_span(base);
        if span > 0 {
            unmark_range(base, span);
        }
    }

    #[cfg(test)]
    pub(super) fn is_marked(addr: usize) -> bool {
        if LAZY_COUNT.load(Ordering::Acquire) == 0 {
            return false;
        }
        let page = addr & !(PAGE - 1);
        let g = REGIONS.read().unwrap_or_else(|e| e.into_inner());
        let idx = g.partition_point(|r| r.end <= page);
        idx < g.len() && g[idx].is_marked(page)
    }

    /// Returns `(marked, run_end)` for the contiguous marked or unmarked run
    /// starting at `query_base` within `[query_base, region_end)`, or `None` if
    /// no lazy region overlaps `[query_base, region_end)`.
    pub(super) fn query_run(query_base: usize, region_end: usize) -> Option<(bool, usize)> {
        if LAZY_COUNT.load(Ordering::Acquire) == 0 || query_base >= region_end {
            return None;
        }
        let g = REGIONS.read().unwrap_or_else(|e| e.into_inner());
        let first_page = query_base & !(PAGE - 1);
        let mut idx = g.partition_point(|r| r.end <= first_page);
        if idx >= g.len() || g[idx].base >= region_end {
            return None;
        }
        let page_marked = |p: usize, i: &mut usize| -> bool {
            while *i < g.len() && g[*i].end <= p {
                *i += 1;
            }
            *i < g.len() && g[*i].is_marked(p)
        };
        let initial = page_marked(first_page, &mut idx);
        let mut cur = first_page.saturating_add(PAGE);
        while cur < region_end && page_marked(cur, &mut idx) == initial {
            match cur.checked_add(PAGE) {
                Some(next) => cur = next,
                None => break,
            }
        }
        Some((initial, cur.min(region_end)))
    }

    pub(super) fn ensure(p: usize, len: usize) {
        if len == 0 || LAZY_COUNT.load(Ordering::Acquire) == 0 {
            return;
        }
        let first = p & !(PAGE - 1);
        let end = p.saturating_add(len).div_ceil(PAGE).saturating_mul(PAGE);
        let mut committed_pages = Vec::new();
        {
            let g = REGIONS.read().unwrap_or_else(|e| e.into_inner());
            let mut idx = g.partition_point(|r| r.end <= first);
            while idx < g.len() && g[idx].base < end {
                let mut cur = first.max(g[idx].base);
                let stop = end.min(g[idx].end);
                while cur < stop {
                    if g[idx].begin_commit(cur) == CommitClaim::Claimed {
                        // SAFETY: `cur` is a 64 KiB reserved page owned by the process.
                        let ok = unsafe {
                            !VirtualAlloc(cur as *const c_void, PAGE, MEM_COMMIT, PAGE_READWRITE)
                                .is_null()
                        };
                        if ok {
                            g[idx].finish_commit(cur);
                            LAZY_COUNT.fetch_sub(1, Ordering::Release);
                            committed_pages.push(cur);
                        } else {
                            g[idx].abort_commit(cur);
                        }
                    }
                    cur += PAGE;
                }
                idx += 1;
            }
        }
        for page in committed_pages {
            victory_gpu::title_memory_committed(page, PAGE);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};

        #[test]
        fn policy_requires_4_mib_commit_readwrite_and_defaults_off() {
            assert!(!enabled_for(None));
            assert!(!enabled_for(Some("0")));
            assert!(enabled_for(Some("1")));
            assert!(eligible_policy(4 << 20, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE));
            assert!(!eligible_policy((4 << 20) - 1, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE));
            assert!(!eligible_policy(4 << 20, MEM_RESERVE, PAGE_READWRITE));
            assert!(!eligible_policy(4 << 20, MEM_RESERVE | MEM_COMMIT, PAGE_READONLY));
        }

        #[test]
        fn lazy_pages_commit_on_first_touch_and_ensure_commits_without_fault() {
            // SAFETY: fresh 4-page reservation for test.
            let base = unsafe { VirtualAlloc(std::ptr::null(), 4 * PAGE, MEM_RESERVE, PAGE_READWRITE) } as usize;
            assert_ne!(base, 0);
            mark_range(base, 4 * PAGE);
            assert!(is_marked(base));
            assert!(is_marked(base + 3 * PAGE));

            // First read/write commits via VEH.
            // SAFETY: marked lazy page inside our reservation.
            unsafe {
                assert_eq!(std::ptr::read_volatile((base + 16) as *const u32), 0);
                std::ptr::write_volatile((base + 16) as *mut u32, 0x1234_5678);
                assert_eq!(std::ptr::read_volatile((base + 16) as *const u32), 0x1234_5678);
            }
            assert!(!is_marked(base));
            assert!(is_marked(base + PAGE));

            // `ensure` commits proactively for kernel syscalls.
            ensure(base + PAGE + 4, PAGE);
            assert!(!is_marked(base + PAGE));
            assert!(!is_marked(base + 2 * PAGE));
            assert!(is_marked(base + 3 * PAGE));

            unmark_allocation(base);
            assert!(!is_marked(base + 3 * PAGE));
            // SAFETY: ours.
            assert_ne!(unsafe { VirtualFree(base as *mut c_void, 0, MEM_RELEASE) }, 0);
        }

        #[test]
        fn concurrent_first_touch_on_shared_lazy_pages_succeeds() {
            const THREADS: usize = 8;
            const PAGES: usize = 64;
            // SAFETY: fresh reservation for test.
            let base = unsafe { VirtualAlloc(std::ptr::null(), PAGES * PAGE, MEM_RESERVE, PAGE_READWRITE) } as usize;
            assert_ne!(base, 0);
            mark_range(base, PAGES * PAGE);
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let b = barrier.clone();
                    std::thread::spawn(move || {
                        for i in 0..PAGES {
                            b.wait();
                            // SAFETY: disjoint 8-byte slot per thread inside `base`.
                            unsafe {
                                std::ptr::write_volatile(
                                    (base + i * PAGE + t * 64) as *mut u64,
                                    (i * THREADS + t) as u64,
                                );
                            }
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            for i in 0..PAGES {
                for t in 0..THREADS {
                    // SAFETY: written above.
                    let v = unsafe { std::ptr::read_volatile((base + i * PAGE + t * 64) as *const u64) };
                    assert_eq!(v, (i * THREADS + t) as u64);
                }
            }
            unmark_allocation(base);
            // SAFETY: ours.
            assert_ne!(unsafe { VirtualFree(base as *mut c_void, 0, MEM_RELEASE) }, 0);
        }

        #[test]
        fn lazy_physical_sec_reserve_view_commits_on_touch_and_ensure() {
            use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
            use windows_sys::Win32::System::Memory::{
                CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, SEC_RESERVE, UnmapViewOfFile,
            };

            const LEN: usize = 4 * PAGE;
            // SAFETY: creating a temporary pagefile-backed SEC_RESERVE section for test.
            let section = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE | SEC_RESERVE,
                    0,
                    LEN as u32,
                    std::ptr::null(),
                )
            };
            assert!(!section.is_null());

            // SAFETY: mapping our section as a writable view.
            let view = unsafe { MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, LEN) };
            let base = view.Value as usize;
            assert_ne!(base, 0);

            mark_range(base, LEN);
            assert!(is_reserved_span(base, LEN));
            assert!(is_marked(base));

            // First CPU write commits page 0 via VEH; page 1 stays reserved.
            // SAFETY: page 0 is marked lazy inside our mapped section view.
            unsafe { std::ptr::write_volatile(base as *mut u32, 0xCAFE_BABE) };
            // SAFETY: page 0 is now committed.
            assert_eq!(unsafe { std::ptr::read_volatile(base as *const u32) }, 0xCAFE_BABE);
            assert!(!is_marked(base));
            assert!(is_marked(base + PAGE));

            // `ensure` commits page 2 ahead of a kernel write without faulting.
            ensure(base + 2 * PAGE, PAGE);
            assert!(!is_marked(base + 2 * PAGE));
            assert!(is_marked(base + 3 * PAGE));

            unmark_range(base, LEN);
            assert!(!is_marked(base + PAGE));
            assert!(!is_marked(base + 3 * PAGE));

            // SAFETY: unmapping and closing our test section.
            unsafe {
                UnmapViewOfFile(view);
                CloseHandle(section);
            }
        }

        #[test]
        fn virtual_free_and_virtual_protect_clear_and_commit_lazy_pages() {
            use windows_sys::Win32::System::Memory::MEM_DECOMMIT;

            const LEN: usize = 4 * PAGE;
            // SAFETY: fresh reservation for test.
            let base = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE, PAGE_READWRITE) } as usize;
            assert_ne!(base, 0);
            record_reservation(base, LEN);
            mark_range(base, LEN);
            assert!(is_marked(base));

            // VirtualProtect commits lazy pages before changing protection.
            let mut old = 0u32;
            assert_ne!(
                crate::modules::kernel::VirtualProtect(
                    (base + 2 * PAGE) as *const c_void,
                    PAGE,
                    PAGE_READONLY,
                    &mut old,
                ),
                0
            );
            assert!(!is_marked(base + 2 * PAGE));

            // VirtualFree(MEM_DECOMMIT) clears lazy marks for the decommitted sub-range.
            assert_ne!(
                crate::modules::kernel::VirtualFree(
                    (base + PAGE) as *mut c_void,
                    PAGE,
                    MEM_DECOMMIT,
                ),
                0
            );
            assert!(!is_marked(base + PAGE));
            assert!(is_marked(base));
            assert!(is_marked(base + 3 * PAGE));

            // VirtualFree(MEM_RELEASE) with non-zero size fails and must NOT clear marks/reservations.
            assert_eq!(
                crate::modules::kernel::VirtualFree(base as *mut c_void, 4096, MEM_RELEASE),
                0
            );
            assert!(is_marked(base), "failed MEM_RELEASE must preserve lazy marks");
            assert!(is_non_placeholder_reservation(base, PAGE));

            // Sub-64 KiB MEM_DECOMMIT commits sibling 4 KiB pages while unmarking the block.
            assert_ne!(
                crate::modules::kernel::VirtualFree(
                    (base + 4096) as *mut c_void,
                    4096,
                    MEM_DECOMMIT,
                ),
                0
            );
            assert!(!is_marked(base));
            // Sibling 4 KiB page at `base` must remain committed and writable without AV.
            // SAFETY: sibling 4 KiB page inside our allocation.
            unsafe { std::ptr::write_volatile(base as *mut u32, 0x1234_5678) };
            // SAFETY: read back sibling 4 KiB page.
            assert_eq!(unsafe { std::ptr::read_volatile(base as *const u32) }, 0x1234_5678);

            // Eager non-RW XMemVirtualAlloc(MEM_COMMIT, PAGE_READONLY) clears lazy marks.
            let p3 = base + 3 * PAGE;
            assert!(is_marked(p3));
            let committed = super::super::XMemVirtualAlloc(
                p3 as *mut c_void,
                PAGE,
                MEM_COMMIT,
                0,
                PAGE_READONLY,
            );
            assert_eq!(committed as usize, p3);
            assert!(!is_marked(p3), "eager non-RW commit must clear lazy mark");

            // Whole-allocation VirtualFree(base, 0, MEM_DECOMMIT) clears all remaining marks.
            mark_range(base + 2 * PAGE, PAGE);
            assert!(is_marked(base + 2 * PAGE));
            assert_ne!(
                crate::modules::kernel::VirtualFree(base as *mut c_void, 0, MEM_DECOMMIT),
                0
            );
            assert!(!is_marked(base + 2 * PAGE));

            // VirtualFree(MEM_RELEASE) clears all remaining marks and reservation record.
            assert_ne!(
                crate::modules::kernel::VirtualFree(base as *mut c_void, 0, MEM_RELEASE),
                0
            );
            assert!(!is_marked(base));
            assert!(!is_marked(base + 3 * PAGE));
            assert!(!is_non_placeholder_reservation(base, LEN));
        }

        #[test]
        fn xmem_virtual_query_clamps_region_size_and_rejects_placeholders() {
            const HALF: usize = 4 * super::super::MIB;
            const TOTAL: usize = 8 * super::super::MIB;
            // SAFETY: fresh 8 MiB reservation for test.
            let base = unsafe { VirtualAlloc(std::ptr::null(), TOTAL, MEM_RESERVE, PAGE_READWRITE) } as usize;
            assert_ne!(base, 0);
            mark_range(base, HALF);

            let mut info = super::super::BasicInformation {
                host: MEMORY_BASIC_INFORMATION::default(),
                flags: 0,
                tag_code: 0,
                tag: [0; 4],
            };
            // SAFETY: returns the pseudo-handle for the current process.
            let process = unsafe { GetCurrentProcess() };
            assert_eq!(
                super::super::XMemVirtualQuery(process, base as *const c_void, &mut info),
                1
            );
            assert_eq!(info.host.State, MEM_COMMIT);
            assert_eq!(info.host.Protect, PAGE_READWRITE);
            assert_eq!(info.host.RegionSize, HALF, "RegionSize clamped to marked run");

            assert_eq!(
                super::super::XMemVirtualQuery(
                    process,
                    (base + HALF) as *const c_void,
                    &mut info
                ),
                1
            );
            assert_eq!(info.host.State, MEM_RESERVE);
            assert_eq!(info.host.RegionSize, HALF, "unmarked tail stays MEM_RESERVE");

            unmark_range(base, HALF);
            // SAFETY: ours.
            assert_ne!(unsafe { VirtualFree(base as *mut c_void, 0, MEM_RELEASE) }, 0);

            // Placeholders created with PAGES_64K must never be accepted by the two-step lazy path.
            let ph = super::super::XMemVirtualAlloc(
                std::ptr::null_mut(),
                HALF,
                MEM_RESERVE | super::super::PAGES_64K,
                0,
                PAGE_READWRITE,
            ) as usize;
            assert_ne!(ph, 0);
            assert!(!is_non_placeholder_reservation(ph, HALF));
            // SAFETY: ours.
            assert_ne!(unsafe { VirtualFree(ph as *mut c_void, 0, MEM_RELEASE) }, 0);
        }
    }
}

/// Opt-in diagnostic (`VICTORY_XMEM_TOUCH_DIAG=1`) measuring how much of the
/// title's committed write-watched `XMemVirtualAlloc` memory is ever written
/// (`GetWriteWatch(0, ...)`, non-destructive so GPU write-watch state is never
/// disturbed). Reports every 30 s alongside process commit and working set.
mod touch_diag {
    use std::collections::BTreeMap;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{LazyLock, Mutex, Once};
    use std::time::Duration;

    use windows_sys::Win32::System::Memory::{
        GetWriteWatch, MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQueryEx,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    const REPORT_INTERVAL: Duration = Duration::from_secs(30);
    const PAGE_BYTES: usize = 4096;

    static NEXT_GEN: AtomicU64 = AtomicU64::new(1);
    static TRACKED: LazyLock<Mutex<BTreeMap<usize, (usize, u64)>>> =
        LazyLock::new(|| Mutex::new(BTreeMap::new()));

    fn enabled_for(val: Option<&str>) -> bool {
        val.is_some_and(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0"
        })
    }

    fn enabled() -> bool {
        static ON: LazyLock<bool> = LazyLock::new(|| {
            !cfg!(test) && enabled_for(std::env::var("VICTORY_XMEM_TOUCH_DIAG").ok().as_deref())
        });
        *ON
    }

    pub(super) fn track(base: usize, len: usize) {
        if !enabled() || base == 0 || len == 0 {
            return;
        }
        let epoch = NEXT_GEN.fetch_add(1, Ordering::Relaxed);
        TRACKED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(base, (len, epoch));
        static SPAWN: Once = Once::new();
        SPAWN.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("xmem-touch-diag".into())
                .spawn(reporter_loop);
        });
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct TouchSummary {
        regions: usize,
        committed_bytes: usize,
        touched_bytes: usize,
        untouched_bytes: usize,
    }

    /// Inspects one write-watched allocation across all consecutive subregions
    /// (handling `VirtualProtect` splits and partial `MEM_DECOMMIT`) without
    /// resetting watch bits. Returns `None` once the reservation is released or
    /// replaced by a non-write-watched mapping.
    fn inspect_region(base: usize, len: usize) -> Option<(usize, usize)> {
        let end = base.saturating_add(len);
        let mut cursor = base;
        let mut live = false;
        let mut total_committed = 0usize;
        let mut total_touched = 0usize;
        let mut addrs: Vec<*mut c_void> = Vec::new();

        while cursor < end {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: querying current process VA metadata only.
            let queried = unsafe {
                VirtualQueryEx(
                    GetCurrentProcess(),
                    cursor as *const c_void,
                    &mut mbi,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if queried == 0 || mbi.RegionSize == 0 || mbi.AllocationBase as usize != base {
                break;
            }
            live = true;
            let seg_end = cursor.saturating_add(mbi.RegionSize).min(end);
            let seg_len = seg_end.saturating_sub(cursor);
            if mbi.State == MEM_COMMIT && seg_len > 0 {
                let max_pages = seg_len.div_ceil(PAGE_BYTES).max(1);
                if addrs.len() < max_pages {
                    addrs.resize(max_pages, std::ptr::null_mut());
                }
                let mut count = max_pages;
                let mut granularity = 0u32;
                // Flag 0 = non-destructive query (does not reset write-watch bits).
                // SAFETY: `cursor..seg_end` is a live committed write-watched subregion.
                let rc = unsafe {
                    GetWriteWatch(
                        0,
                        cursor as *const c_void,
                        seg_len,
                        addrs.as_mut_ptr(),
                        &mut count,
                        &mut granularity,
                    )
                };
                if rc != 0 {
                    return None;
                }
                let page = (granularity as usize).max(PAGE_BYTES);
                total_committed = total_committed.saturating_add(seg_len);
                total_touched = total_touched.saturating_add(count.saturating_mul(page).min(seg_len));
            }
            cursor = seg_end;
        }

        live.then_some((total_committed, total_touched))
    }

    fn snapshot_map(tracked: &Mutex<BTreeMap<usize, (usize, u64)>>) -> TouchSummary {
        let entries: Vec<(usize, usize, u64)> = tracked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(&b, &(l, epoch))| (b, l, epoch))
            .collect();
        let mut summary = TouchSummary::default();
        let mut stale = Vec::new();
        for (base, len, epoch) in entries {
            match inspect_region(base, len) {
                Some((committed, touched)) => {
                    summary.regions += 1;
                    summary.committed_bytes = summary.committed_bytes.saturating_add(committed);
                    summary.touched_bytes = summary.touched_bytes.saturating_add(touched);
                }
                None => stale.push((base, epoch)),
            }
        }
        summary.untouched_bytes = summary.committed_bytes.saturating_sub(summary.touched_bytes);
        if !stale.is_empty() {
            let mut g = tracked.lock().unwrap_or_else(|e| e.into_inner());
            for (base, epoch) in stale {
                if g.get(&base).is_some_and(|&(_, cur_epoch)| cur_epoch == epoch) {
                    g.remove(&base);
                }
            }
        }
        summary
    }

    fn process_memory() -> Option<(usize, usize)> {
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        // SAFETY: querying current process memory counters.
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        };
        (ok != 0).then_some((counters.PagefileUsage, counters.WorkingSetSize))
    }

    fn reporter_loop() {
        loop {
            std::thread::sleep(REPORT_INTERVAL);
            let s = snapshot_map(&TRACKED);
            if s.regions == 0 {
                continue;
            }
            let to_mib = |b: usize| b as f64 / (1024.0 * 1024.0);
            let untouched_pct = if s.committed_bytes > 0 {
                (s.untouched_bytes as f64 / s.committed_bytes as f64) * 100.0
            } else {
                0.0
            };
            let (proc_commit, proc_ws) = process_memory().unwrap_or((0, 0));
            tracing::info!(
                regions = s.regions,
                committed_mib = format_args!("{:.1}", to_mib(s.committed_bytes)),
                touched_mib = format_args!("{:.1}", to_mib(s.touched_bytes)),
                untouched_mib = format_args!("{:.1}", to_mib(s.untouched_bytes)),
                untouched_pct = format_args!("{:.1}%", untouched_pct),
                proc_commit_mib = format_args!("{:.1}", to_mib(proc_commit)),
                proc_ws_mib = format_args!("{:.1}", to_mib(proc_ws)),
                "xmem commit vs touched"
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::Memory::{
            MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect,
        };

        #[test]
        fn env_flag_defaults_off_and_accepts_non_zero() {
            assert!(!enabled_for(None));
            assert!(!enabled_for(Some("")));
            assert!(!enabled_for(Some("0")));
            assert!(enabled_for(Some("1")));
            assert!(enabled_for(Some("true")));
        }

        #[test]
        fn snapshot_counts_touched_vs_untouched_without_resetting_and_prunes_freed() {
            const LEN: usize = 16 * PAGE_BYTES;
            // SAFETY: fresh write-watched allocation for test.
            let base = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    LEN,
                    MEM_RESERVE | MEM_COMMIT | super::super::MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                )
            } as usize;
            assert_ne!(base, 0);

            let map = Mutex::new(BTreeMap::from([(base, (LEN, 1))]));
            let before = snapshot_map(&map);
            assert_eq!(before.regions, 1);
            assert_eq!(before.committed_bytes, LEN);
            assert_eq!(before.touched_bytes, 0);
            assert_eq!(before.untouched_bytes, LEN);

            // Touch 3 distinct 4 KiB pages.
            // SAFETY: inside our committed LEN-byte allocation.
            unsafe {
                std::ptr::write_volatile(base as *mut u8, 1);
                std::ptr::write_volatile((base + 2 * PAGE_BYTES) as *mut u8, 2);
                std::ptr::write_volatile((base + 7 * PAGE_BYTES) as *mut u8, 3);
            }
            let after1 = snapshot_map(&map);
            assert_eq!(after1.touched_bytes, 3 * PAGE_BYTES);
            assert_eq!(after1.untouched_bytes, LEN - 3 * PAGE_BYTES);

            // Second snapshot must report the same touched count (non-destructive query).
            let after2 = snapshot_map(&map);
            assert_eq!(after2, after1);

            // Freeing the block prunes it cleanly on the next snapshot.
            // SAFETY: ours.
            assert_ne!(unsafe { VirtualFree(base as *mut c_void, 0, MEM_RELEASE) }, 0);
            let freed = snapshot_map(&map);
            assert_eq!(freed.regions, 0);
            assert!(map.lock().unwrap().is_empty());
        }

        #[test]
        fn snapshot_walks_virtual_protect_splits_and_leading_decommit() {
            const LEN: usize = 16 * PAGE_BYTES;
            // SAFETY: fresh write-watched allocation for test.
            let base = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    LEN,
                    MEM_RESERVE | MEM_COMMIT | super::super::MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                )
            } as usize;
            assert_ne!(base, 0);

            // Touch page 0 and page 10.
            // SAFETY: inside our committed LEN-byte allocation.
            unsafe {
                std::ptr::write_volatile(base as *mut u8, 1);
                std::ptr::write_volatile((base + 10 * PAGE_BYTES) as *mut u8, 2);
            }

            // Split the allocation into 3 subregions via VirtualProtect on pages 4..8.
            let mut old = 0u32;
            assert_ne!(
                // SAFETY: valid subregion inside our allocation.
                unsafe {
                    VirtualProtect(
                        (base + 4 * PAGE_BYTES) as *const c_void,
                        4 * PAGE_BYTES,
                        PAGE_READONLY,
                        &mut old,
                    )
                },
                0
            );

            let map = Mutex::new(BTreeMap::from([(base, (LEN, 1))]));
            let split = snapshot_map(&map);
            assert_eq!(split.regions, 1);
            assert_eq!(split.committed_bytes, LEN);
            assert_eq!(split.touched_bytes, 2 * PAGE_BYTES);

            // Decommit the leading 4 pages; the remaining 12 pages stay live and counted.
            assert_ne!(
                // SAFETY: valid subregion inside our allocation.
                unsafe { VirtualFree(base as *mut c_void, 4 * PAGE_BYTES, MEM_DECOMMIT) },
                0
            );
            let partial = snapshot_map(&map);
            assert_eq!(partial.regions, 1);
            assert_eq!(partial.committed_bytes, 12 * PAGE_BYTES);
            assert_eq!(partial.touched_bytes, PAGE_BYTES);
            assert!(!map.lock().unwrap().is_empty());

            // SAFETY: ours.
            assert_ne!(unsafe { VirtualFree(base as *mut c_void, 0, MEM_RELEASE) }, 0);
        }

        #[test]
        fn snapshot_keeps_reserved_write_watch_arena_until_committed() {
            const LEN: usize = 8 * PAGE_BYTES;
            // SAFETY: fresh reserved-only write-watched arena for test.
            let base = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    LEN,
                    MEM_RESERVE | super::super::MEM_WRITE_WATCH,
                    PAGE_READWRITE,
                )
            } as usize;
            assert_ne!(base, 0);

            let map = Mutex::new(BTreeMap::from([(base, (LEN, 1))]));
            let reserved = snapshot_map(&map);
            assert_eq!(reserved.regions, 1);
            assert_eq!(reserved.committed_bytes, 0);
            assert_eq!(reserved.touched_bytes, 0);
            assert!(!map.lock().unwrap().is_empty());

            // Commit the upper half and touch 1 page.
            // SAFETY: inside our reserved arena.
            let committed = unsafe {
                VirtualAlloc(
                    (base + 4 * PAGE_BYTES) as *const c_void,
                    4 * PAGE_BYTES,
                    MEM_COMMIT,
                    PAGE_READWRITE,
                )
            };
            assert!(!committed.is_null());
            // SAFETY: inside the newly committed half.
            unsafe { std::ptr::write_volatile((base + 5 * PAGE_BYTES) as *mut u8, 7) };

            let after = snapshot_map(&map);
            assert_eq!(after.regions, 1);
            assert_eq!(after.committed_bytes, 4 * PAGE_BYTES);
            assert_eq!(after.touched_bytes, PAGE_BYTES);

            // SAFETY: ours.
            assert_ne!(unsafe { VirtualFree(base as *mut c_void, 0, MEM_RELEASE) }, 0);
            let freed = snapshot_map(&map);
            assert_eq!(freed.regions, 0);
            assert!(map.lock().unwrap().is_empty());
        }
    }
}
