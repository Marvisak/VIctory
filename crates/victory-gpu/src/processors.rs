//! Where helper threads of ours ask to run, on a processor of two halves with
//! a last-level cache each (`VICTORY_CCD_SPLIT=1`; off when unset or `0`).
//!
//! On a Ryzen with two chiplets the helpers the frame does not wait for
//! (disk decompression, releases) ask for the processors of the upper cache,
//! which leaves the lower one to the title's threads. A selected processor
//! set is a preference of the scheduler's, not a hard affinity. On any other
//! processor nothing is asked.
//!
//! Telling the halves apart by their caches follows TaynanGT/Vice-Tay
//! 4c3f311a (GPL-3.0).

mod topology;

use std::sync::OnceLock;

use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetSystemCpuSetInformation, RelationAll,
    SYSTEM_CPU_SET_INFORMATION,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, SetThreadSelectedCpuSets,
};

use topology::{Halves, Mask, Topology, sets_in};

/// The cores and last-level caches of this machine.
fn machine() -> Topology {
    let mut bytes = 0u32;
    // SAFETY: a size query.
    unsafe { GetLogicalProcessorInformationEx(RelationAll, std::ptr::null_mut(), &mut bytes) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(RelationAll, buffer.as_mut_ptr().cast(), &mut bytes)
    };
    if ok == 0 {
        return Topology::default();
    }
    let len = (bytes as usize).min(8 * buffer.len());
    // SAFETY: the bytes the call wrote, inside the buffer.
    Topology::parse(unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), len) })
}

/// The system's processor sets of group 0: each one's id and its logical
/// processor.
fn cpu_sets() -> Vec<(u32, u8)> {
    let mut bytes = 0u32;
    // SAFETY: a size query.
    unsafe {
        GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0)
    };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe {
        GetSystemCpuSetInformation(
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
            GetCurrentProcess(),
            0,
        )
    };
    let mut sets = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe {
            std::ptr::read_unaligned(
                buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(at)
                    .cast::<SYSTEM_CPU_SET_INFORMATION>(),
            )
        };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { info.Anonymous.CpuSet };
        if set.Group == 0 {
            sets.push((set.Id, set.LogicalProcessorIndex));
        }
        at += info.Size as usize;
    }
    sets
}

/// Selects the processor sets of `mask` for the calling thread. True if the
/// system took them.
fn select(mask: Mask) -> bool {
    let ids = sets_in(mask, &cpu_sets());
    // SAFETY: our own thread and a list of the system's own set ids.
    !ids.is_empty()
        && unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), ids.as_ptr(), ids.len() as u32) }
            != 0
}

/// The halves, if `VICTORY_CCD_SPLIT` asks for them and this processor has
/// them; asked and logged once.
fn halves() -> Option<Halves> {
    static HALVES: OnceLock<Option<Halves>> = OnceLock::new();
    *HALVES.get_or_init(|| {
        if !crate::switch::on("VICTORY_CCD_SPLIT") {
            return None;
        }
        let machine = machine();
        let halves = machine.halves();
        match halves {
            Some(h) => tracing::info!(
                main = format_args!("{:#x}", h.main),
                helpers = format_args!("{:#x}", h.helpers),
                "VICTORY_CCD_SPLIT: helper threads of ours ask for the processors of the upper cache"
            ),
            None => tracing::info!(
                cores = machine.cores.len(),
                caches = machine.l3.len(),
                "VICTORY_CCD_SPLIT asks nothing here: not a processor of like cores under several last-level caches"
            ),
        }
        halves
    })
}

/// Called by a helper thread the frame does not wait for, on itself: with
/// `VICTORY_CCD_SPLIT=1` on a processor of two halves it asks for the upper
/// one. Does nothing otherwise.
pub fn keep_helper_apart(what: &str) {
    let Some(halves) = halves() else {
        return;
    };
    if !select(halves.helpers) {
        tracing::warn!(
            what,
            error = %std::io::Error::last_os_error(),
            "VICTORY_CCD_SPLIT: the thread could not be moved"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::GetThreadSelectedCpuSets;

    /// The system's own answer, on this processor: run by hand on a Ryzen
    /// with two chiplets with
    /// `cargo test -p victory-gpu processors -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a processor of two halves with a last-level cache each"]
    fn this_machines_upper_half_is_selected() {
        let machine = machine();
        let halves = machine.halves().expect("two halves");
        println!(
            "{} cores, caches {:x?}, helpers {:#x}",
            machine.cores.len(),
            machine.l3,
            halves.helpers
        );
        std::thread::spawn(move || {
            assert!(select(halves.helpers));
            let mut ids = [0u32; 64];
            let mut count = 0;
            // SAFETY: our own thread and room for 64 ids.
            let ok = unsafe {
                GetThreadSelectedCpuSets(GetCurrentThread(), ids.as_mut_ptr(), 64, &mut count)
            };
            assert_ne!(ok, 0);
            let mut selected = ids[..count as usize].to_vec();
            selected.sort_unstable();
            let mut wanted = sets_in(halves.helpers, &cpu_sets());
            wanted.sort_unstable();
            assert_eq!(selected, wanted);
        })
        .join()
        .expect("the thread");
    }
}
