//! The device titles get from device creation, laid out as their inlined
//! code expects: the object header, then device fields. Its table starts
//! like the PC device interface (IUnknown, the object methods, then the
//! device methods in PC order); console additions follow. Methods we have
//! not written log their table offset and fail, so a run shows what to
//! write next.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::object::{
    self, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, HRESULT, Iid, Object, S_FALSE, S_OK, Table, This,
    guid, guid_text,
};
use crate::{
    descriptor, dxgi, fence, frame, list, pipeline, query, queue, resource, root, signature,
};

/// What D3D returns when none of the requested feature levels is supported.
const UNSUPPORTED: HRESULT = 0x887A_0004_u32 as i32;

/// The device interface id (the same as on PC). Titles also import it as
/// data.
pub static IID: Iid = guid(
    0x189819f1,
    0x1db6,
    0x4b57,
    [0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7],
);

/// The levels the device answers to, as on PC: `ID3D12Device1` to
/// `ID3D12Device8`, and `ID3D12Device11`, which the title creates
/// its device as. Each level's table is the one before plus its methods.
const LEVEL_IIDS: [Iid; 9] = [
    guid(
        0x77acce80,
        0x638e,
        0x4e65,
        [0x88, 0x95, 0xc1, 0xf2, 0x33, 0x86, 0x86, 0x3e],
    ),
    guid(
        0x30baa41e,
        0xb15b,
        0x475c,
        [0xa0, 0xbb, 0x1a, 0xf5, 0xc5, 0xb6, 0x43, 0x28],
    ),
    guid(
        0x81dadc15,
        0x2bad,
        0x4392,
        [0x93, 0xc5, 0x10, 0x13, 0x45, 0xc4, 0xaa, 0x98],
    ),
    guid(
        0xe865df17,
        0xa9ee,
        0x46f9,
        [0xa4, 0x63, 0x30, 0x98, 0x31, 0x5a, 0xa2, 0xe5],
    ),
    guid(
        0x8b4f173b,
        0x2fea,
        0x4b80,
        [0x8f, 0x58, 0x43, 0x07, 0x19, 0x1a, 0xb9, 0x5d],
    ),
    guid(
        0xc70b221b,
        0x40e4,
        0x4a17,
        [0x89, 0xaf, 0x02, 0x5a, 0x07, 0x27, 0xa6, 0xdc],
    ),
    guid(
        0x5c014b53,
        0x68a1,
        0x4b9b,
        [0x8b, 0xd1, 0xdd, 0x60, 0x46, 0xb9, 0x35, 0x8b],
    ),
    guid(
        0x728f1fa2,
        0xea11,
        0x4a9f,
        [0x9b, 0x52, 0x9b, 0x42, 0xa4, 0x84, 0xd5, 0xc9],
    ),
    guid(
        0x5405c344,
        0xd457,
        0x444e,
        [0xb4, 0xdd, 0x23, 0x66, 0xe4, 0x5a, 0xee, 0x39],
    ),
];

/// Whether `iid` names the device: its first interface or a newer level.
pub(crate) fn is_device(iid: *const Iid) -> bool {
    object::is(iid, &[IID]) || object::is(iid, &LEVEL_IIDS)
}

/// The interface version these layouts were checked against (2.4). Both
/// Minecraft (March 2022) and the title (April 2026) pass it.
pub const CHECKED_VERSION: u32 = 0x0002_0004;

/// Feature level 12_2, what the console hardware supports.
const HIGHEST_FEATURE_LEVEL: u32 = 0xc200;

#[repr(C)]
pub struct Device {
    object: Object,
    /// GPU address of a null constant buffer; none yet.
    null_constant_buffer: u64,
    /// Byte 0 non-zero: calls the title would otherwise inline come to the
    /// table. The rest is reserved.
    out_of_line: [u32; 16],
}
const _: () = assert!(size_of::<Device>() == 0x58);

/// One device per process, as on the console.
pub(crate) static DEVICE: Device = Device {
    object: Object::new(&FUNCTIONS),
    null_constant_buffer: 0,
    out_of_line: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
};

pub(crate) fn device() -> This {
    (&raw const DEVICE).cast()
}

/// Device creation: `(adapter, parameters, iid, out)`. The parameters start
/// with the interface version; the rest (ring and scratch sizes, features
/// to leave out) does not matter to us yet.
pub extern "system" fn create(
    _adapter: *const c_void,
    parameters: *const u32,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if parameters.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes its parameters and an id.
    let (version, iid) = unsafe { (parameters.read(), &*iid) };
    tracing::info!(version = format_args!("{version:#x}"), "D3D12.X device");
    if version != CHECKED_VERSION {
        tracing::warn!(
            version = format_args!("{version:#x}"),
            "device layouts were checked against {CHECKED_VERSION:#x} only"
        );
    }
    if !is_device(iid) {
        tracing::warn!(iid = %guid_text(iid), "device created as an unknown interface");
        return E_NOINTERFACE;
    }
    if out.is_null() {
        return S_FALSE;
    }
    if *iid == LEVEL_IIDS[8] {
        NEWER.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    object::answer(object::DEVICE, iid, Some(device()), out)
}

/// Set when the title creates the device as `ID3D12Device11`. Its lists and
/// queues then have the newer layouts (two more methods each; see
/// `list` and `queue`), although the interface version stays 2.4.
/// shortcut: the creation interface is the only sign of the newer layouts
/// we have (the title, April 2026, versus Minecraft, March 2022);
/// look for a better one if a title breaks the rule.
static NEWER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn newer() -> bool {
    NEWER.load(std::sync::atomic::Ordering::Relaxed)
}

/// The device, or its DXGI side.
extern "system" fn query_interface(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = if is_device(iid) {
        Some(this)
    } else if object::is(iid, &dxgi::DEVICE_IIDS) {
        Some(dxgi::device())
    } else {
        None
    };
    object::answer(object::DEVICE, iid, found, out)
}

/// What `D3D12ConfigureVirtualMemoryX` takes and fills in.
#[repr(C)]
pub struct VirtualMemory {
    flags: u32,
    /// GPU page table size in 4 MiB pages.
    page_table_pages: u32,
    /// Memory kept across title processes for the frame buffer.
    preserved_bytes: u32,
    /// Out: the range graphics addresses fall in.
    lowest: u64,
    highest: u64,
}
const _: () = assert!(size_of::<VirtualMemory>() == 0x20);

/// `d3d12_xs!D3D12ConfigureVirtualMemoryX`, called before any graphics
/// allocation. The inputs size console structures we do not have; the
/// range out is all of user space, since GPU addresses are CPU addresses.
pub extern "system" fn configure_virtual_memory(config: *mut VirtualMemory) -> HRESULT {
    // SAFETY: the title passes its configuration (null is tolerated).
    let Some(config) = (unsafe { config.as_mut() }) else {
        return E_INVALIDARG;
    };
    tracing::info!(
        flags = config.flags,
        page_table_pages = config.page_table_pages,
        preserved_bytes = config.preserved_bytes,
        "graphics virtual memory"
    );
    (config.lowest, config.highest) = user_space();
    S_OK
}

fn user_space() -> (u64, u64) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        // SAFETY: plain data, filled in by the call.
        let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer.
        unsafe { GetSystemInfo(&mut info) };
        (
            info.lpMinimumApplicationAddress as u64,
            info.lpMaximumApplicationAddress as u64,
        )
    }
    #[cfg(not(windows))]
    (0x1_0000, 0x7fff_fffe_ffff)
}

extern "system" fn node_count(_: This) -> u32 {
    1
}

/// Feature query 2: the highest of the requested feature levels we support.
#[repr(C)]
struct FeatureLevels {
    count: u32,
    requested: *const u32,
    highest: u32,
}

/// Feature query 0, what an RDNA 2 GPU reports. The console's layout has
/// one field more than PC's (index 11).
const OPTIONS: [u32; 16] = [
    1,  // double-precision shader operations
    1,  // output-merger logic operations
    2,  // minimum precision: 16-bit
    3,  // tiled resources tier
    3,  // resource binding tier
    1,  // stencil reference from pixel shaders
    1,  // typed UAV loads of more formats
    1,  // rasterizer-ordered views
    3,  // conservative rasterization tier
    40, // GPU address bits per resource
    0,  // 64 KiB standard swizzle
    0,  // console-only field
    0,  // cross-node sharing tier
    0,  // cross-adapter row-major textures
    1,  // viewport and render target index from any stage
    2,  // resource heap tier
];

extern "system" fn check_feature_support(
    _: This,
    feature: u32,
    data: *mut c_void,
    size: u32,
) -> HRESULT {
    const OPTIONS_QUERY: u32 = 0;
    const ARCHITECTURE: u32 = 1;
    const FEATURE_LEVELS: u32 = 2;
    const FORMAT_SUPPORT: u32 = 3;
    const MULTISAMPLE: u32 = 4;
    const SHADER_MODEL: u32 = 7;
    const OPTIONS1: u32 = 8;
    const OPTIONS4: u32 = 23;
    let expected = match feature {
        OPTIONS_QUERY => size_of_val(&OPTIONS),
        ARCHITECTURE => 16,
        FEATURE_LEVELS => size_of::<FeatureLevels>(),
        FORMAT_SUPPORT => 12,
        MULTISAMPLE => 16,
        SHADER_MODEL => 4,
        OPTIONS1 => 24,
        OPTIONS4 => 12,
        _ => {
            tracing::warn!(feature, size, "device feature query not implemented");
            return E_NOTIMPL;
        }
    };
    if data.is_null() || size as usize != expected {
        return E_INVALIDARG;
    }
    match feature {
        OPTIONS_QUERY => {
            // SAFETY: checked size and non-null.
            unsafe { data.cast::<[u32; 16]>().write_unaligned(OPTIONS) };
            S_OK
        }
        ARCHITECTURE => {
            // SAFETY: checked size and non-null.
            let a = unsafe { &mut *data.cast::<[u32; 4]>() };
            if a[0] != 0 {
                return E_INVALIDARG; // one node only
            }
            // Not tile-based; unified, cache-coherent memory.
            a[1..].copy_from_slice(&[0, 1, 1]);
            S_OK
        }
        FORMAT_SUPPORT => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 3]>() };
            // shortcut: every use of every format; answer from the host
            // device once there is one.
            q[1] = 0x7fff_ffff;
            q[2] = 0xffff;
            S_OK
        }
        MULTISAMPLE => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 4]>() };
            // Format, sample count, flags in; one quality level out for the
            // counts the hardware has.
            q[3] = u32::from(matches!(q[1], 1 | 2 | 4 | 8));
            S_OK
        }
        SHADER_MODEL => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<u32>() };
            // The highest asked for, up to 6.6 (the AMD upscaler asks).
            *q = (*q).min(0x66);
            S_OK
        }
        OPTIONS1 => {
            let lanes = crate::console::current().compute_units() * 64;
            // SAFETY: checked size and non-null.
            unsafe {
                data.cast::<[u32; 6]>()
                    .write_unaligned([1, 32, 64, lanes, 1, 1])
            };
            S_OK
        }
        OPTIONS4 => {
            // SAFETY: checked size and non-null. Native 16-bit shader operations.
            unsafe { data.cast::<[u32; 3]>().write_unaligned([0, 0, 1]) };
            S_OK
        }
        _ => feature_levels(data),
    }
}

fn feature_levels(data: *mut c_void) -> HRESULT {
    // SAFETY: the caller checked size and non-null.
    let query = unsafe { &mut *data.cast::<FeatureLevels>() };
    if query.requested.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `count` levels.
    let requested = unsafe { std::slice::from_raw_parts(query.requested, query.count as usize) };
    match requested
        .iter()
        .copied()
        .filter(|&l| l <= HIGHEST_FEATURE_LEVEL)
        .max()
    {
        Some(level) => {
            query.highest = level;
            S_OK
        }
        None => UNSUPPORTED,
    }
}

/// Device methods 0x1c8 and 0x218 (`SLOT`). The title calls both
/// (title+0x2477578 and title+0x24800d5) as `(this, description, interface
/// id, out)` and asks for `ID3D12PipelineState`. What the logged calls'
/// descriptions held (run of 2026-11-25):
///
/// * word 0 is one of our root signatures ([`crate::root`]);
/// * then (address, length) pairs into title memory. 0x218 passes 12-word
///   elements of an array (words 12-15 of one call were words 0-3 of the
///   next): a 0x1c-byte blob shared by neighbouring elements, a 0x14-byte
///   one ending where the metadata starts, the code, the metadata, two
///   addresses and a 64-bit value like a hash. 0x1c8 passes two blobs
///   (0x184, 0x18c or 0x200 bytes, then 0x50 to 0xbc), then groups of four
///   words (code, length, metadata, length), zero for an absent group;
///   neighbouring calls can share groups. How many groups there are, and
///   which stage each is, were not known (that run logged 16 words; this
///   build logs 40, [`unknown_dump`]);
/// * code lies on 256-byte boundaries, in lengths of 64-byte units, as
///   GFX10 programs would (not checked in that run, which logged no code
///   bytes; this build logs the first 64 bytes of each blob and a hash).
///   Metadata are 0x18 (0x218) or 0x10 (0x1c8) bytes plus 32-byte units:
///   not the layout of the `FSSM` records of a container
///   ([`victory_shader::native`]), whose 20-byte tables would not keep
///   every length on one 32-byte step.
///
/// With one stage per description, 0x218 is likely the compute form and
/// 0x1c8 the graphics one. The calls fail as they always did, with `out`
/// null, and the title goes on without the object; whether it then makes
/// the same pipeline through the standard methods is counted
/// ([`standard_pipeline`]). Calls 1-4 and each power of two log the
/// description's first 16 words and the 16 words its first word points at;
/// the first calls of each layout log every blob it points at
/// (`unknown_dump`), and every 1024th call the count.
/// shortcut: not implemented. Answering `S_OK` here without making the
/// pipeline would hand the title whatever its stack held as an object.
extern "system" fn unknown_pipeline<const SLOT: usize>(
    _: This,
    desc: *const u8,
    _iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    let calls = UNKNOWN_CALLS[usize::from(SLOT == 0x218)].fetch_add(1, Ordering::Relaxed) + 1;
    if !out.is_null() {
        // SAFETY: the title's out pointer (a local of its own).
        unsafe { *out = std::ptr::null() };
    }
    #[cfg(windows)]
    unknown_dump::dump::<SLOT>(desc, calls);
    if calls.is_multiple_of(1024) {
        tracing::info!(
            slot = format_args!("{:#x}", SLOT),
            calls,
            "a pipeline creation method that is not implemented: calls so far"
        );
    }
    if calls <= 4 || calls.is_power_of_two() {
        // Sixteen 64-bit words at `at`: neither pointer is known to be mapped,
        // and a guard page is not read ([`unknown_dump::plain`]).
        #[cfg(windows)]
        let words = |at: usize| -> Option<Vec<u64>> {
            let mut bytes = [0u8; 128];
            (at != 0 && unknown_dump::plain(at as u64, bytes.len()) && crate::host::read_into(at as *const u8, &mut bytes)).then(|| {
                bytes
                    .as_chunks::<8>().0.iter()
                    .map(|c| c.iter().rev().fold(0u64, |v, &b| v << 8 | u64::from(b)))
                    .collect()
            })
        };
        #[cfg(not(windows))]
        let words = |_: usize| None::<Vec<u64>>;
        let description = words(desc as usize);
        let first = description
            .as_ref()
            .and_then(|w| words(w[0] as usize));
        tracing::warn!(
            slot = format_args!("{:#x}", SLOT),
            calls,
            description = format_args!("{description:x?}"),
            first_points_at = format_args!("{first:x?}"),
            "a pipeline creation method that is not implemented: the title goes without this pipeline"
        );
    }
    LAST_UNKNOWN.with(|l| l.set(Some((SLOT, Instant::now()))));
    E_NOTIMPL
}

/// Calls to the unknown pipeline methods ([`unknown_pipeline`]): 0x1c8, 0x218.
static UNKNOWN_CALLS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

thread_local! {
    /// This thread's last unknown pipeline call not yet followed by a
    /// pipeline made through a standard method: its slot and when.
    static LAST_UNKNOWN: Cell<Option<(usize, Instant)>> = const { Cell::new(None) };
}

/// A title pipeline made through standard method `method` (0x50, 0x58,
/// 0x1f0, 0x2b0), the title having called at `asked`: counted; and counted
/// apart, by how soon (under 20 µs, 200 µs, 2 ms, later), when this
/// thread's last unknown pipeline call came before it: a title that falls
/// back to the standard methods shows about one per failed call, soon
/// after it. Called once the method made the pipeline, so calls that fail
/// count nothing and leave the last unknown call for the next.
pub(crate) fn standard_pipeline(method: usize, asked: Instant) {
    const SOON: [Duration; 3] = [Duration::from_micros(20), Duration::from_micros(200), Duration::from_millis(2)];
    static MADE: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
    /// method * 8 + slot (0 0x1c8, 1 0x218) * 4 + how soon
    static AFTER: [AtomicU32; 32] = [const { AtomicU32::new(0) }; 32];
    /// Per method, how many of its pipelines came after an unknown call.
    static AFTER_ANY: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
    let Some(m) = [0x50, 0x58, 0x1f0, 0x2b0].iter().position(|&x| x == method) else {
        return;
    };
    let made = MADE[m].fetch_add(1, Ordering::Relaxed) + 1;
    let mut after_any = 0;
    if let Some((slot, at)) = LAST_UNKNOWN.with(Cell::take) {
        let soon = SOON.iter().position(|&l| asked.saturating_duration_since(at) < l).unwrap_or(3);
        AFTER[m * 8 + usize::from(slot == 0x218) * 4 + soon].fetch_add(1, Ordering::Relaxed);
        after_any = AFTER_ANY[m].fetch_add(1, Ordering::Relaxed) + 1;
    }
    // (also the first pipelines, and each power of two of those after an
    // unknown call: a method that makes few still says whether it follows one)
    if made <= 4 || made.is_power_of_two() && made >= 16 || made.is_multiple_of(4096) || after_any.is_power_of_two() {
        let after = |s: usize| (0..4).map(|k| AFTER[m * 8 + s * 4 + k].load(Ordering::Relaxed)).collect::<Vec<_>>();
        tracing::info!(
            method = format_args!("{method:#x}"),
            made,
            after_0x1c8 = ?after(0),
            after_0x218 = ?after(1),
            calls_0x1c8 = UNKNOWN_CALLS[0].load(Ordering::Relaxed),
            calls_0x218 = UNKNOWN_CALLS[1].load(Ordering::Relaxed),
            "pipelines made through a standard method so far; of them right after a failed unknown pipeline call on the same thread (under 20 µs, 200 µs, 2 ms, later)"
        );
    }
}

/// What [`unknown_pipeline`]'s descriptions point at, byte by byte: for the
/// first 4 calls of each slot and the first 2 of each layout, every blob as
/// little-endian dwords (512 bytes of state and metadata, 64 of code) and a
/// hash of all of it. At most `MOST` calls per slot, among its first
/// `LOOKED`; release builds log it.
#[cfg(windows)]
mod unknown_dump {
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

    /// A 0x218 element is 12 words (the next one follows); 0x1c8 has a root,
    /// two blobs and four-word stage groups, how many is not known.
    const WORDS: usize = 40;
    const SHOWN: usize = 512;
    const SHOWN_CODE: usize = 64;
    const SHOWN_POINTED: usize = 64;
    /// Longest blob read whole (longest seen: code of 0x1f9c0 bytes).
    const LONGEST: u64 = 4 << 20;
    const MOST: u32 = 48;
    /// Calls looked at per slot: past them a call costs nothing here, even
    /// when the title has fewer layouts than `MOST` dumps would take.
    const LOOKED: u32 = 256;
    /// Graphics groups a short read (`MIN_BYTES`) covers in full; the layout
    /// key names only these, so the length read does not change it.
    const KEY_GROUPS: u32 = 2;

    static DUMPED: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
    /// Calls seen per slot and layout.
    type Layouts = Mutex<Option<HashMap<(usize, [u64; 3]), u32>>>;
    static LAYOUTS: Layouts = Mutex::new(None);

    /// Whether `[at, at + len)` is committed memory that can be read and has
    /// no guard page. What the dump reads is guessed, and the in-process copy
    /// (`crate::host::read_into`) takes a guard page's one-shot exception,
    /// which disarms the title's guard (a stack that can then not grow).
    pub(super) fn plain(at: u64, len: usize) -> bool {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
        };
        let end = at.saturating_add(len as u64);
        let mut p = at;
        while p < end {
            // SAFETY: a query of an address into a local structure.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            let got = unsafe { VirtualQuery(p as usize as *const _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
            if got == 0 || info.State != MEM_COMMIT || info.Protect == 0 || info.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0 {
                return false;
            }
            p = (info.BaseAddress as u64).saturating_add(info.RegionSize as u64);
        }
        true
    }

    fn read(at: u64, len: u64, most: usize) -> Option<Vec<u8>> {
        if at == 0 || len == 0 || len > LONGEST {
            return None;
        }
        let mut bytes = vec![0; (len as usize).min(most)];
        (plain(at, bytes.len()) && crate::host::read_into(at as usize as *const u8, &mut bytes)).then_some(bytes)
    }

    #[test]
    fn a_guessed_read_leaves_a_guard_page_armed() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE,
            VirtualAlloc, VirtualFree, VirtualQuery,
        };
        // SAFETY: fresh pages of our own, freed below.
        let (guarded, open) = unsafe {
            (
                VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE | PAGE_GUARD),
                VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE),
            )
        };
        assert!(!guarded.is_null() && !open.is_null());
        assert!(read(guarded as u64, 64, 64).is_none(), "a guard page is not read");
        // SAFETY: a structure of integers and pointers, valid all zero.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a query into a local structure.
        unsafe { VirtualQuery(guarded, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
        assert_ne!(info.Protect & PAGE_GUARD, 0, "the title's guard is still armed");
        assert_eq!(read(open as u64, 64, 64).map(|b| b.len()), Some(64), "plain memory is read");
        // SAFETY: the pages allocated above.
        unsafe {
            VirtualFree(guarded, 0, MEM_RELEASE);
            VirtualFree(open, 0, MEM_RELEASE);
        }
    }

    fn dwords(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() / 4 * 9);
        for (i, c) in bytes.chunks(4).enumerate() {
            let mut w = [0; 4];
            w[..c.len()].copy_from_slice(c);
            let _ = write!(s, "{}{:08x}", if i == 0 { "" } else { " " }, u32::from_le_bytes(w));
        }
        s
    }

    /// Four words at `at`: an absent group (all zero), a present one (code on
    /// a 256-byte boundary, length in 64-byte units, metadata, its length),
    /// or neither (None: the groups end).
    fn group(w: &[u64], at: usize) -> Option<bool> {
        let g = w.get(at..at + 4)?;
        if g.iter().all(|&v| v == 0) {
            return Some(false);
        }
        let code = g[0] != 0 && g[0] % 0x100 == 0 && g[1] % 0x40 == 0 && (0x40..=LONGEST).contains(&g[1]);
        (code && g[2] != 0 && (1..=LONGEST).contains(&g[3])).then_some(true)
    }

    pub(super) fn dump<const SLOT: usize>(desc: *const u8, calls: u32) {
        let compute = SLOT == 0x218;
        let dumped = &DUMPED[usize::from(compute)];
        if desc.is_null() || calls > LOOKED || dumped.load(Relaxed) >= MOST {
            return;
        }
        // A 0x218 element is 12 words; 0x1c8 has at least a root, two blobs
        // and two groups (13 words), read as 16.
        let min_words = if compute { 12 } else { 16 };
        let mut bytes = [0u8; WORDS * 8];
        let readable = |n: usize, bytes: &mut [u8]| plain(desc as u64, n * 8) && crate::host::read_into(desc, &mut bytes[..n * 8]);
        let n = if readable(WORDS, &mut bytes) {
            WORDS
        } else if readable(min_words, &mut bytes) {
            min_words
        } else {
            return;
        };
        let w: Vec<u64> = bytes[..n * 8].as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
        let names = if compute { ["small 1", "small 2"] } else { ["state 1", "state 2"] };
        let mut blobs: Vec<(String, usize)> = vec![(names[0].into(), 1), (names[1].into(), 3)];
        let mut filled = 0u64;
        let class = |v: u64| u64::from(v.checked_ilog2().unwrap_or(0));
        let (end, sizes) = if compute {
            blobs.push(("code".into(), 5));
            blobs.push(("metadata".into(), 7));
            (12, class(w[8]) << 8 | class(w[6]))
        } else {
            // (zeroed memory past the description reads as absent groups: the
            // description ends with its last present group)
            let (mut at, mut i, mut last) = (5, 0, 5);
            while let Some(here) = group(&w, at) {
                if here {
                    filled |= 1 << i;
                    blobs.push((format!("code {i}"), at));
                    blobs.push((format!("metadata {i}"), at + 2));
                    last = at + 4;
                }
                at += 4;
                i += 1;
            }
            (last, filled & ((1 << KEY_GROUPS) - 1))
        };
        // (for graphics, not the end and only the first groups: memory past
        // the description, or a short read, could make one layout look like two)
        let layout = [w[2], w[4], if compute { sizes | (end as u64) << 32 } else { sizes }];
        {
            let mut layouts = LAYOUTS.lock().unwrap_or_else(|e| e.into_inner());
            let seen = layouts.get_or_insert_with(HashMap::new).entry((SLOT, layout)).or_default();
            *seen += 1;
            if calls > 4 && *seen > 2 {
                return;
            }
        }
        if dumped.fetch_add(1, Relaxed) >= MOST {
            return;
        }
        let slot = format!("{SLOT:#x}");
        if compute {
            tracing::warn!(slot = %slot, calls, words = end, description = format_args!("{:x?}", &w[..n]),
                "a pipeline creation method that is not implemented: its description");
        } else {
            // (`words`: up to the last present group; `read`: how many were read)
            tracing::warn!(slot = %slot, calls, words = end, read = n, groups = format_args!("{filled:#b}"),
                description = format_args!("{:x?}", &w[..n]),
                "a pipeline creation method that is not implemented: its description");
        }
        for (name, at) in &blobs {
            let (to, len) = (w[*at], w[*at + 1]);
            let most = if name.starts_with("code") { SHOWN_CODE } else { SHOWN };
            let shown = read(to, len, most).map(|b| dwords(&b));
            let hash = read(to, len, usize::MAX).map(|b| blake3::hash(&b).to_hex()[..16].to_owned());
            tracing::warn!(slot = %slot, calls, blob = %name, at = format_args!("{to:#x}"),
                len = format_args!("{len:#x}"), hash = hash.as_deref().unwrap_or("unreadable"),
                bytes = shown.as_deref().unwrap_or("unreadable"),
                "a pipeline creation method that is not implemented: a blob of its description");
        }
        if compute {
            for at in [9, 10] {
                if let Some(b) = read(w[at], SHOWN_POINTED as u64, SHOWN_POINTED) {
                    let text: String = b.iter().map(|&c| if c.is_ascii_graphic() { c as char } else { '.' }).collect();
                    tracing::warn!(slot = %slot, calls, word = at, bytes = %dwords(&b), text = %text,
                        "a pipeline creation method that is not implemented: what a word of its description points at");
                }
            }
        }
    }
}

/// The console device uses the host device's removal state.
extern "system" fn removed_reason(_: This) -> HRESULT {
    #[cfg(windows)]
    if let Some(host) = crate::host::host() {
        // SAFETY: a plain query on a live device.
        if let Err(error) = unsafe { host.device.GetDeviceRemovedReason() } {
            return error.code().0;
        }
    }
    S_OK
}

/// IUnknown 3, object methods 4, device methods 76 (0x18-0x290), then the
/// methods `ID3D12Device1` to `ID3D12Device8` add, in order: 3, 1, 3, 6, 9,
/// 1, 2, 9 (0x298-0x3a0, the console's own among them), then those of
/// `ID3D12Device9` to `ID3D12Device11`, whose console layout is unknown.
/// Every slot exists so that a call to one we have not written logs it.
static FUNCTIONS: Table<160> = {
    let mut f = object::d3d12_object(object::missing_table::<{ object::DEVICE }, 160>());
    f[0] = query_interface as *const (); // 0x00
    f[7] = node_count as *const (); // 0x38
    f[8] = queue::create as *const (); // 0x40
    f[9] = list::create_allocator as *const (); // 0x48
    f[12] = list::create_list as *const (); // 0x60
    f[10] = pipeline::create_graphics as *const (); // 0x50
    f[11] = pipeline::create_compute as *const (); // 0x58
    f[13] = check_feature_support as *const (); // 0x68
    f[14] = descriptor::create_heap as *const (); // 0x70
    f[15] = descriptor::increment_size as *const (); // 0x78
    f[16] = root::create as *const (); // 0x80
    f[17] = descriptor::create_cbv as *const (); // 0x88
    f[18] = descriptor::create_srv as *const (); // 0x90
    f[19] = descriptor::create_uav as *const (); // 0x98
    f[20] = descriptor::create_rtv as *const (); // 0xa0
    f[21] = descriptor::create_dsv as *const (); // 0xa8
    f[22] = descriptor::create_sampler as *const (); // 0xb0
    f[23] = descriptor::copy as *const (); // 0xb8
    f[24] = descriptor::copy_simple as *const (); // 0xc0
    f[25] = resource::allocation_info as *const (); // 0xc8
    f[26] = resource::custom_heap_properties as *const (); // 0xd0
    f[27] = resource::create_committed as *const (); // 0xd8
    f[28] = resource::create_heap as *const (); // 0xe0
    f[29] = resource::create_placed as *const (); // 0xe8
    f[36] = fence::create as *const (); // 0x120
    f[37] = removed_reason as *const (); // 0x128
    f[38] = resource::copyable_footprints as *const (); // 0x130
    f[39] = query::create as *const (); // 0x138
    f[41] = signature::create as *const (); // 0x148
    f[48] = queue::create_x as *const (); // 0x180
    f[57] = unknown_pipeline::<0x1c8> as *const (); // 0x1c8
    f[61] = descriptor::create_sampler_x as *const (); // 0x1e8
    f[62] = pipeline::create_compute_x as *const (); // 0x1f0
    f[67] = unknown_pipeline::<0x218> as *const (); // 0x218
    f[69] = object::ignore as *const (); // 0x228 debug error filter
    f[70] = descriptor::create_placed_srv as *const (); // 0x230
    f[71] = descriptor::create_placed_uav as *const (); // 0x238
    f[73] = list::create_list_x as *const (); // 0x248
    f[74] = resource::register_page_pool as *const (); // 0x250
    f[75] = resource::unregister_page_pool as *const (); // 0x258
    // CreateCommittedResourceX: its description is PC's plus DCC options,
    // which we have no use for (the AMD upscaler's, on Series S).
    f[76] = resource::create_committed as *const (); // 0x260
    f[78] = frame::set_interval as *const (); // 0x270
    f[79] = frame::schedule as *const (); // 0x278
    f[80] = frame::wait as *const (); // 0x280
    f[81] = frame::statistics as *const (); // 0x288
    f[86] = pipeline::create_stream as *const (); // 0x2b0
    f[108] = resource::allocation_info2 as *const (); // 0x360
    f[127] = resource::create_committed3 as *const (); // 0x3f8
    f[133] = resource::create_at_address as *const (); // 0x428
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    fn create_as(iid: &Iid) -> (HRESULT, This) {
        let version = CHECKED_VERSION;
        let mut device = std::ptr::null();
        let hr = create(std::ptr::null(), &version, iid, &mut device);
        (hr, device)
    }

    #[test]
    fn creation_hands_out_the_device() {
        let (hr, device) = create_as(&IID);
        assert_eq!(hr, S_OK);
        assert_eq!(device, super::device());
        // The title tests byte 0x18 before inlining a device call.
        // SAFETY: the device is 0x58 bytes.
        assert_ne!(unsafe { device.cast::<u8>().add(0x18).read() }, 0);
        assert_eq!(create_as(&[0; 16]).0, E_NOINTERFACE);
        assert_eq!(
            slot::<extern "system" fn(This) -> u32>(device, 0x38)(device),
            1
        );
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0xf0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    #[test]
    fn a_healthy_device_reports_no_removal() {
        let device = device();
        let reason = slot::<extern "system" fn(This) -> HRESULT>(device, 0x128);
        assert_eq!(reason(device), S_OK);
    }

    #[test]
    fn newer_device_levels_are_the_same_device() {
        // ID3D12Device11, as the title asks for it.
        let (hr, device) = create_as(&LEVEL_IIDS[8]);
        assert_eq!((hr, device), (S_OK, super::device()));
        let mut same = std::ptr::null();
        assert_eq!(query_interface(device, &LEVEL_IIDS[4], &mut same), S_OK);
        assert_eq!(same, device);
        // A Device8 method we have not written fails without crashing.
        let missing = slot::<extern "system" fn() -> HRESULT>(device, 0x3a0);
        assert_eq!(missing(), E_NOTIMPL);
    }

    /// A device child, and the DXGI device, hand out the device as any
    /// interface the device itself answers to.
    #[test]
    fn the_device_is_found_at_every_level_through_others() {
        type Get = extern "system" fn(This, *const Iid, *mut This) -> HRESULT;
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        let mut fence = std::ptr::null();
        assert_eq!(
            crate::fence::create(std::ptr::null(), 0, 0, fence_iid, &mut fence),
            S_OK
        );
        let get_device = slot::<Get>(fence, 0x38);
        let dxgi = dxgi::device();
        let query = slot::<Get>(dxgi, 0);
        for iid in std::iter::once(&IID).chain(&LEVEL_IIDS) {
            let mut found = std::ptr::null();
            assert_eq!(get_device(fence, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, device());
            let mut found = std::ptr::null();
            assert_eq!(query(dxgi, iid, &mut found), S_OK, "{iid:02x?}");
            assert_eq!(found, device());
        }
        // What the device is not, it is not through them either.
        let mut found = device();
        assert_eq!(get_device(fence, fence_iid, &mut found), E_NOINTERFACE);
        assert!(found.is_null());
        slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence);
    }

    /// The host's: a fence's `GetDevice` for `ID3D12Device5`.
    #[cfg(windows)]
    #[test]
    fn the_hosts_children_hand_out_newer_device_levels() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_FENCE_FLAG_NONE, ID3D12Device5, ID3D12Fence,
        };
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        // SAFETY: plain calls on the host device and its fence.
        unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let mut device: Option<ID3D12Device5> = None;
            assert!(fence.GetDevice(&mut device).is_ok());
            assert!(device.is_some());
        }
    }

    #[test]
    fn virtual_memory_configuration_reports_user_space() {
        let mut config = VirtualMemory {
            flags: 0,
            page_table_pages: 7,
            preserved_bytes: 0,
            lowest: 0,
            highest: 0,
        };
        assert_eq!(configure_virtual_memory(&mut config), S_OK);
        assert!(0 < config.lowest && config.lowest < config.highest);
        let heap = Box::new(0u8);
        assert!((config.lowest..=config.highest).contains(&(&raw const *heap as u64)));
        assert_eq!(configure_virtual_memory(std::ptr::null_mut()), E_INVALIDARG);
    }

    #[test]
    fn feature_levels_pick_the_highest_supported() {
        type Check = extern "system" fn(This, u32, *mut c_void, u32) -> HRESULT;
        let device = device();
        let check = slot::<Check>(device, 0x68);
        let query = |levels: &[u32], size| {
            let mut query = FeatureLevels {
                count: levels.len() as u32,
                requested: levels.as_ptr(),
                highest: 0,
            };
            let hr = check(device, 2, (&raw mut query).cast(), size);
            (hr, query.highest)
        };
        let levels = [0xb000, 0xb100, 0xc000, 0xc100, 0xd000];
        assert_eq!(query(&levels, 0x18), (S_OK, 0xc100));
        assert_eq!(query(&levels, 0x10).0, E_INVALIDARG);
        assert_eq!(query(&levels[4..], 0x18).0, UNSUPPORTED);
    }

    /// The queries the AMD upscaler's capability check makes.
    #[test]
    fn shader_model_and_wave_options_are_the_consoles() {
        type Check = extern "system" fn(This, u32, *mut c_void, u32) -> HRESULT;
        let device = device();
        let check = slot::<Check>(device, 0x68);
        let mut model = 0x67u32;
        assert_eq!(check(device, 7, (&raw mut model).cast(), 4), S_OK);
        assert_eq!(model, 0x66);
        model = 0x62;
        assert_eq!(check(device, 7, (&raw mut model).cast(), 4), S_OK);
        assert_eq!(model, 0x62);
        let mut waves = [0u32; 6];
        assert_eq!(check(device, 8, waves.as_mut_ptr().cast(), 24), S_OK);
        assert_eq!(waves[..3], [1, 32, 64]);
        let mut options4 = [0u32; 3];
        assert_eq!(check(device, 23, options4.as_mut_ptr().cast(), 12), S_OK);
        assert_eq!(options4[2], 1, "native 16-bit operations");
        assert_eq!(
            check(device, 8, waves.as_mut_ptr().cast(), 16),
            E_INVALIDARG
        );
    }

}
