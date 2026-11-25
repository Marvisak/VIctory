//! Private Win32 heap global allocator for the `victory` host process.
//!
//! `victory-gpu` installs a process-wide Vectored Exception Handler that
//! acquires `GUARDS` and manipulates Rust collections. Sharing
//! `GetProcessHeap()` with the guest title, `ntdll`, `kernelbase`, and
//! `d3d12core.dll` can AB-BA deadlock between the process heap lock and
//! `GUARDS`. Giving Rust its own growable serialized heap via `HeapCreate`
//! keeps host allocations completely independent of `GetProcessHeap()`.

use std::alloc::{GlobalAlloc, Layout};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use windows_sys::Win32::System::Memory::{
    HEAP_ZERO_MEMORY, HeapAlloc, HeapCreate, HeapDestroy, HeapFree, HeapReAlloc,
};

#[cfg(target_pointer_width = "64")]
const MIN_ALIGN: usize = 16;
#[cfg(target_pointer_width = "32")]
const MIN_ALIGN: usize = 8;

const HEADER_SIZE: usize = size_of::<*mut u8>();

static HEAP: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Returns the private Win32 heap handle, creating it lazily on first use.
pub fn handle() -> *mut c_void {
    let h = HEAP.load(Ordering::Acquire);
    if !h.is_null() {
        return h;
    }
    // SAFETY: `HeapCreate(0, 0, 0)` creates a private, growable, serialized
    // Win32 heap independent of `GetProcessHeap()`.
    let created = unsafe { HeapCreate(0, 0, 0) };
    if created.is_null() {
        return ptr::null_mut();
    }
    match HEAP.compare_exchange(
        ptr::null_mut(),
        created,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => created,
        Err(existing) => {
            // SAFETY: `created` was just created by this thread and never
            // published or allocated from; destroy the losing heap.
            unsafe {
                let _ = HeapDestroy(created);
            }
            existing
        }
    }
}

/// Global allocator backed by a dedicated Win32 heap (`HeapCreate(0, 0, 0)`).
pub struct PrivateHeap;

impl PrivateHeap {
    unsafe fn alloc_impl(&self, layout: Layout, flags: u32) -> *mut u8 {
        let heap = handle();
        if heap.is_null() {
            return ptr::null_mut();
        }
        let align = layout.align();
        if align <= MIN_ALIGN {
            // SAFETY: `heap` is our live private Win32 heap; `HeapAlloc`
            // guarantees at least `MIN_ALIGN`-byte alignment on Windows.
            return unsafe { HeapAlloc(heap, flags, layout.size()).cast() };
        }

        let Some(total) = layout.size().checked_add(align) else {
            return ptr::null_mut();
        };
        // SAFETY: `heap` is our live private Win32 heap.
        let raw: *mut u8 = unsafe { HeapAlloc(heap, flags, total).cast() };
        if raw.is_null() {
            return ptr::null_mut();
        }

        let raw_addr = raw as usize;
        let mask = align - 1;
        let aligned_addr = raw_addr
            .wrapping_add(HEADER_SIZE)
            .wrapping_add(mask)
            & !mask;
        let offset = aligned_addr.wrapping_sub(raw_addr);
        debug_assert!(offset >= HEADER_SIZE && offset <= align);

        // SAFETY: `raw` is aligned to `MIN_ALIGN >= HEADER_SIZE` and has
        // `layout.size() + align` bytes available, so `offset` lies in
        // `[HEADER_SIZE, align]`, `aligned.sub(HEADER_SIZE)` is aligned to
        // `HEADER_SIZE` and inside the allocation, and `[aligned, aligned + size)`
        // stays within `[raw, raw + total)`.
        unsafe {
            let aligned = raw.add(offset);
            aligned.sub(HEADER_SIZE).cast::<*mut u8>().write(raw);
            aligned
        }
    }
}

// SAFETY: `PrivateHeap` delegates to a serialized Win32 heap created via
// `HeapCreate(0, 0, 0)`, preserves all requested alignments (including
// over-aligned layouts via a stored back-pointer), and preserves existing
// contents on `realloc`.
unsafe impl GlobalAlloc for PrivateHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Caller upholds the `GlobalAlloc::alloc` contract.
        unsafe { self.alloc_impl(layout, 0) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Caller upholds the `GlobalAlloc::alloc_zeroed` contract.
        // `HEAP_ZERO_MEMORY` zeroes the entire block; for over-aligned layouts
        // the header is written strictly before the returned pointer.
        unsafe { self.alloc_impl(layout, HEAP_ZERO_MEMORY) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let heap = handle();
        if heap.is_null() {
            return;
        }
        let raw = if layout.align() <= MIN_ALIGN {
            ptr
        } else {
            // SAFETY: `ptr` was produced by `alloc_impl` for `align > MIN_ALIGN`,
            // which wrote the original `HeapAlloc` pointer at `ptr.sub(HEADER_SIZE)`.
            unsafe { ptr.sub(HEADER_SIZE).cast::<*mut u8>().read() }
        };
        // SAFETY: `raw` was returned by `HeapAlloc`/`HeapReAlloc` on `heap`
        // and has not yet been freed.
        unsafe {
            let _ = HeapFree(heap, 0, raw.cast());
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.align() <= MIN_ALIGN {
            let heap = handle();
            if heap.is_null() {
                return ptr::null_mut();
            }
            // SAFETY: `ptr` was allocated directly on `heap` (no over-alignment
            // header), and caller upholds the `GlobalAlloc::realloc` contract.
            return unsafe { HeapReAlloc(heap, 0, ptr.cast(), new_size).cast() };
        }

        let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
            return ptr::null_mut();
        };
        // SAFETY: `new_layout` has the requested size and alignment.
        let new_ptr = unsafe { self.alloc(new_layout) };
        if !new_ptr.is_null() {
            let copy_len = layout.size().min(new_size);
            // SAFETY: `ptr` is valid for `layout.size()` bytes, `new_ptr` is a
            // newly allocated non-overlapping block valid for `new_size` bytes,
            // and `dealloc` frees the original allocation with its `layout`.
            unsafe {
                ptr::copy_nonoverlapping(ptr, new_ptr, copy_len);
                self.dealloc(ptr, layout);
            }
        }
        new_ptr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::GetProcessHeap;

    const ALIGNMENTS: &[usize] = &[1, 8, 16, 32, 64, 256, 4096];

    #[test]
    fn handle_is_distinct_from_process_heap() {
        let priv_heap = handle();
        assert!(!priv_heap.is_null(), "private heap handle must be non-null");
        // SAFETY: `GetProcessHeap` takes no arguments and is always safe to call.
        let proc_heap = unsafe { GetProcessHeap() };
        assert!(!proc_heap.is_null(), "process heap handle must be non-null");
        assert_ne!(
            priv_heap, proc_heap,
            "private heap must be separate from GetProcessHeap()"
        );
        assert_eq!(priv_heap, handle(), "handle() must be idempotent");
    }

    #[test]
    fn alloc_and_dealloc_all_alignments() {
        let alloc = PrivateHeap;
        for &align in ALIGNMENTS {
            for &size in &[1usize, 7, 16, 63, 257, 4096] {
                let layout = Layout::from_size_align(size, align).unwrap();
                // SAFETY: `layout` has non-zero size and power-of-two alignment.
                let ptr = unsafe { alloc.alloc(layout) };
                assert!(!ptr.is_null(), "alloc failed for size={size}, align={align}");
                assert_eq!(
                    (ptr as usize) % align,
                    0,
                    "pointer {:p} not aligned to {align}",
                    ptr
                );
                // SAFETY: `ptr` is valid for `size` writable bytes; then freed with `layout`.
                unsafe {
                    ptr::write_bytes(ptr, 0xA5, size);
                    let slice = std::slice::from_raw_parts(ptr, size);
                    assert!(slice.iter().all(|&b| b == 0xA5));
                    alloc.dealloc(ptr, layout);
                }
            }
        }
    }

    #[test]
    fn alloc_zeroed_all_alignments() {
        let alloc = PrivateHeap;
        for &align in ALIGNMENTS {
            for &size in &[1usize, 19, 128, 1024, 4096] {
                let layout = Layout::from_size_align(size, align).unwrap();
                // SAFETY: `layout` has non-zero size and power-of-two alignment.
                let ptr = unsafe { alloc.alloc_zeroed(layout) };
                assert!(
                    !ptr.is_null(),
                    "alloc_zeroed failed for size={size}, align={align}"
                );
                assert_eq!((ptr as usize) % align, 0);
                // SAFETY: `ptr` is valid for `size` zero-initialized bytes; then freed with `layout`.
                unsafe {
                    let slice = std::slice::from_raw_parts(ptr, size);
                    assert!(
                        slice.iter().all(|&b| b == 0),
                        "alloc_zeroed returned non-zero byte for size={size}, align={align}"
                    );
                    alloc.dealloc(ptr, layout);
                }
            }
        }
    }

    #[test]
    fn realloc_grow_and_shrink_all_alignments() {
        let alloc = PrivateHeap;
        for &align in ALIGNMENTS {
            let initial_size = 64usize;
            let grown_size = 512usize;
            let shrunk_size = 24usize;

            let initial_layout = Layout::from_size_align(initial_size, align).unwrap();
            // SAFETY: `initial_layout` has non-zero size and power-of-two alignment.
            let ptr = unsafe { alloc.alloc(initial_layout) };
            assert!(!ptr.is_null());
            assert_eq!((ptr as usize) % align, 0);

            // SAFETY: `ptr` is valid for `initial_size` bytes.
            unsafe {
                for i in 0..initial_size {
                    ptr.add(i).write((i as u8).wrapping_mul(3).wrapping_add(1));
                }
            }

            // Grow
            // SAFETY: `ptr` was allocated with `initial_layout` and `grown_size > 0`.
            let grown = unsafe { alloc.realloc(ptr, initial_layout, grown_size) };
            assert!(!grown.is_null(), "realloc grow failed for align={align}");
            assert_eq!((grown as usize) % align, 0);
            // SAFETY: First `initial_size` bytes were preserved; write tail up to `grown_size`.
            unsafe {
                for i in 0..initial_size {
                    assert_eq!(
                        grown.add(i).read(),
                        (i as u8).wrapping_mul(3).wrapping_add(1),
                        "grow corrupted byte {i} for align={align}"
                    );
                }
                for i in initial_size..grown_size {
                    grown.add(i).write(0x5A);
                }
            }

            // Shrink
            let grown_layout = Layout::from_size_align(grown_size, align).unwrap();
            // SAFETY: `grown` was allocated with `grown_layout` and `shrunk_size > 0`.
            let shrunk = unsafe { alloc.realloc(grown, grown_layout, shrunk_size) };
            assert!(!shrunk.is_null(), "realloc shrink failed for align={align}");
            assert_eq!((shrunk as usize) % align, 0);
            // SAFETY: First `shrunk_size` bytes were preserved; then deallocate with `shrunk_layout`.
            unsafe {
                for i in 0..shrunk_size {
                    assert_eq!(
                        shrunk.add(i).read(),
                        (i as u8).wrapping_mul(3).wrapping_add(1),
                        "shrink corrupted byte {i} for align={align}"
                    );
                }
                let shrunk_layout = Layout::from_size_align(shrunk_size, align).unwrap();
                alloc.dealloc(shrunk, shrunk_layout);
            }
        }
    }
}
