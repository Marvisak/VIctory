//! Persistent local storage: a host folder of the title's own
//! (`runtime::Title::storage`, under the data directory, never the cache)
//! with the quota its manifest declares, seen by the title as a drive
//! (`files`).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::path::Path;

use super::xasync::{self, AsyncBlock, E_ABORT};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, NOT_SUPPORTED, Object, S_OK, add_ref, missing, put,
    put_string,
};
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0xf4fa_f4d4,
    0x2d04,
    0x4fce,
    [0xb3, 0xe0, 0x47, 0x4a, 0x71, 0x3a, 0x3e, 0x84],
);
pub const IID: Guid = Guid::new(
    0xd294_11df,
    0x0794,
    0x4553,
    [0x8b, 0x27, 0x95, 0xfc, 0x02, 0xd0, 0xf7, 0x5d],
);

/// As Grand Theft Auto VI (2026) asks for it; it calls 0x20 (path) and 0x28
/// (space) with the same arguments.
pub const IID_2026: Guid = Guid::new(
    0x41a4_e10c,
    0x5a7e,
    0x41d9,
    [0x8c, 0x37, 0x37, 0xbd, 0xe6, 0x2a, 0x07, 0xd6],
);

pub static OBJECT: Object = Object::new("persistent local storage", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT), (PROVIDER, IID_2026, &OBJECT)];

const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;

type This = *const Object;

/// The path titles get: the root of a virtual drive over the folder. On the
/// console storage is a drive of its own, and titles rely on it: Minecraft
/// keeps only what precedes the path's last `:\` and mounts that as a
/// drive, so a host folder such as `C:\Users\..` would make it write to the
/// root of `C:`. None (the path calls fail) before the title is set.
fn folder() -> Option<String> {
    let storage = runtime::title().storage;
    if storage.as_os_str().is_empty() {
        return None;
    }
    #[cfg(windows)]
    return super::files::mount(&storage).map(|letter| format!("{letter}:\\"));
    #[cfg(not(windows))]
    None
}

extern "system" fn path_size(_: This, out: *mut usize) -> HRESULT {
    match folder() {
        Some(path) => put(out, path.len() + 1),
        None => NOT_SUPPORTED,
    }
}

/// A buffer too small for the path fails with `E_FAIL`, the documented
/// Xbox result (a PC gets `ERROR_INSUFFICIENT_BUFFER`).
extern "system" fn path(_: This, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    match folder() {
        Some(path) if size <= path.len() => E_FAIL,
        Some(path) => put_string(&path, size, buffer, used),
        None => NOT_SUPPORTED,
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceInfo {
    /// Writable now.
    pub available_free: u64,
    /// Left in the quota.
    pub total_free: u64,
    pub used: u64,
    /// The quota.
    pub total: u64,
}

/// Bytes of the real files under `dir`. Junctions and other links are
/// skipped rather than followed: a link into its own parent would loop a
/// naive walk (the old recursion would overflow the stack on it), and one
/// pointing elsewhere would charge foreign bytes to the title's quota.
fn used_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                pending.push(e.path());
            } else if let Ok(meta) = e.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// What the host disk under `dir` has free (unlimited if unknown).
pub(crate) fn disk_free(dir: &Path) -> u64 {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0;
        let null = std::ptr::null_mut();
        // SAFETY: a NUL-terminated path and a valid output.
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                null,
                null,
            )
        };
        if ok != 0 {
            return free;
        }
    }
    let _ = dir;
    u64::MAX
}

fn space() -> SpaceInfo {
    let title = runtime::title();
    let used = used_bytes(&title.storage);
    let free = title.storage_bytes.saturating_sub(used);
    SpaceInfo {
        // Writable now: the quota's rest, as far as the host disk allows.
        available_free: free.min(disk_free(&title.storage)),
        total_free: free,
        used,
        total: title.storage_bytes,
    }
}

extern "system" fn space_info(_: This, out: *mut SpaceInfo) -> HRESULT {
    put(out, space())
}

/// Tells our prompts apart from other calls on the same block.
static PROMPT: u8 = 0;

/// There is nobody to ask: granted when the quota has room, else declined.
extern "system" fn prompt_for_space(_: This, bytes: u64, block: *mut AsyncBlock) -> HRESULT {
    let hr = if bytes <= space().total_free {
        S_OK
    } else {
        E_ABORT
    };
    xasync::finished(block, (&raw const PROMPT).cast(), hr)
}

extern "system" fn prompt_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const PROMPT).cast(), 0, none, none.cast())
}

static VTABLE: Methods<8> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    path_size as *const (),        // 0x18
    path as *const (),             // 0x20
    space_info as *const (),       // 0x28
    prompt_for_space as *const (), // 0x30
    prompt_result as *const (),    // 0x38
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    fn serves_a_drive_over_the_title_folder_and_its_quota() {
        type Size = extern "system" fn(This, *mut usize) -> HRESULT;
        type Get = extern "system" fn(This, usize, *mut u8, *mut usize) -> HRESULT;
        type Space = extern "system" fn(This, *mut SpaceInfo) -> HRESULT;
        type Prompt = extern "system" fn(This, u64, *mut AsyncBlock) -> HRESULT;
        type Result = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        let this: This = &OBJECT;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("save.dat"), [0u8; 100]).unwrap();
        runtime::set_title(runtime::Title {
            id: 1,
            storage: dir.path().to_owned(),
            storage_bytes: 1000,
            ..Default::default()
        });

        // The path is a drive root, never the host folder.
        let mut size = 0;
        assert_eq!(method::<Size>(&OBJECT, 0x18)(this, &mut size), S_OK);
        assert_eq!(size, 4, "X:\\ and its NUL");
        let (mut buffer, mut used) = ([0u8; 4], 0);
        let get = method::<Get>(&OBJECT, 0x20);
        assert_eq!(get(this, 4, buffer.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(get(this, 3, buffer.as_mut_ptr(), &mut used), E_FAIL);
        let drive = std::str::from_utf8(&buffer[..3]).unwrap();
        assert!(drive.ends_with(":\\"), "{drive}");
        // Title file calls on it reach the folder.
        let path = format!("{drive}save.dat\0");
        let mut attributes = [0u8; 36];
        let get_attributes = super::super::files::GetFileAttributesExW;
        let wide: Vec<u16> = path.encode_utf16().collect();
        assert_ne!(
            get_attributes(wide.as_ptr(), 0, attributes.as_mut_ptr().cast()),
            0
        );

        let mut info = SpaceInfo {
            available_free: 0,
            total_free: 0,
            used: 0,
            total: 0,
        };
        assert_eq!(method::<Space>(&OBJECT, 0x28)(this, &mut info), S_OK);
        assert_eq!((info.used, info.total_free, info.total), (100, 900, 1000));

        let prompt = method::<Prompt>(&OBJECT, 0x30);
        let result = method::<Result>(&OBJECT, 0x38);
        for (bytes, expect) in [(900, S_OK), (901, E_ABORT)] {
            let mut block = AsyncBlock {
                queue: std::ptr::null(),
                context: std::ptr::null_mut(),
                callback: None,
                internal: [0; 4],
            };
            assert_eq!(prompt(this, bytes, &mut block), S_OK);
            assert_eq!(xasync::XAsyncGetStatus(&mut block, true), expect);
            assert_eq!(result(this, &mut block), expect, "{bytes} bytes");
        }
    }

    /// A junction into the tree itself or out of it must neither hang or
    /// crash the quota count nor charge foreign bytes to the title.
    #[cfg(windows)]
    #[test]
    fn used_bytes_skips_junctions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.dat"), [0u8; 10]).unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.dat"), [0u8; 5]).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("c.dat"), [0u8; 100]).unwrap();
        let junction = |name: &str, target: &Path| make_junction(&dir.path().join(name), target);
        junction("cycle", dir.path());
        junction("escape", outside.path());

        // Only the two real files count; neither junction is walked.
        assert_eq!(used_bytes(dir.path()), 15);
    }

    /// Makes `link` a junction (mount-point reparse point) to `target`
    /// through the reparse-point API directly — what `cmd /c mklink /J`
    /// does, without shelling out to `cmd`.
    #[cfg(windows)]
    fn make_junction(link: &Path, target: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
        use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_MOUNT_POINT;

        std::fs::create_dir(link).unwrap();
        let wide: Vec<u16> = link.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string that
        // outlives the call; the returned handle is checked below.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE, "opening {link:?}");

        // A mount point names its target as an NT path ("\??\C:\...").
        let target = target.canonicalize().unwrap();
        let target = target.to_str().unwrap();
        let target = target.strip_prefix(r"\\?\").unwrap_or(target);
        let substitute: Vec<u16> = format!(r"\??\{target}").encode_utf16().collect();
        let print: Vec<u16> = target.encode_utf16().collect();
        let substitute_bytes = (substitute.len() * 2) as u16;
        let print_bytes = (print.len() * 2) as u16;

        // REPARSE_DATA_BUFFER for a mount point: tag, length, reserved,
        // then the substitute/print name header followed by both names,
        // each NUL-terminated.
        let data_len = 8 + substitute_bytes + 2 + print_bytes + 2;
        let mut buffer = vec![0u8; 8 + data_len as usize];
        buffer[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buffer[4..6].copy_from_slice(&data_len.to_le_bytes());
        buffer[10..12].copy_from_slice(&substitute_bytes.to_le_bytes());
        buffer[12..14].copy_from_slice(&(substitute_bytes + 2).to_le_bytes());
        buffer[14..16].copy_from_slice(&print_bytes.to_le_bytes());
        let mut at = 16;
        for unit in substitute.iter().copied().chain([0]).chain(print.iter().copied().chain([0]))
        {
            buffer[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            at += 2;
        }

        let mut returned = 0;
        // SAFETY: `handle` is the live directory handle from above;
        // `buffer` points to `buffer.len()` readable bytes holding a
        // well-formed mount-point REPARSE_DATA_BUFFER.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: `handle` is still open and is closed exactly once.
        unsafe { CloseHandle(handle) };
        assert_ne!(ok, 0, "junction {link:?} -> {target:?}");
    }
}
