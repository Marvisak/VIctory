//! The device titles get from device creation, laid out as their inlined
//! code expects: the object header, then device fields. Its table starts
//! like the PC device interface (IUnknown, the object methods, then the
//! device methods in PC order); console additions follow. Methods we have
//! not written log their table offset and fail, so a run shows what to
//! write next.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

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
/// `ID3D12Device8`, and `ID3D12Device11`, which Grand Theft Auto VI creates
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
/// Minecraft (March 2022) and Grand Theft Auto VI (April 2026) pass it.
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
/// we have (Grand Theft Auto VI, April 2026, versus Minecraft, March 2022);
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

/// `MakeResident` and `Evict` `(count, objects)`: our objects are always
/// where the title left them, and the host pages its own.
extern "system" fn residency(_: This, count: u32, objects: *const This) -> HRESULT {
    if count > 0 && objects.is_null() {
        E_INVALIDARG
    } else {
        S_OK
    }
}

/// `SetStablePowerState(enable)`: the host's clocks are its own business.
extern "system" fn stable_power_state(_: This, _enable: i32) -> HRESULT {
    S_OK
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

/// Feature query ids, as on PC.
const OPTIONS_QUERY: u32 = 0;
const ARCHITECTURE: u32 = 1;
const FEATURE_LEVELS: u32 = 2;
const FORMAT_SUPPORT: u32 = 3;
const MULTISAMPLE: u32 = 4;
const FORMAT_INFO: u32 = 5;
const GPU_VIRTUAL_ADDRESS: u32 = 6;
const SHADER_MODEL: u32 = 7;
const OPTIONS1: u32 = 8;
const ARCHITECTURE1: u32 = 16;
const OPTIONS4: u32 = 23;

/// Bits of a GPU address in one resource, as in [`OPTIONS`].
const ADDRESS_BITS_PER_RESOURCE: u32 = 40;

/// `CheckFeatureSupport(feature, data, size)`. Ids are taken to be PC's, as
/// the answers for 0-4 always took them; a size that is not the
/// structure's fails with `E_INVALIDARG`, as on PC.
///
/// What the console's hardware or our memory model decides is answered
/// here: the options (in the console's layout), the architecture, feature
/// levels, GPU address bits, the shader model and the wave and 16-bit
/// options (1 and 4). Formats are answered by the host, which
/// is what makes and samples them, with the console's formats as the
/// host's (`resource::pc_format`). Every other query goes to the host
/// unchanged, which checks its size: our work runs there.
/// shortcut: their console structures are taken to be PC's. Only the
/// options are known to differ, and they are not passed on.
extern "system" fn check_feature_support(
    _: This,
    feature: u32,
    data: *mut c_void,
    size: u32,
) -> HRESULT {
    let expected = match feature {
        OPTIONS_QUERY => size_of_val(&OPTIONS),
        ARCHITECTURE => 16,
        FEATURE_LEVELS => size_of::<FeatureLevels>(),
        FORMAT_SUPPORT => 12,
        MULTISAMPLE => 16,
        FORMAT_INFO => 8,
        GPU_VIRTUAL_ADDRESS => 8,
        SHADER_MODEL => 4,
        OPTIONS1 => 24,
        ARCHITECTURE1 => 20,
        OPTIONS4 => 12,
        _ => return host_feature(feature, data, size),
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
        ARCHITECTURE | ARCHITECTURE1 => {
            // SAFETY: checked size (16 or 20 bytes) and non-null.
            let a = unsafe { std::slice::from_raw_parts_mut(data.cast::<u32>(), expected / 4) };
            if a[0] != 0 {
                return E_INVALIDARG; // one node only
            }
            // Not tile-based; unified, cache-coherent memory.
            a[1..4].copy_from_slice(&[0, 1, 1]);
            if feature == ARCHITECTURE1 {
                // Whether the GPU honours the CPU's page protections: the
                // host GPU's business, which says so itself.
                let mut host = [0u32; 5];
                let hr = host_check(ARCHITECTURE1, host.as_mut_ptr().cast(), 20);
                a[4] = if hr == Some(S_OK) { host[4] } else { 0 };
            }
            S_OK
        }
        GPU_VIRTUAL_ADDRESS => {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 2]>() };
            // GPU addresses are CPU addresses: all of user space.
            let process = u64::BITS - user_space().1.leading_zeros();
            *q = [ADDRESS_BITS_PER_RESOURCE, process];
            S_OK
        }
        FORMAT_SUPPORT => host_format_feature(feature, data, expected).unwrap_or_else(|| {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 3]>() };
            // shortcut: without a host, every use of every format.
            q[1] = 0x7fff_ffff;
            q[2] = 0xffff;
            S_OK
        }),
        MULTISAMPLE => host_format_feature(feature, data, expected).unwrap_or_else(|| {
            // SAFETY: checked size and non-null.
            let q = unsafe { &mut *data.cast::<[u32; 4]>() };
            // Format, sample count, flags in; without a host, one quality
            // level out for the counts the hardware has.
            q[3] = u32::from(matches!(q[1], 1 | 2 | 4 | 8));
            S_OK
        }),
        FORMAT_INFO => host_format_feature(feature, data, expected).unwrap_or_else(|| {
            tracing::warn!("no host device for a format's plane count");
            E_NOTIMPL
        }),
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

/// A format query (format first, `size` bytes) as the host answers it for
/// the format the host has in its place; the title's format stays in the
/// structure. None without a host.
fn host_format_feature(feature: u32, data: *mut c_void, size: usize) -> Option<HRESULT> {
    // SAFETY: the caller checked `size` (8 to 16) and non-null.
    let title = unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), size) };
    let mut query = [0u32; 4];
    for (word, bytes) in query.iter_mut().zip(title.chunks(4)) {
        *word = u32::from_ne_bytes(bytes.try_into().ok()?);
    }
    query[0] = resource::pc_format(query[0]);
    let hr = host_check(feature, query.as_mut_ptr().cast(), size as u32)?;
    // Everything after the format.
    for (bytes, word) in title.chunks_mut(4).zip(query).skip(1) {
        bytes.copy_from_slice(&word.to_ne_bytes());
    }
    Some(hr)
}

/// A query the host answers as it is (it checks the size); `E_NOTIMPL`
/// without a host.
fn host_feature(feature: u32, data: *mut c_void, size: u32) -> HRESULT {
    host_check(feature, data, size).unwrap_or_else(|| {
        tracing::warn!(feature, size, "device feature query without a host device");
        E_NOTIMPL
    })
}

/// The host device's `CheckFeatureSupport`, None without one.
fn host_check(feature: u32, data: *mut c_void, size: u32) -> Option<HRESULT> {
    #[cfg(windows)]
    if let Some(host) = crate::host::host() {
        use windows::Win32::Graphics::Direct3D12::D3D12_FEATURE;
        // SAFETY: the title's structure of `size` bytes, as D3D12 takes it;
        // the host checks the size and the pointer.
        let result = unsafe {
            host.device
                .CheckFeatureSupport(D3D12_FEATURE(feature as i32), data, size)
        };
        let hr = result.map_or_else(|e| e.code().0, |()| S_OK);
        tracing::debug!(
            feature,
            size,
            hr,
            "device feature query answered by the host"
        );
        return Some(hr);
    }
    let _ = (feature, data, size);
    None
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

/// The host device's state: the console's device is removed when the host's is.
extern "system" fn removed_reason(_: This) -> HRESULT {
    #[cfg(windows)]
    if let Some(host) = crate::host::host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            return e.code().0;
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
    f[34] = residency as *const (); // 0x110 make resident
    f[35] = residency as *const (); // 0x118 evict
    f[36] = fence::create as *const (); // 0x120
    f[37] = removed_reason as *const (); // 0x128
    f[38] = resource::copyable_footprints as *const (); // 0x130
    f[39] = query::create as *const (); // 0x138
    f[40] = stable_power_state as *const (); // 0x140
    f[41] = signature::create as *const (); // 0x148
    f[48] = queue::create_x as *const (); // 0x180
    f[61] = descriptor::create_sampler_x as *const (); // 0x1e8
    f[62] = pipeline::create_compute_x as *const (); // 0x1f0
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
    fn newer_device_levels_are_the_same_device() {
        // ID3D12Device11, as Grand Theft Auto VI asks for it.
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

    #[test]
    fn a_healthy_device_reports_no_removal() {
        let device = device();
        let reason = slot::<extern "system" fn(This) -> HRESULT>(device, 0x128);
        assert_eq!(reason(device), S_OK);
    }

    /// The PC device methods between creating a placed resource and a
    /// fence, and the one after creating a query heap, at their PC slots.
    #[test]
    fn residency_and_power_state_succeed_as_on_pc() {
        type Residency = extern "system" fn(This, u32, *const This) -> HRESULT;
        let device = device();
        let mut fence = std::ptr::null();
        let fence_iid = &crate::fence::FENCE_IIDS[0];
        assert_eq!(fence::create(device, 0, 0, fence_iid, &mut fence), S_OK);
        for offset in [0x110, 0x118] {
            let call = slot::<Residency>(device, offset);
            assert_eq!(call(device, 1, &fence), S_OK, "{offset:#x}");
            assert_eq!(call(device, 0, std::ptr::null()), S_OK, "{offset:#x}");
            assert_eq!(call(device, 1, std::ptr::null()), E_INVALIDARG);
        }
        let stable = slot::<extern "system" fn(This, i32) -> HRESULT>(device, 0x140);
        assert_eq!(stable(device, 1), S_OK);
        assert_eq!(stable(device, 0), S_OK);
        slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence);
    }

    type Check = extern "system" fn(This, u32, *mut c_void, u32) -> HRESULT;

    /// `CheckFeatureSupport` through the device's table, on `N` words.
    fn check<const N: usize>(feature: u32, words: &mut [u32; N], size: u32) -> HRESULT {
        let device = device();
        slot::<Check>(device, 0x68)(device, feature, words.as_mut_ptr().cast(), size)
    }

    /// Queries answered here fail on a size that is not their structure's
    /// and on null data, as on PC, and leave the structure alone then.
    #[test]
    fn feature_queries_check_their_size() {
        let sizes = [
            (OPTIONS_QUERY, 64),
            (ARCHITECTURE, 16),
            (FEATURE_LEVELS, 24),
            (FORMAT_SUPPORT, 12),
            (MULTISAMPLE, 16),
            (FORMAT_INFO, 8),
            (GPU_VIRTUAL_ADDRESS, 8),
            (SHADER_MODEL, 4),
            (OPTIONS1, 24),
            (ARCHITECTURE1, 20),
            (OPTIONS4, 12),
        ];
        let device = device();
        let check_raw = slot::<Check>(device, 0x68);
        for (feature, size) in sizes {
            for wrong in [size - 4, size + 4, 0] {
                let mut words = [0x5a5a_5a5au32; 24];
                assert_eq!(check(feature, &mut words, wrong), E_INVALIDARG, "{feature}");
                assert!(words.iter().all(|&w| w == 0x5a5a_5a5a), "{feature}");
            }
            let null = std::ptr::null_mut();
            assert_eq!(check_raw(device, feature, null, size), E_INVALIDARG);
        }
        // PC's options are a word shorter than the console's.
        assert_eq!(check(OPTIONS_QUERY, &mut [0; 16], 60), E_INVALIDARG);
    }

    #[test]
    fn local_feature_queries_answer_the_console() {
        let mut options = [0; 16];
        assert_eq!(check(OPTIONS_QUERY, &mut options, 64), S_OK);
        assert_eq!(options, OPTIONS);
        let mut architecture = [0, 7, 7, 7];
        assert_eq!(check(ARCHITECTURE, &mut architecture, 16), S_OK);
        assert_eq!(architecture, [0, 0, 1, 1]);
        assert_eq!(check(ARCHITECTURE, &mut [1, 0, 0, 0], 16), E_INVALIDARG);
        let mut architecture1 = [0, 7, 7, 7, 7];
        assert_eq!(check(ARCHITECTURE1, &mut architecture1, 20), S_OK);
        assert_eq!(architecture1[..4], [0, 0, 1, 1]);
        assert!(architecture1[4] <= 1);
        assert_eq!(check(ARCHITECTURE1, &mut [1, 0, 0, 0, 0], 20), E_INVALIDARG);

        let mut address = [0; 2];
        assert_eq!(check(GPU_VIRTUAL_ADDRESS, &mut address, 8), S_OK);
        assert_eq!(address[0], OPTIONS[9]);
        let (_, highest) = user_space();
        // The process's bits cover user space, and no more.
        assert!(highest < 1 << address[1] && highest >= 1 << (address[1] - 1));
        #[cfg(all(windows, target_pointer_width = "64"))]
        assert_eq!(address[1], 47);
    }

    /// The host answers the rest as it is; ours must match its own answer.
    #[cfg(windows)]
    #[test]
    fn other_feature_queries_are_the_hosts() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_FEATURE_D3D12_OPTIONS3, D3D12_FEATURE_D3D12_OPTIONS5,
            D3D12_FEATURE_DATA_D3D12_OPTIONS3, D3D12_FEATURE_DATA_D3D12_OPTIONS5,
            D3D12_FEATURE_DATA_ROOT_SIGNATURE, D3D12_FEATURE_ROOT_SIGNATURE,
        };
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let device = device();
        let check_raw = slot::<Check>(device, 0x68);
        let ask = |feature: u32, data: *mut c_void, size: usize| {
            check_raw(device, feature, data, size as u32)
        };

        // Options the console does not decide: the host's own answers.
        let mut ours = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
        let mut theirs = ours;
        let feature = D3D12_FEATURE_D3D12_OPTIONS3.0 as u32;
        assert_eq!(
            ask(feature, (&raw mut ours).cast(), size_of_val(&ours)),
            S_OK
        );
        // SAFETY: a plain query.
        unsafe {
            host.device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS3,
                    (&raw mut theirs).cast(),
                    size_of_val(&theirs) as u32,
                )
                .unwrap()
        };
        assert_eq!(ours, theirs);
        assert_eq!(ask(feature, (&raw mut ours).cast(), 4), E_INVALIDARG);

        let mut ours = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
        let mut theirs = ours;
        let feature = D3D12_FEATURE_D3D12_OPTIONS5.0 as u32;
        assert_eq!(
            ask(feature, (&raw mut ours).cast(), size_of_val(&ours)),
            S_OK
        );
        // SAFETY: a plain query.
        unsafe {
            host.device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS5,
                    (&raw mut theirs).cast(),
                    size_of_val(&theirs) as u32,
                )
                .unwrap()
        };
        assert_eq!(ours, theirs);
        assert_eq!(ask(feature, (&raw mut ours).cast(), 4), E_INVALIDARG);

        // Root signature 1.1 asked: 1.1, which our serializer (the host's)
        // writes.
        let mut root = D3D12_FEATURE_DATA_ROOT_SIGNATURE::default();
        root.HighestVersion.0 = 2;
        let feature = D3D12_FEATURE_ROOT_SIGNATURE.0 as u32;
        assert_eq!(
            ask(feature, (&raw mut root).cast(), size_of_val(&root)),
            S_OK
        );
        assert_eq!(root.HighestVersion.0, 2);

        // No such query.
        assert!(ask(0xffff, (&raw mut root).cast(), 4) < 0);
    }

    /// Formats: the host's answer, the console's own depth formats answered
    /// as the PC formats standing in for them.
    #[cfg(windows)]
    #[test]
    fn format_queries_are_the_hosts() {
        if crate::host::host().is_none() {
            return; // no host GPU here
        }
        const RGBA8: u32 = 28;
        const D24S8: u32 = 45;
        const CONSOLE_D16S8: u32 = 118;
        const TEXTURE2D: u32 = 0x20;
        const RENDER_TARGET: u32 = 0x4000;
        const DEPTH_STENCIL: u32 = 0x10000;

        let mut rgba = [RGBA8, 0, 0];
        assert_eq!(check(FORMAT_SUPPORT, &mut rgba, 12), S_OK);
        assert_eq!(rgba[0], RGBA8);
        assert_ne!(rgba[1] & (TEXTURE2D | RENDER_TARGET), 0);
        assert_eq!(rgba[1] & DEPTH_STENCIL, 0, "{:#x}", rgba[1]);
        let mut depth = [CONSOLE_D16S8, 0, 0];
        assert_eq!(check(FORMAT_SUPPORT, &mut depth, 12), S_OK);
        assert_eq!(depth[0], CONSOLE_D16S8);
        assert_ne!(depth[1] & DEPTH_STENCIL, 0);
        let mut pc = [D24S8, 0, 0];
        assert_eq!(check(FORMAT_SUPPORT, &mut pc, 12), S_OK);
        assert_eq!(depth[1..], pc[1..]);

        // Format, count, flags in; quality levels out.
        let mut four = [RGBA8, 4, 0, 0];
        assert_eq!(check(MULTISAMPLE, &mut four, 16), S_OK);
        assert!(four[3] >= 1);
        let mut three = [RGBA8, 3, 0, 7];
        assert_eq!(check(MULTISAMPLE, &mut three, 16), S_OK);
        assert_eq!(three[3], 0);

        // Format in, plane count (a byte) out.
        let mut info = [RGBA8, 0];
        assert_eq!(check(FORMAT_INFO, &mut info, 8), S_OK);
        assert_eq!(info[1] & 0xff, 1);
        let mut info = [CONSOLE_D16S8, 0];
        assert_eq!(check(FORMAT_INFO, &mut info, 8), S_OK);
        assert_eq!((info[0], info[1] & 0xff), (CONSOLE_D16S8, 2));
    }
}
