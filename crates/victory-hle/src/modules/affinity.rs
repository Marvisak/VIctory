//! `VICTORY_TITLE_AFFINITY=ignore`: where the title asks its threads to run
//! is remembered and told back to it, and never given to the host.
//!
//! A console title ties its threads to the console's processors, whose
//! numbers mean something else on the host: two of them may be the two
//! threads of one host core while other cores idle. With the switch the
//! system places the title's threads on any processor.
//!
//! With the variable unset (the default) this module is not registered, so
//! these imports go to the host exactly as before.
//!
//! Asks the host would refuse for their shape (no processors, another
//! processor group, a handle that is not a thread's) still go to the host,
//! for its own answer. What a thread asked for is kept by its id and not
//! forgotten when it exits.
//!
//! The ignore mode follows maren-halv/VIctory-Fast eb37550c (GPL-3.0).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::{HleModule, hle_export};

/// Win32 `GROUP_AFFINITY`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupAffinity {
    pub mask: usize,
    pub group: u16,
    pub reserved: [u16; 3],
}

/// Win32 `PROCESSOR_NUMBER`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessorNumber {
    pub group: u16,
    pub number: u8,
    pub reserved: u8,
}

const _: () = assert!(size_of::<GroupAffinity>() == 16 && size_of::<ProcessorNumber>() == 4);

/// `SetThreadIdealProcessor`'s processor that only asks for the current one.
const MAXIMUM_PROCESSORS: u32 = 64;

/// What a thread asked for last (for what it is told back).
#[derive(Clone, Copy, Default)]
struct Asked {
    affinity: Option<GroupAffinity>,
    ideal: Option<u8>,
}

/// By thread id.
fn asked() -> MutexGuard<'static, HashMap<u32, Asked>> {
    static ASKED: OnceLock<Mutex<HashMap<u32, Asked>>> = OnceLock::new();
    ASKED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Whether a `VICTORY_TITLE_AFFINITY` value asks for the asks to be ignored.
fn ignore_from(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("ignore"))
}

/// Whether this module takes the title's placement calls
/// (`VICTORY_TITLE_AFFINITY=ignore`).
pub(crate) fn serving() -> bool {
    let value = std::env::var("VICTORY_TITLE_AFFINITY").ok();
    let ignore = ignore_from(value.as_deref());
    if ignore {
        tracing::info!(
            "VICTORY_TITLE_AFFINITY=ignore: the title's thread placement is remembered, not given to the host"
        );
    } else if let Some(value) = value.filter(|v| !v.is_empty()) {
        tracing::warn!(
            value,
            "VICTORY_TITLE_AFFINITY: only `ignore` is known; nothing changes"
        );
    }
    ignore
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "affinity",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-processthreads-*",
            "api-ms-win-core-processtopology-*",
        ],
        exports: vec![
            hle_export!(SetThreadAffinityMask),
            hle_export!(SetThreadGroupAffinity),
            hle_export!(GetThreadGroupAffinity),
            hle_export!(SetThreadIdealProcessor),
            hle_export!(SetThreadIdealProcessorEx),
        ],
    }
}

/// Remembers `now` as what thread `id` asked for; what it asked for before,
/// or else what the host had.
fn swap_affinity(id: u32, now: GroupAffinity, host: Option<GroupAffinity>) -> GroupAffinity {
    let before = asked().entry(id).or_default().affinity.replace(now);
    before.or(host).unwrap_or(GroupAffinity {
        mask: sys::process_mask(),
        ..Default::default()
    })
}

/// [`swap_affinity`] for the ideal processor.
fn swap_ideal(id: u32, now: u8, host: Option<u8>) -> u8 {
    let before = asked().entry(id).or_default().ideal.replace(now);
    before.or(host).unwrap_or(0)
}

pub extern "system" fn SetThreadAffinityMask(thread: *mut c_void, mask: usize) -> usize {
    tracing::debug!(
        mask = format_args!("{mask:#x}"),
        "title SetThreadAffinityMask (ignored)"
    );
    let Some(id) = (mask != 0).then(|| sys::thread_id(thread)).flatten() else {
        return sys::set_affinity_mask(thread, mask);
    };
    let host = sys::group_affinity(thread);
    let now = GroupAffinity {
        mask,
        group: host.unwrap_or_default().group,
        reserved: [0; 3],
    };
    swap_affinity(id, now, host).mask
}

pub extern "system" fn SetThreadGroupAffinity(
    thread: *mut c_void,
    wanted: *const GroupAffinity,
    previous: *mut GroupAffinity,
) -> i32 {
    // SAFETY: the title passes a `GROUP_AFFINITY` or nothing.
    let ours = unsafe { wanted.as_ref() }
        .copied()
        .filter(|w| w.mask != 0 && w.group == 0 && w.reserved == [0; 3]);
    let Some((now, id)) = ours.and_then(|w| Some((w, sys::thread_id(thread)?))) else {
        return sys::set_group_affinity(thread, wanted, previous);
    };
    tracing::debug!(
        mask = format_args!("{:#x}", now.mask),
        "title SetThreadGroupAffinity (ignored)"
    );
    let before = swap_affinity(id, now, sys::group_affinity(thread));
    if !previous.is_null() {
        // SAFETY: the title's own output, checked.
        unsafe { previous.write(before) };
    }
    1
}

pub extern "system" fn GetThreadGroupAffinity(thread: *mut c_void, out: *mut GroupAffinity) -> i32 {
    let told = (!out.is_null())
        .then(|| sys::thread_id(thread))
        .flatten()
        .and_then(|id| asked().get(&id).and_then(|a| a.affinity));
    match told {
        Some(told) => {
            // SAFETY: the title's own output, checked.
            unsafe { out.write(told) };
            1
        }
        None => sys::get_group_affinity(thread, out),
    }
}

pub extern "system" fn SetThreadIdealProcessor(thread: *mut c_void, ideal: u32) -> u32 {
    let Some(id) = sys::thread_id(thread) else {
        return sys::set_ideal(thread, ideal);
    };
    if ideal >= MAXIMUM_PROCESSORS {
        // Only asks which it is (or is refused by the host).
        let told = asked().get(&id).and_then(|a| a.ideal);
        return told.map_or_else(|| sys::set_ideal(thread, ideal), u32::from);
    }
    tracing::debug!(ideal, "title SetThreadIdealProcessor (ignored)");
    u32::from(swap_ideal(id, ideal as u8, sys::ideal(thread)))
}

pub extern "system" fn SetThreadIdealProcessorEx(
    thread: *mut c_void,
    ideal: *const ProcessorNumber,
    previous: *mut ProcessorNumber,
) -> i32 {
    // SAFETY: the title passes a `PROCESSOR_NUMBER` or nothing.
    let ours = unsafe { ideal.as_ref() }
        .copied()
        .filter(|w| w.group == 0 && u32::from(w.number) < MAXIMUM_PROCESSORS);
    let Some((now, id)) = ours.and_then(|w| Some((w, sys::thread_id(thread)?))) else {
        return sys::set_ideal_ex(thread, ideal, previous);
    };
    tracing::debug!(
        ideal = now.number,
        "title SetThreadIdealProcessorEx (ignored)"
    );
    let before = swap_ideal(id, now.number, sys::ideal(thread));
    if !previous.is_null() {
        // SAFETY: the title's own output, checked.
        unsafe {
            previous.write(ProcessorNumber {
                number: before,
                ..Default::default()
            })
        };
    }
    1
}

/// The host's own calls.
mod sys {
    use std::ffi::c_void;

    use super::{GroupAffinity, ProcessorNumber};

    mod raw {
        use std::ffi::c_void;

        use super::{GroupAffinity, ProcessorNumber};

        #[link(name = "kernel32")]
        unsafe extern "system" {
            pub(super) fn SetThreadAffinityMask(thread: *mut c_void, mask: usize) -> usize;
            pub(super) fn SetThreadGroupAffinity(
                thread: *mut c_void,
                wanted: *const GroupAffinity,
                previous: *mut GroupAffinity,
            ) -> i32;
            pub(super) fn GetThreadGroupAffinity(
                thread: *mut c_void,
                out: *mut GroupAffinity,
            ) -> i32;
            pub(super) fn SetThreadIdealProcessor(thread: *mut c_void, ideal: u32) -> u32;
            pub(super) fn SetThreadIdealProcessorEx(
                thread: *mut c_void,
                ideal: *const ProcessorNumber,
                previous: *mut ProcessorNumber,
            ) -> i32;
            pub(super) fn GetThreadIdealProcessorEx(
                thread: *mut c_void,
                out: *mut ProcessorNumber,
            ) -> i32;
            pub(super) fn GetThreadId(thread: *mut c_void) -> u32;
            pub(super) fn GetCurrentProcess() -> *mut c_void;
            pub(super) fn GetProcessAffinityMask(
                process: *mut c_void,
                process_mask: *mut usize,
                system_mask: *mut usize,
            ) -> i32;
        }
    }

    // SAFETY (every function): forwarding the title's own arguments to the
    // host API of the same name, or asking it about a handle it checks.

    pub(super) fn set_affinity_mask(thread: *mut c_void, mask: usize) -> usize {
        // SAFETY: see above.
        unsafe { raw::SetThreadAffinityMask(thread, mask) }
    }

    pub(super) fn set_group_affinity(
        thread: *mut c_void,
        wanted: *const GroupAffinity,
        previous: *mut GroupAffinity,
    ) -> i32 {
        // SAFETY: see above.
        unsafe { raw::SetThreadGroupAffinity(thread, wanted, previous) }
    }

    pub(super) fn get_group_affinity(thread: *mut c_void, out: *mut GroupAffinity) -> i32 {
        // SAFETY: see above.
        unsafe { raw::GetThreadGroupAffinity(thread, out) }
    }

    /// The processors the host has the thread on, if it says.
    pub(super) fn group_affinity(thread: *mut c_void) -> Option<GroupAffinity> {
        let mut out = GroupAffinity::default();
        (get_group_affinity(thread, &mut out) != 0 && out.mask != 0).then_some(out)
    }

    pub(super) fn set_ideal(thread: *mut c_void, ideal: u32) -> u32 {
        // SAFETY: see above.
        unsafe { raw::SetThreadIdealProcessor(thread, ideal) }
    }

    pub(super) fn set_ideal_ex(
        thread: *mut c_void,
        ideal: *const ProcessorNumber,
        previous: *mut ProcessorNumber,
    ) -> i32 {
        // SAFETY: see above.
        unsafe { raw::SetThreadIdealProcessorEx(thread, ideal, previous) }
    }

    /// The host's ideal processor of the thread, if it says.
    pub(super) fn ideal(thread: *mut c_void) -> Option<u8> {
        let mut out = ProcessorNumber::default();
        // SAFETY: see above.
        (unsafe { raw::GetThreadIdealProcessorEx(thread, &mut out) } != 0).then_some(out.number)
    }

    /// The thread's id: none for a handle that is not a thread's (or may not
    /// be asked).
    pub(super) fn thread_id(thread: *mut c_void) -> Option<u32> {
        // SAFETY: see above.
        Some(unsafe { raw::GetThreadId(thread) }).filter(|&id| id != 0)
    }

    /// The processors this process may use (every one, if the host does not
    /// say).
    pub(super) fn process_mask() -> usize {
        let (mut process, mut system) = (0usize, 0usize);
        // SAFETY: our own process and two outputs.
        let ok = unsafe {
            raw::GetProcessAffinityMask(raw::GetCurrentProcess(), &mut process, &mut system)
        };
        if ok != 0 && process != 0 {
            process
        } else {
            usize::MAX
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This thread's pseudo handle.
    fn me() -> *mut c_void {
        -2isize as *mut c_void
    }

    #[test]
    fn only_ignore_takes_the_calls() {
        assert!(ignore_from(Some("ignore")));
        assert!(ignore_from(Some(" Ignore ")));
        for off in [None, Some(""), Some("0"), Some("1"), Some("passthrough")] {
            assert!(!ignore_from(off), "{off:?}");
        }
    }

    #[test]
    fn the_calls_are_served_for_every_dll_that_exports_them() {
        let module = module();
        for dll in [
            "kernel32.dll",
            "KernelBase.dll",
            "api-ms-win-core-processthreads-l1-1-3.dll",
            "api-ms-win-core-processtopology-l1-1-0.dll",
            "api-ms-win-core-processtopology-obsolete-l1-1-0.dll",
        ] {
            assert!(module.serves(dll), "{dll}");
        }
        assert_eq!(module.exports.len(), 5);
    }

    #[test]
    fn asks_are_told_back_and_leave_the_host_as_it_was() {
        std::thread::spawn(|| {
            // This id may have been another thread's.
            asked().remove(&sys::thread_id(me()).expect("this thread's id"));
            let host = sys::group_affinity(me()).expect("where this thread runs");
            let ideal = sys::ideal(me()).expect("this thread's ideal processor");

            // The first ask is told what the host had, the next what was asked.
            assert_eq!(SetThreadAffinityMask(me(), 0b1), host.mask);
            assert_eq!(SetThreadAffinityMask(me(), 0b10), 0b1);
            let mut told = GroupAffinity::default();
            assert_eq!(GetThreadGroupAffinity(me(), &mut told), 1);
            assert_eq!((told.mask, told.group), (0b10, host.group));
            let wanted = GroupAffinity {
                mask: 0x80,
                ..Default::default()
            };
            let mut before = GroupAffinity::default();
            assert_eq!(SetThreadGroupAffinity(me(), &wanted, &mut before), 1);
            assert_eq!(before.mask, 0b10);
            assert_eq!(sys::group_affinity(me()), Some(host), "never moved");

            assert_eq!(SetThreadIdealProcessor(me(), 3), u32::from(ideal));
            assert_eq!(SetThreadIdealProcessor(me(), MAXIMUM_PROCESSORS), 3);
            let wanted = ProcessorNumber {
                number: 5,
                ..Default::default()
            };
            let mut before = ProcessorNumber::default();
            assert_eq!(SetThreadIdealProcessorEx(me(), &wanted, &mut before), 1);
            assert_eq!(before.number, 3);
            assert_eq!(sys::ideal(me()), Some(ideal), "never moved");

            // What is not remembered gets the host's own answer: no processors
            // (some Windows versions accept an empty group mask), no pointer,
            // no thread.
            assert_eq!(SetThreadAffinityMask(me(), 0), 0);
            assert_eq!(SetThreadAffinityMask(std::ptr::null_mut(), 1), 0);
            for wanted in [&GroupAffinity::default() as *const _, std::ptr::null()] {
                assert_eq!(
                    SetThreadGroupAffinity(me(), wanted, std::ptr::null_mut()),
                    sys::set_group_affinity(me(), wanted, std::ptr::null_mut())
                );
            }
        })
        .join()
        .expect("the thread");
    }
}
