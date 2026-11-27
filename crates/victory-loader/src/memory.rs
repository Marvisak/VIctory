//! Debugging aid: reading memory of this process that may be gone.

use std::ffi::c_void;

use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// A copy of `len` bytes at `at`, or `None` if any of them cannot be read
/// (freed, never committed, guarded): for diagnostics that watch objects
/// the title may free at any time.
pub fn read_memory(at: usize, len: usize) -> Option<Vec<u8>> {
    let mut copy = vec![0u8; len];
    let mut read = 0;
    // SAFETY: the system reads our own address space into `copy`, of `len`
    // bytes, and fails instead of faulting on memory it cannot read.
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            at as *const c_void,
            copy.as_mut_ptr().cast(),
            len,
            &mut read,
        )
    };
    (ok != 0 && read == len).then_some(copy)
}

/// The committed, read-write, file- or section-mapped regions of this
/// process between `start` and `end`, as `(base, size)`: for diagnostics
/// that look for title data in its mapped memory.
pub fn mapped_regions(start: usize, end: usize) -> Vec<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    let mut found = Vec::new();
    let mut at = start;
    while at < end {
        // SAFETY: zeroes are a valid MEMORY_BASIC_INFORMATION.
        let mut region: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: describes our own address space into `region`, of its size.
        if unsafe { VirtualQuery(at as *const c_void, &mut region, size_of_val(&region)) } == 0 {
            break;
        }
        let base = region.BaseAddress as usize;
        let next = base.saturating_add(region.RegionSize);
        if region.State == MEM_COMMIT
            && region.Type == MEM_MAPPED
            && region.Protect == PAGE_READWRITE
        {
            found.push((base, region.RegionSize));
        }
        if next <= at {
            break;
        }
        at = next;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    #[test]
    fn readable_memory_is_copied() {
        let value = [1u8, 2, 3, 4];
        assert_eq!(
            read_memory(value.as_ptr() as usize, 4),
            Some(value.to_vec())
        );
    }

    #[test]
    fn unmapped_memory_reads_as_none() {
        // SAFETY: a fresh page, released at once; only its address is kept.
        let page = unsafe {
            let p = VirtualAlloc(
                std::ptr::null(),
                0x1000,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            assert!(!p.is_null());
            VirtualFree(p, 0, MEM_RELEASE);
            p as usize
        };
        assert_eq!(read_memory(page, 16), None);
    }
}
