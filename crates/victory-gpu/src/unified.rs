//! What the host GPU's memory is like (decision 0005's unified-memory item,
//! #880): unified with the CPU's (an APU, an integrated GPU, Apple silicon
//! under a translation layer) or its own, and whether the CPU maps part of
//! it.
//!
//! The console's GPU reads title memory where it is. Here host copies of it
//! are kept current at each submission (`crate::shadow`), which on a unified
//! host buys nothing: there the GPU reaches every placement at the same
//! speed, so the copies cost their CPU time and give no GPU time back. On a
//! discrete GPU video memory is much faster than system memory, and memory
//! the CPU maps there (GPU upload heaps, through Resizable BAR) changes how
//! copies in it are filled.
//!
//! [`detect`] asks the device: two feature queries and the memory budgets
//! (about 0.3 ms), logged once at host creation and named in the GPU
//! failure text.
//!
//! [`self_test`] checks, on a unified host, that title memory really opens
//! as a GPU heap: the GPU reads bytes the CPU wrote there and the CPU reads
//! bytes the GPU wrote. A layer can have the method and still not share
//! the memory (vkd3d-proton without `VK_EXT_external_memory_host` makes new
//! memory), so the answer is what the GPU did. Title memory goes in place
//! by default only where it passes (`crate::in_place`).
//!
//! What opening title memory as a heap implies, as measured on Windows 11
//! (NVIDIA, AMD integrated, WARP alike): while the heap lives the memory
//! cannot be protected, unmapped or decommitted (error 87, `crate::pinned`);
//! a read-only page in the range makes the open fail; write watching still
//! sees CPU writes but not the GPU's; opening costs about 0.2-0.75 ms per
//! MiB and locks the pages.

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_MEMORY_SEGMENT_GROUP,
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
    DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIFactory4,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;
use windows_sys::Win32::System::Memory as mem;

/// What the host GPU's memory is like.
#[derive(Clone, Debug)]
pub(crate) struct Memory {
    /// The device says it shares memory with the CPU
    /// (`D3D12_FEATURE_DATA_ARCHITECTURE1::UMA`).
    pub(crate) uma: bool,
    /// And that the CPU's caches are coherent with it for every kind of
    /// memory (`CacheCoherentUMA`). AMD APUs say not, yet write-back system
    /// memory is coherent both ways there too.
    pub(crate) cache_coherent: bool,
    /// Video memory the CPU maps (`D3D12_HEAP_TYPE_GPU_UPLOAD`: Resizable
    /// BAR on a discrete GPU).
    pub(crate) gpu_upload_heaps: bool,
    /// The memory manager's budgets for this process, in MiB: local (video
    /// memory; all of it on a unified host) and non-local (system memory
    /// the GPU reaches; none on a unified host).
    pub(crate) budgets: Option<(u64, u64)>,
}

impl std::fmt::Display for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let kind = match (self.uma, self.gpu_upload_heaps) {
            (true, _) => "unified with the CPU's",
            (false, true) => "its own, the CPU maps part of it (Resizable BAR)",
            (false, false) => "its own",
        };
        write!(f, "{kind}")?;
        if self.uma {
            write!(f, ", cache-coherent {}", self.cache_coherent)?;
        }
        if let Some((local, non_local)) = self.budgets {
            write!(f, "; budgets {local} MiB local, {non_local} MiB non-local")?;
        }
        Ok(())
    }
}

/// What `device` reports about its memory.
pub(crate) fn detect(device: &ID3D12Device) -> Memory {
    let mut architecture = D3D12_FEATURE_DATA_ARCHITECTURE1::default();
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
    // SAFETY: each query writes its own structure, of the given size.
    let (known, upload) = unsafe {
        let known = device
            .CheckFeatureSupport(
                D3D12_FEATURE_ARCHITECTURE1,
                (&raw mut architecture).cast(),
                size_of_val(&architecture) as u32,
            )
            .is_ok();
        let upload = device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS16,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .is_ok()
            && options.GPUUploadHeapSupported.as_bool();
        (known, upload)
    };
    Memory {
        uma: known && architecture.UMA.as_bool(),
        cache_coherent: known && architecture.CacheCoherentUMA.as_bool(),
        gpu_upload_heaps: upload,
        budgets: budgets(device),
    }
}

/// The local and non-local budgets of `device`'s adapter, in MiB.
fn budgets(device: &ID3D12Device) -> Option<(u64, u64)> {
    // SAFETY: plain device and DXGI queries with valid out pointers.
    unsafe {
        let luid = device.GetAdapterLuid();
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter3 = factory.EnumAdapterByLuid(luid).ok()?;
        let budget = |group: DXGI_MEMORY_SEGMENT_GROUP| {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            adapter.QueryVideoMemoryInfo(0, group, &mut info).ok()?;
            Some(info.Budget >> 20)
        };
        Some((
            budget(DXGI_MEMORY_SEGMENT_GROUP_LOCAL)?,
            budget(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL)?,
        ))
    }
}

/// Bytes of title memory the self-test opens: one 64 KiB block, the GPU
/// reading the first half and writing the second.
const BYTES: usize = 1 << 16;
const HALF: usize = BYTES / 2;

/// The self-test's patterns: what the CPU writes in title memory and what
/// the GPU copies into its second half.
fn pattern(i: usize, salt: u32) -> u32 {
    (i as u32).wrapping_mul(0x9E37_79B1) ^ salt
}
const CPU_SALT: u32 = 0x5EED_0001;
const GPU_SALT: u32 = 0x5EED_0002;

/// Opens a block of memory allocated as the title's pools are (private,
/// write-watched) as a GPU heap with `OpenExistingHeapFromAddress1`, then
/// on a copy queue of its own copies its first half out and other bytes into
/// its second half, and checks that each processor saw the other's writes.
/// Asked once, on a unified host only (`crate::in_place`'s default):
/// 3-20 ms (WARP, an AMD APU, an RTX 5070 Ti); nothing is left behind.
pub(crate) fn self_test(device: &ID3D12Device) -> Result<(), String> {
    let device13: ID3D12Device13 = device
        .cast()
        .map_err(|_| "no OpenExistingHeapFromAddress1 (ID3D12Device13)".to_string())?;
    let title = Block::new()?;
    // SAFETY: our committed block, written before the GPU sees it.
    let words = unsafe { std::slice::from_raw_parts_mut(title.0.cast::<u32>(), BYTES / 4) };
    for (i, w) in words.iter_mut().enumerate() {
        *w = pattern(i, CPU_SALT);
    }
    // SAFETY: the block outlives the heap and every resource in it (they
    // are released below, before `title` is).
    let heap: ID3D12Heap = unsafe { device13.OpenExistingHeapFromAddress1(title.0.cast(), BYTES) }
        .map_err(|e| format!("open: {e}"))?;
    let opened = placed(device, &heap).map_err(|e| format!("buffer in the opened heap: {e}"))?;
    let (other, other_data) = system_buffer(device).map_err(|e| format!("system buffer: {e}"))?;
    // SAFETY: a mapped buffer of BYTES bytes, the GPU idle on it.
    let other_words = unsafe { std::slice::from_raw_parts_mut(other_data, BYTES / 4) };
    for (i, w) in other_words.iter_mut().enumerate().skip(HALF / 4) {
        *w = pattern(i, GPU_SALT);
    }
    copy(device, &opened, &other).map_err(|e| format!("copy: {e}"))?;
    drop(opened);
    drop(heap);
    shared(&other_words[..HALF / 4], &words[HALF / 4..])
}

/// Whether the GPU read the CPU's bytes (`read`: the first half of title
/// memory as the GPU copied it out) and the CPU sees the GPU's (`written`:
/// the second half of title memory after the GPU wrote it).
fn shared(read: &[u32], written: &[u32]) -> Result<(), String> {
    if read
        .iter()
        .enumerate()
        .any(|(i, &w)| w != pattern(i, CPU_SALT))
    {
        return Err(
            "the GPU did not read the bytes the CPU wrote: the heap is other memory".into(),
        );
    }
    let first = HALF / 4;
    if written
        .iter()
        .enumerate()
        .any(|(i, &w)| w != pattern(first + i, GPU_SALT))
    {
        return Err("the bytes the GPU wrote did not reach the memory the CPU reads".into());
    }
    Ok(())
}

/// A block of memory as the runtime allocates the title's pools.
struct Block(*mut std::ffi::c_void);

impl Block {
    fn new() -> Result<Block, String> {
        // The title's pools are write-watched (`victory_hle` xmem).
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        // SAFETY: plain allocation.
        let at = unsafe {
            mem::VirtualAlloc(
                std::ptr::null(),
                BYTES,
                mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH,
                mem::PAGE_READWRITE,
            )
        };
        if at.is_null() {
            return Err(format!("allocation: {}", std::io::Error::last_os_error()));
        }
        Ok(Block(at))
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        // SAFETY: our allocation, no heap on it any more.
        unsafe { mem::VirtualFree(self.0, 0, mem::MEM_RELEASE) };
    }
}

fn buffer_desc(flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
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
        Flags: flags,
        ..Default::default()
    }
}

/// A buffer over all of an opened heap.
fn placed(device: &ID3D12Device, heap: &ID3D12Heap) -> windows::core::Result<ID3D12Resource> {
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: plain creation in a live heap of BYTES bytes.
    unsafe {
        device.CreatePlacedResource(
            heap,
            0,
            &buffer_desc(D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut out,
        )?;
    }
    out.ok_or_else(windows::core::Error::empty)
}

/// A mapped buffer in system memory, as `Host::shared_buffer`'s.
fn system_buffer(device: &ID3D12Device) -> windows::core::Result<(ID3D12Resource, *mut u32)> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_CUSTOM,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
        MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
        ..Default::default()
    };
    let mut out: Option<ID3D12Resource> = None;
    let mut data = std::ptr::null_mut();
    // SAFETY: plain creation and mapping of a CPU-visible buffer.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(D3D12_RESOURCE_FLAG_NONE),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut out,
        )?;
        let out = out.ok_or_else(windows::core::Error::empty)?;
        out.Map(0, None, Some(&mut data))?;
        Ok((out, data.cast()))
    }
}

/// On a copy queue of its own: the first half of `opened` into `other`, the
/// second half of `other` into `opened`; waits up to five seconds.
fn copy(
    device: &ID3D12Device,
    opened: &ID3D12Resource,
    other: &ID3D12Resource,
) -> windows::core::Result<()> {
    // SAFETY: plain creation and recording with live objects; the event is
    // ours and closed on every path.
    unsafe {
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            ..Default::default()
        })?;
        let allocator: ID3D12CommandAllocator =
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
        list.CopyBufferRegion(other, 0, opened, 0, HALF as u64);
        list.CopyBufferRegion(opened, HALF as u64, other, HALF as u64, HALF as u64);
        list.Close()?;
        queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>()?)]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        queue.Signal(&fence, 1)?;
        let event = CreateEventW(None, false, false, None)?;
        let waited = fence.SetEventOnCompletion(1, event).and_then(|()| {
            if WaitForSingleObject(event, 5000) == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    "the copies did not finish in 5 s",
                ))
            }
        });
        let _ = CloseHandle(event);
        waited
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_names_what_was_not_shared() {
        let read: Vec<u32> = (0..HALF / 4).map(|i| pattern(i, CPU_SALT)).collect();
        let written: Vec<u32> = (HALF / 4..BYTES / 4)
            .map(|i| pattern(i, GPU_SALT))
            .collect();
        assert_eq!(shared(&read, &written), Ok(()));
        // New memory instead of the title's: the GPU read zeros.
        let zeros = vec![0; HALF / 4];
        assert!(
            shared(&zeros, &written)
                .unwrap_err()
                .contains("other memory")
        );
        // The GPU's writes went elsewhere: title memory still holds the CPU's.
        let stale: Vec<u32> = (HALF / 4..BYTES / 4)
            .map(|i| pattern(i, CPU_SALT))
            .collect();
        assert!(shared(&read, &stale).unwrap_err().contains("did not reach"));
    }

    /// Title memory opens in place on WARP (a unified host), where the
    /// runtime has the method.
    #[test]
    fn title_memory_opens_in_place_on_warp() {
        // SAFETY: plain DXGI and device creation.
        let device = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            let Ok(adapter) =
                factory.EnumWarpAdapter::<windows::Win32::Graphics::Dxgi::IDXGIAdapter>()
            else {
                return;
            };
            let mut device: Option<ID3D12Device> = None;
            let level = windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
            if D3D12CreateDevice(&adapter, level, &mut device).is_err() {
                return;
            }
            device.unwrap()
        };
        if device.cast::<ID3D12Device13>().is_err() {
            return;
        }
        assert_eq!(self_test(&device), Ok(()));
    }

    /// WARP is a unified host.
    #[test]
    fn warp_is_unified() {
        // SAFETY: plain DXGI and device creation.
        let device = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            let Ok(adapter) =
                factory.EnumWarpAdapter::<windows::Win32::Graphics::Dxgi::IDXGIAdapter>()
            else {
                return;
            };
            let mut device: Option<ID3D12Device> = None;
            let level = windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
            if D3D12CreateDevice(&adapter, level, &mut device).is_err() {
                return;
            }
            device.unwrap()
        };
        let memory = detect(&device);
        assert!(memory.uma, "{memory}");
        assert!(
            memory.to_string().starts_with("unified with the CPU's"),
            "{memory}"
        );
    }

    /// The host device's answer (whatever it is), as the host logs it.
    #[test]
    fn the_host_device_is_described() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let memory = &host.memory;
        assert!(memory.uma || !memory.cache_coherent, "{memory}");
        assert!(!memory.to_string().is_empty());
    }
}
