//! Title buffers in place: where the host GPU shares memory with the CPU,
//! the host buffer for a title buffer is the title's memory itself, opened
//! as a GPU heap, instead of a copy kept current at every submission.
//!
//! On such a host every placement reads at the same speed (`crate::unified`),
//! so a copy only costs its refreshes, snapshots and copy-backs. A buffer in
//! place has none of those: the GPU reads what the title wrote and the title
//! reads what the GPU wrote, both in the same pages.
//!
//! Each buffer gets a heap over its own pages only
//! (`OpenExistingHeapFromAddress1`, 4 KiB granular), with one buffer placed
//! at its start. While that heap lives, Windows refuses to free, unmap or
//! protect those pages (error 87), so every heap is registered with
//! `crate::pinned`: the title's frees, protections and remaps first drop the
//! buffer from the copies' map ([`crate::shadow::let_go_in_place`]), and the
//! change goes through once the lists still holding it are reset.
//!
//! What a program using title memory at computed addresses writes may be
//! withheld from title memory (`crate::memory::withholding`): a buffer such
//! a program writes gets a copy from then on, and the one submission that
//! wrote it in place is undone if withheld (`Shadow::withholdable`).
//!
//! `VICTORY_IN_PLACE`: unset, on where the host reports unified memory and
//! title memory passes `crate::unified::self_test` (off in tests, which
//! check the copies); `1` on wherever the host can open title memory as a
//! heap; `0` off.

use std::ffi::OsStr;
use std::sync::OnceLock;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU32};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Whether title buffers go in place, decided once (see the module docs).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let Some(host) = crate::host::host() else {
            return false;
        };
        let var = std::env::var_os("VICTORY_IN_PLACE");
        // Tests check the copies on every host unless they ask.
        let uma = host.memory.uma && !cfg!(test);
        let on = decide(var.as_deref(), uma, || {
            crate::unified::self_test(&host.device)
        });
        match (&on, var.is_some()) {
            (Ok(true), false) => tracing::info!(
                "title buffers in place: the host GPU shares memory with the CPU, and title memory \
                 passed the self-test"
            ),
            (Ok(on), true) => tracing::info!(on, "title buffers in place (VICTORY_IN_PLACE)"),
            (Ok(false), false) => {}
            (Err(why), _) => tracing::warn!(
                %why,
                "title buffers copied: the host GPU shares memory with the CPU, but title memory \
                 failed the in-place self-test"
            ),
        }
        on.unwrap_or(false)
    })
}

/// [`enabled`] from the switch's value, whether the host GPU shares memory
/// with the CPU, and the self-test (run only when it decides).
fn decide(
    var: Option<&OsStr>,
    uma: bool,
    self_test: impl FnOnce() -> Result<(), String>,
) -> Result<bool, String> {
    match var {
        Some(v) => Ok(v == "1"),
        None if uma => self_test().map(|()| true),
        None => Ok(false),
    }
}

/// The smallest title buffer that goes in place (`VICTORY_IN_PLACE_MIN_KB`,
/// 64 by default): opening a heap costs ~0.3 ms per MiB on an AMD APU, and
/// a heap per tiny buffer would cost a kernel call each.
pub(crate) fn min_len() -> u64 {
    static MIN: OnceLock<u64> = OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("VICTORY_IN_PLACE_MIN_KB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(64)
            << 10
    })
}

/// A host buffer that is title memory itself.
pub(crate) struct InPlace {
    /// Placed at the start of a heap opened over the title's pages from
    /// the 4 KiB page holding the buffer's first byte.
    pub(crate) buffer: ID3D12Resource,
    /// Where the title buffer starts in `buffer`, and its GPU address.
    pub(crate) offset: u64,
    pub(crate) gpu: u64,
}

/// Title memory `[start, start + len)` as a host buffer, or `None` where the
/// host cannot open it as a heap (no `ID3D12Device13`, memory that is not
/// committed read-write memory of one allocation) or place a buffer in it.
pub(crate) fn open(start: u64, len: u64) -> Option<InPlace> {
    if !OPENS.worth_asking() {
        return None;
    }
    let host = crate::host::host()?;
    let Ok(device13) = host.device.cast::<ID3D12Device13>() else {
        OPENS.cannot();
        return None;
    };
    let from = start & !0xFFF;
    let offset = start - from;
    // Whole pages: a view rounded up to 256 bytes at the buffer's end fits.
    let size = (offset + len).next_multiple_of(4096);
    // SAFETY: title memory; the heap only holds its pages, and a range that
    // is not committed read-write memory is refused.
    let opened = unsafe { device13.OpenExistingHeapFromAddress1(from as *const _, size as usize) };
    let heap: ID3D12Heap = match opened {
        Ok(heap) => heap,
        Err(e) if e.code() == windows::Win32::Foundation::E_NOTIMPL => {
            OPENS.cannot();
            return None;
        }
        Err(_) => {
            OPENS.refused();
            return None;
        }
    };
    OPENS.opened();
    pin_heap(&heap, from, size);
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: a buffer the size of the heap, at its start.
    unsafe {
        host.device.CreatePlacedResource(
            &heap,
            0,
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        )
    }
    .ok()?;
    let buffer = buffer?;
    // The buffer holds its heap, and so the heap's pin: native D3D12 keeps
    // a heap for its placed resources, but vkd3d-proton destroys it, for its
    // destruction callbacks, once its own references go.
    // SAFETY: live objects; the buffer's private data holds a reference to
    // the heap until the buffer goes.
    unsafe { buffer.SetPrivateDataInterface(&HOLDS_ITS_HEAP, &heap) }.ok()?;
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + offset;
    Some(InPlace {
        buffer,
        offset,
        gpu,
    })
}

/// Whether opening title memory as a heap is worth asking again. One range
/// can be refused (a read-only page, several allocations); a host that never
/// opens any (a translation layer without the method, or one that fails it
/// every time: D3DMetal returns `E_FAIL`) is not asked again, so each new
/// buffer does not pay a failing call before its copy.
struct Opens {
    /// Refusals in a row, while none succeeded; [`GIVE_UP`] when the host
    /// cannot open heaps at all.
    refused: AtomicU32,
    opened: AtomicBool,
}

/// Refusals in a row, none ever opened, after which the host is not asked.
const GIVE_UP: u32 = 16;

static OPENS: Opens = Opens {
    refused: AtomicU32::new(0),
    opened: AtomicBool::new(false),
};

impl Opens {
    fn worth_asking(&self) -> bool {
        self.opened.load(Relaxed) || self.refused.load(Relaxed) < GIVE_UP
    }

    fn opened(&self) {
        self.opened.store(true, Relaxed);
    }

    /// One range refused.
    fn refused(&self) {
        if self.refused.fetch_add(1, Relaxed) + 1 == GIVE_UP && !self.opened.load(Relaxed) {
            tracing::info!("title memory never opens as a heap here: title buffers keep copies");
        }
    }

    /// The host has no way to open title memory as a heap.
    fn cannot(&self) {
        if self.refused.swap(GIVE_UP, Relaxed) < GIVE_UP {
            tracing::info!(
                "this host cannot open title memory as a heap: title buffers keep copies"
            );
        }
    }
}

/// The private data by which a buffer in place holds its heap.
const HOLDS_ITS_HEAP: windows::core::GUID =
    windows::core::GUID::from_u128(0x6f1d_2c3a_9b84_4e57_a0c2_51e8_7d93_b41f);

/// Registers title memory `[at, at + len)`, which `heap` holds, with
/// `crate::pinned`: a free, protection or remap of the title's there first
/// drops what is placed in it ([`crate::shadow::let_go_in_place`]). The
/// registration goes with the heap.
fn pin_heap(heap: &ID3D12Heap, at: u64, len: u64) {
    use windows::Win32::Graphics::Direct3D::ID3DDestructionNotifier;
    extern "system" fn forget(data: *mut std::ffi::c_void) {
        // SAFETY: the box registered below, given back once, when the heap
        // is destroyed.
        unsafe { Box::from_raw(data.cast::<crate::pinned::Pin>()) }.forget();
    }
    let release = move || crate::shadow::let_go_in_place(at, len);
    let pin = crate::pinned::pin(at as usize, len as usize, Box::new(release));
    let Ok(notifier) = heap.cast::<ID3DDestructionNotifier>() else {
        // Registered until the title changes the memory.
        return;
    };
    let data = Box::into_raw(Box::new(pin));
    // SAFETY: `forget` takes the box back when the heap goes.
    if unsafe { notifier.RegisterDestructionCallback(Some(forget), data.cast()) }.is_err() {
        // SAFETY: not registered: still ours. Dropped, it stays registered
        // until the title changes the memory.
        drop(unsafe { Box::from_raw(data) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Opens {
        Opens {
            refused: AtomicU32::new(0),
            opened: AtomicBool::new(false),
        }
    }

    /// A host that refuses every open is not asked after [`GIVE_UP`]
    /// refusals, one without the method at once; one that opened any range
    /// is always asked (other ranges may be refused for their own reasons).
    #[test]
    fn a_host_that_never_opens_heaps_is_not_asked_again() {
        let never = fresh();
        for _ in 0..GIVE_UP - 1 {
            never.refused();
        }
        assert!(never.worth_asking());
        never.refused();
        assert!(!never.worth_asking());
        let without = fresh();
        without.cannot();
        assert!(!without.worth_asking());
        let some = fresh();
        some.opened();
        for _ in 0..2 * GIVE_UP {
            some.refused();
        }
        assert!(some.worth_asking());
    }

    #[test]
    fn the_switch_decides_and_unset_follows_the_host() {
        let passes = || Ok(());
        let fails = || Err("not shared".to_string());
        let never = || -> Result<(), String> { panic!("self-test run") };
        assert_eq!(decide(Some(OsStr::new("1")), false, never), Ok(true));
        assert_eq!(decide(Some(OsStr::new("0")), true, never), Ok(false));
        assert_eq!(decide(None, false, never), Ok(false));
        assert_eq!(decide(None, true, passes), Ok(true));
        assert_eq!(decide(None, true, fails), Err("not shared".to_string()));
    }
}
