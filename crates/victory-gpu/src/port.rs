use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Device13, ID3D12Heap};
use windows_sys::Win32::System::Memory as mem;
use windows_sys::Win32::System::SystemInformation as sysinfo;

const K64: u64 = 64 << 10;
const LOW: u64 = 1 << 32;
const MOST: u64 = 256 << 20;
const TOTAL: u64 = 512 << 20;

#[cfg(test)]
std::thread_local! {
    static PORT_CONSTANTS_IN_TEST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

fn low_for(ram: Option<u64>, vram_mb: Option<f64>) -> bool {
    ram.is_none_or(|bytes| bytes <= 16 << 30) || vram_mb.is_none_or(|mb| mb <= 8.0 * 1024.0)
}

fn low_host() -> bool {
    #[cfg(windows)]
    {
        // SAFETY: `status` is a POD structure; `dwLength` is set before use.
        let mut status: sysinfo::MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = size_of_val(&status) as u32;
        // SAFETY: `status` is initialized with the length the API requires.
        let ram = (unsafe { sysinfo::GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.ullTotalPhys);
        let vram = crate::host::video_memory().map(|memory| memory[0].1);
        low_for(ram, vram)
    }
    #[cfg(not(windows))]
    {
        low_for(None, None)
    }
}

pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = PORT_CONSTANTS_IN_TEST.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("VICTORY_PORT_CONSTANTS").is_ok_and(|v| v.trim() == "1") && !low_host()
    })
}

pub(crate) fn wanted(at: u64, len: u64) -> bool {
    on() && len > 0 && at.checked_add(len).is_some_and(|end| end <= LOW)
}

struct Region {
    start: u64,
    end: u64,
    heap: ID3D12Heap,
}

// SAFETY: the heap is only touched while `REGIONS` is locked.
unsafe impl Send for Region {}

static REGIONS: Mutex<Vec<Region>> = Mutex::new(Vec::new());
static OPENED: AtomicU64 = AtomicU64::new(0);

struct Views {
    sections: BTreeMap<usize, (u64, bool)>,
    views: BTreeMap<usize, (usize, usize, u64)>,
}

static VIEWS: RwLock<Views> = RwLock::new(Views {
    sections: BTreeMap::new(),
    views: BTreeMap::new(),
});

pub fn title_memory_section(section: usize, bytes: u64, committed: bool) {
    if !on() {
        return;
    }
    let mut views = VIEWS.write().unwrap_or_else(|e| e.into_inner());
    views.sections.insert(section, (bytes, committed));
}

pub fn title_memory_section_gone(section: usize) {
    if !on() {
        return;
    }
    let mut views = VIEWS.write().unwrap_or_else(|e| e.into_inner());
    views.sections.remove(&section);
    views.views.retain(|_, (_, s, _)| *s != section);
}

pub fn title_memory_view(at: usize, len: usize, section: usize, offset: u64) {
    if !on() {
        return;
    }
    {
        let mut views = VIEWS.write().unwrap_or_else(|e| e.into_inner());
        views.views.insert(at, (len, section, offset));
    }
    if wanted(at as u64, len as u64)
        && let Some(host) = crate::host::host()
    {
        let _ = open_region(&host.device, at as u64, len as u64);
    }
}

pub fn title_memory_freeing(at: usize, len: usize) {
    if !on() {
        return;
    }
    {
        let mut views = VIEWS.write().unwrap_or_else(|e| e.into_inner());
        let end = at.saturating_add(len);
        let bases: Vec<usize> = views
            .views
            .iter()
            .filter(|&(&base, &(bytes, _, _))| base < end && base.saturating_add(bytes) > at)
            .map(|(&base, _)| base)
            .collect();
        for base in bases {
            views.views.remove(&base);
        }
    }
    forget(at, len);
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Cover {
    None,
    Holes,
    Scattered,
    Run(usize, u64),
}

fn cover_in(views: &BTreeMap<usize, (usize, usize, u64)>, at: usize, len: usize) -> Cover {
    let end = at + len.max(1);
    let Some((&base, &(bytes, section, offset))) = views.range(..=at).next_back().filter(|(base, view)| *base + view.0 > at) else {
        return Cover::None;
    };
    let first = (section, offset + (at - base) as u64);
    let (mut reached, mut one) = (base + bytes, true);
    while reached < end {
        let Some(&(bytes, s, o)) = views.get(&reached) else {
            return Cover::Holes;
        };
        one &= s == first.0 && o == first.1 + (reached - at) as u64;
        reached += bytes;
    }
    if one { Cover::Run(first.0, first.1) } else { Cover::Scattered }
}

fn stretch(at: usize, len: usize) -> Option<(usize, usize, usize, u64)> {
    let views = VIEWS.read().unwrap_or_else(|e| e.into_inner());
    let Cover::Run(section, offset) = cover_in(&views.views, at, len) else {
        return None;
    };
    if !views.sections.get(&section).is_some_and(|&(_, committed)| committed) {
        return None;
    }
    let delta = at as i128 - offset as i128;
    let follows = |a: usize, s: usize, o: u64| s == section && a as i128 - o as i128 == delta;
    let (&first, &(n, _, _)) = views.views.range(..=at).next_back()?;
    let mut start = first;
    for (&a, &(n, s, o)) in views.views.range(..first).rev() {
        if a + n != start || !follows(a, s, o) {
            break;
        }
        start = a;
    }
    let mut stop = first + n;
    for (&a, &(n, s, o)) in views.views.range(first + 1..) {
        if a != stop || !follows(a, s, o) {
            break;
        }
        stop = a + n;
    }
    Some((start, stop - start, section, (start as i128 - delta) as u64))
}

fn committed(at: usize, len: usize) -> bool {
    let mut here = at;
    while here < at + len {
        // SAFETY: `info` is a POD structure filled by the call below.
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid out parameter of the required size.
        if unsafe { mem::VirtualQuery(here as *const _, &mut info, size_of_val(&info)) } == 0
            || info.State != mem::MEM_COMMIT
        {
            return false;
        }
        here = (info.BaseAddress as usize).saturating_add(info.RegionSize);
    }
    true
}

fn open_region(device: &ID3D12Device, start: u64, len: u64) -> Option<ID3D12Heap> {
    let at = start & !(K64 - 1);
    let end = (start + len.max(1)).next_multiple_of(K64);
    let (first, bytes, section, mut offset) = stretch(at as usize, (end - at) as usize)?;
    let (mut first, mut bytes) = (first as u64, bytes as u64);
    if first < LOW && first + bytes > LOW {
        bytes = LOW - first;
    }
    if bytes > MOST {
        offset += at - first;
        (first, bytes) = (at, end - at);
    }
    if bytes > MOST || first > at || first + bytes < end || offset % K64 != 0 {
        return None;
    }
    loop {
        let opened = OPENED.load(Ordering::Relaxed);
        if opened + bytes > TOTAL {
            return None;
        }
        if OPENED
            .compare_exchange(opened, opened + bytes, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
    // SAFETY: `section` is a live mapping and the range was bounded above.
    let view = unsafe {
        mem::MapViewOfFile(
            section as _,
            mem::FILE_MAP_WRITE,
            (offset >> 32) as u32,
            offset as u32,
            bytes as usize,
        )
    }
    .Value as usize;
    if view == 0 || !committed(view, bytes as usize) {
        OPENED.fetch_sub(bytes, Ordering::Relaxed);
        if view != 0 {
            // SAFETY: `view` is the mapping just created above.
            unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
        }
        return None;
    }
    let Some(device) = device.cast::<ID3D12Device13>().ok() else {
        OPENED.fetch_sub(bytes, Ordering::Relaxed);
        // SAFETY: `view` is the mapping just created above.
        unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
        return None;
    };
    // SAFETY: `view` is our own committed view, `bytes` its length.
    let heap = unsafe { device.OpenExistingHeapFromAddress1::<ID3D12Heap>(view as *const _, bytes as usize) }.ok();
    if heap.is_none() {
        OPENED.fetch_sub(bytes, Ordering::Relaxed);
        // SAFETY: `view` is the mapping just created above.
        unsafe { mem::UnmapViewOfFile(mem::MEMORY_MAPPED_VIEW_ADDRESS { Value: view as *mut _ }) };
    }
    let heap = heap?;
    let mut regions = REGIONS.lock().unwrap_or_else(|e| e.into_inner());
    regions.push(Region { start: first, end: first + bytes, heap: heap.clone() });
    Some(heap)
}

fn forget(at: usize, len: usize) {
    let (at, end) = (at as u64, (at as u64).saturating_add(len as u64));
    let mut regions = REGIONS.lock().unwrap_or_else(|e| e.into_inner());
    regions.retain(|r| {
        let _ = &r.heap;
        if r.end <= at || r.start >= end {
            true
        } else {
            OPENED.fetch_sub(r.end - r.start, Ordering::Relaxed);
            false
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_disable_low_hosts() {
        assert!(low_for(Some(8 << 30), Some(16.0 * 1024.0)));
        assert!(low_for(Some(64 << 30), Some(8.0 * 1024.0)));
        assert!(low_for(None, Some(16.0 * 1024.0)));
        assert!(!low_for(Some(64 << 30), Some(16.0 * 1024.0)));
        assert!(!low_for(Some((16 << 30) + 1), Some(8.0 * 1024.0 + 1.0)));
    }

    #[test]
    fn only_32_bit_graphics_memory_is_wanted_with_the_switch() {
        PORT_CONSTANTS_IN_TEST.set(Some(false));
        assert!(!wanted(0x3431_0000, 0x1000));
        PORT_CONSTANTS_IN_TEST.set(Some(true));
        assert!(wanted(0x3431_0000, 0x1000));
        assert!(!wanted(0xffff_0000, 0x2_0000));
        assert!(!wanted(0xe_1c80_0000, 0x1000));
        assert!(!wanted(0x3431_0000, 0));
        assert!(!wanted(u64::MAX, 2));
        PORT_CONSTANTS_IN_TEST.set(None);
    }

    #[test]
    fn a_stretch_of_one_committed_section_is_found() {
        let mut views = VIEWS.write().unwrap();
        views.sections.insert(77, (8 * K64, true));
        views.views.clear();
        views.views.insert(0x1000_0000, (3 * K64 as usize, 77, 0));
        views.views.insert(0x1000_0000 + 3 * K64 as usize, (5 * K64 as usize, 77, 3 * K64));
        drop(views);
        let got = stretch(0x1000_0000 + K64 as usize, 4 * K64 as usize).unwrap();
        assert_eq!(got, (0x1000_0000, 8 * K64 as usize, 77, 0));
        assert!(stretch(0x1000_0000 + 8 * K64 as usize, K64 as usize).is_none());
        let mut views = VIEWS.write().unwrap();
        views.views.clear();
        views.sections.clear();
    }
}
