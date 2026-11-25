//! Console memory: physical pages mapped at more than one address.
//!
//! XMem's reference pages are not public, so these checks are invariants,
//! not documented answers: the same 64 KiB physical pages mapped at two
//! addresses show the same bytes, and an unmapped range is gone. The
//! constants and signatures are VIctory's (`crates/victory-hle/src/modules/
//! xmem.rs`); `MEM_RESERVE` and `PAGE_READWRITE` are Win32's.

use std::ffi::c_void;

use crate::tap::Tap;

const PAGE: usize = 64 << 10;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_READWRITE: u32 = 4;
/// 64 KiB pages (VIctory's `PAGES_64K`).
const PAGES_64K: u32 = 0x2000_0000 | 0x40_0000;

#[link(name = "xmem", kind = "raw-dylib")]
unsafe extern "system" {
    fn XMemAllocatePhysicalPages(flags: u64, count: *mut usize, pages: *mut usize) -> i32;
    fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> i32;
    fn XMemMapPhysicalPages(address: *mut c_void, n: usize, pages: *const usize) -> *mut c_void;
    fn XMemVirtualAlloc(
        base: *mut c_void,
        size: usize,
        kind: u32,
        flags: u64,
        protect: u32,
    ) -> *mut c_void;
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> i32;
    fn VirtualQuery(address: *const c_void, info: *mut [usize; 6], size: usize) -> usize;
}

pub fn run(t: &mut Tap) {
    let mut pages = [0usize; 4];
    let mut n = pages.len();
    // SAFETY: (all calls) room for `n` page numbers; reservations we made.
    unsafe {
        let ok = XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr());
        t.check(
            ok != 0 && n == 4,
            "xmem: four physical pages are allocated",
            || format!("ok {ok}, n {n}"),
        );
        if ok == 0 {
            return;
        }
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                4 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        t.check(
            !a.is_null() && !b.is_null(),
            "xmem: 64 KiB-page reservations are made",
            || format!("{a:?} {b:?}"),
        );
        if a.is_null() || b.is_null() {
            XMemFreePhysicalPages(n, pages.as_ptr());
            return;
        }
        t.eq(
            XMemMapPhysicalPages(a, 4, pages.as_ptr()),
            a,
            "xmem: pages map at the first address",
        );
        // The second mapping takes the pages in another order.
        let reversed: Vec<usize> = pages.iter().rev().copied().collect();
        t.eq(
            XMemMapPhysicalPages(b, 4, reversed.as_ptr()),
            b,
            "xmem: the same pages map at a second address",
        );
        let at = |base: *mut c_void, page: usize, offset: usize| {
            base.cast::<u8>().add(page * PAGE + offset)
        };
        for page in 0..4 {
            at(a, page, 123).write_volatile(0x40 + page as u8);
        }
        let seen: Vec<u8> = (0..4)
            .map(|page| at(b, 3 - page, 123).read_volatile())
            .collect();
        t.eq(
            seen,
            vec![0x40, 0x41, 0x42, 0x43],
            "xmem: a write through one mapping reads back through the other",
        );
        at(b, 0, PAGE - 1).write_volatile(0x77);
        t.eq(
            at(a, 3, PAGE - 1).read_volatile(),
            0x77,
            "xmem: ... in both directions, to a page's last byte",
        );
        // Unmapping (no page list) leaves the range without memory.
        t.eq(
            XMemMapPhysicalPages(b, 4, std::ptr::null()),
            b,
            "xmem: a range unmaps",
        );
        let mut info = [0usize; 6];
        VirtualQuery(b, &mut info, size_of_val(&info));
        // MEMORY_BASIC_INFORMATION.State (offset 32): MEM_COMMIT is 0x1000.
        t.check(
            (info[4] & 0xffff_ffff) != 0x1000,
            "xmem: an unmapped range is not committed memory",
            || format!("state {:#x}", info[4] & 0xffff_ffff),
        );
        t.eq(
            at(a, 2, 123).read_volatile(),
            0x42,
            "xmem: the other mapping keeps its bytes",
        );
        XMemMapPhysicalPages(a, 4, std::ptr::null());
        t.check(
            XMemFreePhysicalPages(n, pages.as_ptr()) != 0,
            "xmem: physical pages are freed",
            || "refused".into(),
        );
        VirtualFree(a, 0, 0x8000);
        VirtualFree(b, 0, 0x8000);
    }
}
