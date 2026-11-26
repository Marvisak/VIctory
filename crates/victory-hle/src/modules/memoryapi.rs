//! The memory functions the title shares with PC that change memory
//! (`VirtualFree`, `VirtualProtect`, unmapping views): the host's, after the
//! GPU side lets go of title memory it holds in place there
//! (`victory_gpu::title_memory_change`). A GPU heap opened over memory
//! makes each of them fail on the host (`ERROR_INVALID_PARAMETER`) while it
//! lives; on the console the GPU reads title memory in place and these
//! always work. Everything else in these DLLs is the host's directly.
//!
//! Cost while the GPU side holds nothing in place: one atomic load a call.

// Named as the imports; the title passes the pointers to the host as is.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Memory::{
    self as mem, MEM_RELEASE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetProcessId};
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

/// The bytes from `at` to the end of the allocation holding it (0 if it is
/// not allocated).
fn rest_of_allocation(at: usize) -> usize {
    let mut info: MEMORY_BASIC_INFORMATION = unsafe_zeroed();
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: a valid out pointer of `size` bytes.
    if unsafe { mem::VirtualQuery(at as *const c_void, &mut info, size) } == 0
        || info.State == mem::MEM_FREE
    {
        return 0;
    }
    let base = info.AllocationBase;
    let mut end = info.BaseAddress as usize + info.RegionSize;
    loop {
        // SAFETY: as above.
        let n = unsafe { mem::VirtualQuery(end as *const c_void, &mut info, size) };
        if n == 0 || info.AllocationBase != base || info.State == mem::MEM_FREE {
            break;
        }
        end = info.BaseAddress as usize + info.RegionSize;
    }
    end - at
}

fn unsafe_zeroed() -> MEMORY_BASIC_INFORMATION {
    // SAFETY: plain data; zero is a valid value.
    unsafe { std::mem::zeroed() }
}

/// Whether `process` is this one (the pseudo handle or a handle to it).
fn this_process(process: HANDLE) -> bool {
    // SAFETY: plain process queries; an invalid handle gives 0.
    unsafe { process == GetCurrentProcess() || GetProcessId(process) == GetCurrentProcessId() }
}

/// Changes title memory at `[at, at + len)` with `change` (a host call,
/// nonzero when it worked) after the GPU side lets go of what it holds in
/// place there (`victory_gpu::title_memory_change`); `len` 0 means to the
/// end of the allocation (`VirtualFree`'s whole release or decommit, a
/// view's unmapping).
pub(crate) fn change(at: *const c_void, len: usize, change: impl FnMut() -> BOOL) -> BOOL {
    let mut change = change;
    if !victory_gpu::title_memory_held() || at.is_null() {
        return change();
    }
    let at = at as usize;
    let len = if len == 0 {
        rest_of_allocation(at)
    } else {
        len
    };
    victory_gpu::title_memory_change(at, len, change)
}

/// The bytes `VirtualFree` changes: a release is of the whole allocation,
/// whatever the size.
fn freed(size: usize, kind: u32) -> usize {
    if kind & MEM_RELEASE != 0 { 0 } else { size }
}

pub extern "system" fn VirtualFree(at: *mut c_void, size: usize, kind: u32) -> BOOL {
    // SAFETY: the title's arguments.
    change(at, freed(size, kind), || unsafe {
        mem::VirtualFree(at, size, kind)
    })
}

pub extern "system" fn VirtualFreeEx(
    process: HANDLE,
    at: *mut c_void,
    size: usize,
    kind: u32,
) -> BOOL {
    // SAFETY: the title's arguments.
    let free = || unsafe { mem::VirtualFreeEx(process, at, size, kind) };
    if this_process(process) {
        change(at, freed(size, kind), free)
    } else {
        free()
    }
}

pub extern "system" fn VirtualProtect(
    at: *const c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> BOOL {
    // SAFETY: the title's arguments.
    let set = || unsafe { mem::VirtualProtect(at, size, protect, old) };
    if size != 0 {
        change(at, size, set)
    } else {
        set()
    }
}

pub extern "system" fn VirtualProtectEx(
    process: HANDLE,
    at: *const c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> BOOL {
    // SAFETY: the title's arguments.
    let set = || unsafe { mem::VirtualProtectEx(process, at, size, protect, old) };
    if this_process(process) && size != 0 {
        change(at, size, set)
    } else {
        set()
    }
}

pub extern "system" fn UnmapViewOfFile(at: MEMORY_MAPPED_VIEW_ADDRESS) -> BOOL {
    // SAFETY: the title's argument.
    change(at.Value, 0, || unsafe { mem::UnmapViewOfFile(at) })
}

pub extern "system" fn UnmapViewOfFileEx(at: MEMORY_MAPPED_VIEW_ADDRESS, flags: u32) -> BOOL {
    // SAFETY: the title's arguments.
    change(at.Value, 0, || unsafe { mem::UnmapViewOfFileEx(at, flags) })
}

pub extern "system" fn UnmapViewOfFile2(
    process: HANDLE,
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    flags: u32,
) -> BOOL {
    // SAFETY: the title's arguments.
    let unmap = || unsafe { mem::UnmapViewOfFile2(process, at, flags) };
    if this_process(process) {
        change(at.Value, 0, unmap)
    } else {
        unmap()
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "memoryapi",
        dlls: &["api-ms-win-core-memory-*", "kernel32.dll", "kernelbase.dll"],
        exports: vec![
            hle_export!(VirtualFree),
            hle_export!(VirtualFreeEx),
            hle_export!(VirtualProtect),
            hle_export!(VirtualProtectEx),
            hle_export!(UnmapViewOfFile),
            hle_export!(UnmapViewOfFileEx),
            hle_export!(UnmapViewOfFile2),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rest_of_an_allocation_spans_its_regions() {
        const PAGE: usize = 4096;
        // SAFETY: plain allocation; the middle page gets other protection,
        // so the allocation is three regions.
        let at = unsafe {
            let at = mem::VirtualAlloc(
                std::ptr::null(),
                3 * PAGE,
                mem::MEM_RESERVE | mem::MEM_COMMIT,
                mem::PAGE_READWRITE,
            ) as usize;
            assert_ne!(at, 0);
            let mut old = 0;
            assert_ne!(
                mem::VirtualProtect(
                    (at + PAGE) as *const c_void,
                    PAGE,
                    mem::PAGE_READONLY,
                    &mut old
                ),
                0
            );
            at
        };
        assert_eq!(rest_of_allocation(at), 3 * PAGE);
        assert_eq!(rest_of_allocation(at + PAGE + 16), 2 * PAGE - 16);
        // The wrappers do what the host's functions do.
        let mut old = 0;
        assert_ne!(
            VirtualProtect(
                (at + PAGE) as *const c_void,
                PAGE,
                mem::PAGE_READWRITE,
                &mut old
            ),
            0
        );
        assert_eq!(old, mem::PAGE_READONLY);
        assert_ne!(VirtualFree(at as *mut c_void, 0, MEM_RELEASE), 0);
        assert_eq!(rest_of_allocation(at), 0, "freed");
    }

    #[test]
    fn the_title_s_memory_functions_are_served_here_and_the_rest_by_the_host() {
        let r = crate::HleResolver::default();
        let name = |n: &str| victory_pe::ImportSymbol::Name {
            hint: 0,
            name: n.into(),
        };
        for dll in [
            "api-ms-win-core-memory-l1-1-0.dll",
            "kernel32.dll",
            "kernelbase.dll",
        ] {
            assert_eq!(
                r.classify(dll, &name("VirtualFree")),
                Some(victory_loader::Provenance::Hle)
            );
        }
        assert_eq!(
            r.classify("api-ms-win-core-memory-l1-1-0.dll", &name("VirtualAlloc")),
            Some(victory_loader::Provenance::Host)
        );
    }
}
