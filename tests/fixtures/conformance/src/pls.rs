//! Persistent local storage.
//!
//! Public reference: reference/system/xpersistentlocalstorage:
//! XPersistentLocalStorageGetPathSize, XPersistentLocalStorageGetPath (size,
//! buffer, used), XPersistentLocalStorageGetSpaceInfo filling
//! XPersistentLocalStorageSpaceInfo (availableFreeBytes, totalFreeBytes,
//! usedBytes, totalBytes). The title writes its own files under the path.
//!
//! The area's ids and slots are VIctory's (`crates/victory-hle/src/modules/
//! storage.rs`). The harness gives the title a 64 MiB quota.

use std::ffi::c_void;

use crate::rt::{Guid, Hresult, S_OK, area};
use crate::tap::Tap;

const PROVIDER: Guid = Guid(
    0xf4fa_f4d4,
    0x2d04,
    0x4fce,
    [0xb3, 0xe0, 0x47, 0x4a, 0x71, 0x3a, 0x3e, 0x84],
);
const IID: Guid = Guid(
    0xd294_11df,
    0x0794,
    0x4553,
    [0x8b, 0x27, 0x95, 0xfc, 0x02, 0xd0, 0xf7, 0x5d],
);

/// XPersistentLocalStorageSpaceInfo.
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
struct SpaceInfo {
    available_free: u64,
    total_free: u64,
    used: u64,
    total: u64,
}

pub fn run(t: &mut Tap) {
    let o = match area(PROVIDER, IID) {
        Ok(o) => o,
        Err(hr) => {
            t.check(
                false,
                "pls: the persistent local storage area is served",
                || format!("QueryApiImpl {hr:#010x}"),
            );
            return;
        }
    };
    // SAFETY: (all slots) the signatures in VIctory's table.
    let size_of: extern "system" fn(*mut c_void, *mut usize) -> Hresult = unsafe { o.method(0x18) };
    let path_of: extern "system" fn(*mut c_void, usize, *mut u8, *mut usize) -> Hresult =
        unsafe { o.method(0x20) };
    let space_of: extern "system" fn(*mut c_void, *mut SpaceInfo) -> Hresult =
        unsafe { o.method(0x28) };
    let mut size = 0;
    t.eq(
        size_of(o.this(), &mut size),
        S_OK,
        "pls: XPersistentLocalStorageGetPathSize succeeds",
    );
    let mut buffer = vec![0u8; size.max(1)];
    let mut used = 0;
    let hr = path_of(o.this(), buffer.len(), buffer.as_mut_ptr(), &mut used);
    t.check(
        hr == S_OK && used == size && buffer.last() == Some(&0),
        "pls: XPersistentLocalStorageGetPath fills exactly the size it gave, NUL included",
        || format!("{hr:#010x} used {used} size {size}"),
    );
    let path = String::from_utf8_lossy(&buffer[..buffer.len().saturating_sub(1)]).into_owned();
    let mut before = SpaceInfo::default();
    t.eq(
        space_of(o.this(), &mut before),
        S_OK,
        "pls: XPersistentLocalStorageGetSpaceInfo succeeds",
    );
    t.check(
        before.available_free <= before.total_free
            && before.total_free <= before.total
            && before.used <= before.total
            && before.total > 0,
        "pls: the space figures are consistent (available <= free <= total, used <= total)",
        || format!("{before:?}"),
    );
    t.check(
        before.total <= 64 << 20,
        "pls: the total is at most the title's declared quota",
        || format!("{before:?}"),
    );

    // A file the title writes under its path is there, and counts.
    let file = std::path::Path::new(&path).join("conformance.bin");
    let written = std::fs::write(&file, vec![0x5au8; 1 << 20]);
    t.check(
        written.is_ok(),
        "pls: the title writes a file under its storage path",
        || format!("{path:?}: {written:?}"),
    );
    let back = std::fs::read(&file).map(|b| b.len());
    t.eq(back.ok(), Some(1 << 20), "pls: ... and reads it back");
    let mut after = SpaceInfo::default();
    space_of(o.this(), &mut after);
    t.check(
        after.used >= before.used + (1 << 20)
            && after.total_free + (1 << 20) <= before.total_free + 4096,
        "pls: a 1 MiB file adds to the used bytes and takes from the free ones",
        || format!("before {before:?}, after {after:?}"),
    );
    let _ = std::fs::remove_file(&file);
}
