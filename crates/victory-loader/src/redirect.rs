//! Debugging aid: sending one call in a mapped image's code to a handler of
//! ours.
//!
//! A `call rel32` (`E8`) at a site the caller has verified is rewritten to
//! call a 16-byte stub placed within reach of the image:
//!
//! ```text
//! mov rax, <handler>   ; 48 B8 imm64
//! jmp rax              ; FF E0
//! ```
//!
//! The jump leaves the title's return address and arguments where its call
//! put them, so the handler runs as if the title had called it, and can call
//! the old callee (returned by [`LoadedImage::redirect_call`]) with the same
//! arguments. `rax` is free at a call: the callee owns it.

use std::ffi::c_void;
use std::sync::Mutex;

use anyhow::Result;
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::{CodeWrite, LoadedImage};

/// What became of [`LoadedImage::redirect_call`] when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirect {
    /// The call now reaches the handler; it used to reach `original`.
    Done { original: usize },
    /// Nothing in the image was changed, for this reason.
    Refused(&'static str),
}

/// Bytes of one stub.
const STUB: usize = 16;
/// Bytes of a page of stubs.
const PAGE: usize = 0x1000;
/// Where `VirtualAlloc` can place a reservation.
const GRANULARITY: usize = 0x1_0000;
/// How far a `rel32` reaches, kept a little short of `i32::MAX`.
const REACH: usize = 0x7FFF_0000;

/// A page of stubs and how much of it is used.
struct StubPage {
    start: usize,
    used: usize,
}

/// Stub pages, never freed: the code they serve lives as long as the process.
static PAGES: Mutex<Vec<StubPage>> = Mutex::new(Vec::new());

impl LoadedImage {
    /// Makes the `call rel32` at `site_rva` call `handler` (through a stub
    /// within reach of the image) and returns the callee it called before.
    /// The handler must have the callee's signature and calling convention.
    /// An error means the call was written but its page protection could not
    /// be restored or the instruction cache flushed.
    ///
    /// # Safety
    /// As [`LoadedImage::write_code`]: no thread runs or reads the site while
    /// it changes, and none has run it yet. `handler` stays valid for the
    /// life of the process.
    pub unsafe fn redirect_call(&self, site_rva: usize, handler: usize) -> Result<Redirect> {
        let (base, size) = (self.base(), self.info().size_of_image as usize);
        if site_rva.checked_add(5).is_none_or(|end| end > size) {
            return Ok(Redirect::Refused("outside the image"));
        }
        let site = base + site_rva;
        let mut code = [0u8; 5];
        // SAFETY: inside the mapped image (checked above), readable code.
        unsafe { code.copy_from_slice(std::slice::from_raw_parts(site as *const u8, 5)) };
        let Some(original) = call_target(site, &code) else {
            return Ok(Redirect::Refused("not a call"));
        };
        let Some(stub_at) = place_stub(base, base + size, handler as u64) else {
            return Ok(Redirect::Refused(
                "no memory for its stub within reach of the image",
            ));
        };
        let Some(call) = encode_call(site, stub_at) else {
            return Ok(Redirect::Refused("its stub is out of reach"));
        };
        // SAFETY: the caller's promise.
        match unsafe { crate::patch::write_code(base, size, site_rva, &call) }? {
            CodeWrite::Done => Ok(Redirect::Done { original }),
            CodeWrite::Refused(why) => Ok(Redirect::Refused(why)),
        }
    }
}

/// Where the `call rel32` at `site` goes, or `None` if `code` is not one.
pub(crate) fn call_target(site: usize, code: &[u8; 5]) -> Option<usize> {
    if code[0] != 0xE8 {
        return None;
    }
    let rel = i32::from_le_bytes([code[1], code[2], code[3], code[4]]);
    Some((site as i64 + 5 + i64::from(rel)) as usize)
}

/// A `call rel32` at `site` to `target`, or `None` if out of reach.
pub(crate) fn encode_call(site: usize, target: usize) -> Option<[u8; 5]> {
    let rel = i32::try_from(target as i64 - (site as i64 + 5)).ok()?;
    let [a, b, c, d] = rel.to_le_bytes();
    Some([0xE8, a, b, c, d])
}

/// The stub that jumps to `handler`.
pub(crate) fn stub(handler: u64) -> [u8; STUB] {
    let mut code = [0xCC; STUB];
    code[..2].copy_from_slice(&[0x48, 0xB8]);
    code[2..10].copy_from_slice(&handler.to_le_bytes());
    code[10..12].copy_from_slice(&[0xFF, 0xE0]);
    code
}

/// Whether `len` bytes at `at` can be called from anywhere in `lo..hi`.
fn reaches(lo: usize, hi: usize, at: usize, len: usize) -> bool {
    at >= hi.saturating_sub(REACH) && at + len <= lo + REACH
}

/// `len` bytes of fresh read-write memory callable from anywhere in
/// `lo..hi`, nearest below `lo` first, then above `hi`.
pub(crate) fn alloc_near(lo: usize, hi: usize, len: usize) -> Option<usize> {
    if hi.checked_sub(lo)? + len >= REACH {
        return None;
    }
    let try_at = |at: usize| {
        // SAFETY: reserves and commits fresh pages at `at`, or fails.
        let p = unsafe {
            VirtualAlloc(
                at as *const c_void,
                len,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        (!p.is_null()).then_some(p as usize)
    };
    let top = lo.saturating_sub(len) & !(GRANULARITY - 1);
    let below = (0..)
        .map(|i| top.wrapping_sub(i * GRANULARITY))
        .take_while(|&at| at != 0 && at <= top && reaches(lo, hi, at, len));
    let above = (0..)
        .map(|i| hi.next_multiple_of(GRANULARITY) + i * GRANULARITY)
        .take_while(|&at| reaches(lo, hi, at, len));
    below
        .chain(above)
        .find_map(|at| try_at(at).filter(|&p| reaches(lo, hi, p, len)))
}

/// Writes a stub to `handler` on a stub page within reach of `lo..hi` and
/// returns its address.
fn place_stub(lo: usize, hi: usize, handler: u64) -> Option<usize> {
    let mut pages = PAGES.lock().unwrap_or_else(|e| e.into_inner());
    let found = pages
        .iter()
        .position(|p| p.used + STUB <= PAGE && reaches(lo, hi, p.start, PAGE));
    let index = match found {
        Some(i) => i,
        None => {
            let start = alloc_near(lo, hi, PAGE)?;
            pages.push(StubPage { start, used: 0 });
            pages.len() - 1
        }
    };
    let page = &mut pages[index];
    let at = page.start + page.used;
    let mut old = 0;
    // SAFETY: our own stub page; no stub on it runs before the image does.
    if unsafe { VirtualProtect(page.start as *const c_void, PAGE, PAGE_READWRITE, &mut old) } == 0 {
        return None;
    }
    // SAFETY: writable now; `at..at + STUB` lies inside the page.
    unsafe { (at as *mut u8).copy_from_nonoverlapping(stub(handler).as_ptr(), STUB) };
    page.used += STUB;
    // SAFETY: as above; the page holds code from now on.
    let sealed = unsafe {
        VirtualProtect(
            page.start as *const c_void,
            PAGE,
            PAGE_EXECUTE_READ,
            &mut old,
        )
    };
    // SAFETY: flushes a range of our own process.
    let flushed = unsafe { FlushInstructionCache(GetCurrentProcess(), at as *const c_void, STUB) };
    (sealed != 0 && flushed != 0).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};

    #[test]
    fn a_call_decodes_to_its_target() {
        assert_eq!(call_target(0x1000, &[0xE8, 0xFB, 0x0F, 0, 0]), Some(0x2000));
        assert_eq!(call_target(0x1000, &[0xE9, 0xFB, 0x0F, 0, 0]), None);
    }

    #[test]
    fn encode_and_decode_round_trip() {
        let site = 0x1_4000_0000;
        for target in [site + 0x7000_0000, site - 0x7000_0000, site + 5] {
            let call = encode_call(site, target).unwrap();
            assert_eq!(call_target(site, &call), Some(target));
        }
    }

    #[test]
    fn a_target_out_of_rel32_range_is_refused() {
        let site = 0x1_0000_0000;
        assert_eq!(encode_call(site, site + 0x8000_0000 + 5), None);
        assert!(encode_call(site, site + 0x7FFF_FFFF + 5).is_some());
    }

    #[test]
    fn the_stub_jumps_to_its_handler() {
        assert_eq!(
            stub(0x1122_3344_5566_7788),
            [
                0x48, 0xB8, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xFF, 0xE0, 0xCC, 0xCC,
                0xCC, 0xCC
            ]
        );
    }

    extern "win64" fn answer() -> u64 {
        42
    }

    #[test]
    fn a_near_stub_runs_its_handler() {
        const IMAGE: usize = 0x10_0000;
        // SAFETY: a fresh reservation standing in for an image, released below.
        let image =
            unsafe { VirtualAlloc(std::ptr::null(), IMAGE, MEM_RESERVE, PAGE_READWRITE) } as usize;
        assert_ne!(image, 0);
        let near = alloc_near(image, image + IMAGE, PAGE).expect("memory near the image");
        assert!(reaches(image, image + IMAGE, near, PAGE));
        // SAFETY: `near` is a fresh read-write page of ours.
        unsafe {
            (near as *mut u8)
                .copy_from_nonoverlapping(stub(answer as *const () as u64).as_ptr(), STUB)
        };
        let mut old = 0;
        // SAFETY: our page; it holds the stub only.
        let sealed =
            unsafe { VirtualProtect(near as *const c_void, PAGE, PAGE_EXECUTE_READ, &mut old) };
        assert_ne!(sealed, 0);
        // SAFETY: the page holds a complete stub that jumps to `answer`,
        // whose signature this is.
        let call: extern "win64" fn() -> u64 = unsafe { std::mem::transmute(near) };
        let result = call();
        // SAFETY: both allocated above with VirtualAlloc.
        unsafe {
            VirtualFree(near as *mut c_void, 0, MEM_RELEASE);
            VirtualFree(image as *mut c_void, 0, MEM_RELEASE);
        }
        assert_eq!(result, 42);
    }
}
