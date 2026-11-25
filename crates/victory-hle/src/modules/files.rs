//! Virtual drives over host folders: on the console, title storage is a
//! drive of its own, and titles build paths on that assumption.
//!
//! Every title import that takes a path is bound here. A path on a virtual
//! drive (`X:\..`, `X:/..`, `X:`, `\\?\X:\..`) is rewritten to the host
//! folder behind it and handed to the host's own function; any other path
//! goes to the host unchanged. A rewritten narrow path goes to the host's
//! wide twin, so host folders outside the ANSI code page work; never to our
//! wide wrapper, which would rewrite the host path a second time when its
//! drive letter is mounted too. Letters are chosen among those the host
//! does not use, so no host drive is hidden.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::ffi::{CStr, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

use crate::{HleExport, HleModule, hle_export};

/// The host's own function: what the import would have been bound to.
macro_rules! host {
    ($dll:literal, $name:literal: $ty:ty) => {{
        static ADDRESS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let address = *ADDRESS.get_or_init(|| {
            crate::host::function($dll, $name)
                .unwrap_or_else(|| panic!("the host lacks {}!{}", $dll, $name))
        });
        // SAFETY: the host exports this function with this signature.
        unsafe { std::mem::transmute::<usize, $ty>(address) }
    }};
}
pub(crate) use host;

/// Mounted drives: upper-case letter and host root (UTF-16, no trailing
/// separator, no NUL).
static DRIVES: Mutex<Vec<(u16, Vec<u16>)>> = Mutex::new(Vec::new());

fn drives() -> MutexGuard<'static, Vec<(u16, Vec<u16>)>> {
    DRIVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Gives the host folder `root` a drive letter (the same one if it already
/// has one): the highest letter neither the host nor another mount uses.
pub fn mount(root: &Path) -> Option<char> {
    let wide = root_wide(root);
    let mut drives = drives();
    if let Some(&(letter, _)) = drives.iter().find(|(_, r)| *r == wide) {
        return char::from_u32(letter.into());
    }
    // SAFETY: no arguments.
    let host = unsafe { GetLogicalDrives() };
    let letter = (b'D'..=b'Z').rev().find(|&l| {
        host >> (l - b'A') & 1 == 0 && !drives.iter().any(|(m, _)| *m == u16::from(l))
    })?;
    tracing::info!(drive = %char::from(letter), root = %root.display(), "virtual drive");
    drives.push((letter.into(), wide));
    Some(letter.into())
}

/// Gives the host folder `root` a drive letter the console fixes (titles
/// open their package as `G:\..` and temporary files as `T:\..`). A host
/// drive of that letter is hidden from the title.
pub fn mount_at(letter: char, root: &Path) {
    let letter = letter.to_ascii_uppercase();
    let Some(index) = u32::from(letter)
        .checked_sub(u32::from('A'))
        .filter(|i| *i < 26)
    else {
        return;
    };
    // SAFETY: no arguments.
    let hides_host_drive = unsafe { GetLogicalDrives() } >> index & 1 != 0;
    tracing::info!(drive = %letter, root = %root.display(), hides_host_drive, "virtual drive");
    let mut drives = drives();
    drives.retain(|(l, _)| u32::from(*l) != u32::from(letter));
    drives.push((letter as u16, root_wide(root)));
}

/// `root` as UTF-16, absolute, without trailing separators (a drive root
/// keeps its own). An empty root is the current directory, as it is to a
/// path joined to it: it is the title's folder when the title is run by its
/// file name alone (`victory run GTA6.exe`).
fn root_wide(root: &Path) -> Vec<u16> {
    let root = if root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        root
    };
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_owned());
    let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
    while wide.len() > 3 && wide.last().is_some_and(|c| SEPARATORS.contains(c)) {
        wide.pop();
    }
    wide
}

const SEPARATORS: [u16; 2] = [b'\\' as u16, b'/' as u16];
const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// Host paths from this length on take the `\\?\` form, which lifts the
/// host's MAX_PATH limit (248 characters for a directory, 260 for a file).
const LONG: usize = 248;

/// `root` and the title's `tail` as a `\\?\` path. That form turns off the
/// host's own clean-up, so `/` becomes `\` and `.` and `..` are resolved
/// here (`..` stops at the root).
fn verbatim(root: &[u16], tail: &[u16]) -> Vec<u16> {
    let backslash = u16::from(b'\\');
    let mut out = if root.starts_with(&VERBATIM) {
        root.to_vec()
    } else if let Some(unc) = root.strip_prefix(&[backslash; 2][..]) {
        let mut out = VERBATIM.to_vec();
        out.extend("UNC\\".encode_utf16());
        out.extend_from_slice(unc);
        out
    } else {
        [&VERBATIM[..], root].concat()
    };
    while out.last() == Some(&backslash) {
        out.pop();
    }
    let dot = u16::from(b'.');
    let mut parts: Vec<&[u16]> = Vec::new();
    for part in tail.split(|c| SEPARATORS.contains(c)) {
        match part {
            [] => {}
            [d] if *d == dot => {}
            [d, e] if [*d, *e] == [dot; 2] => drop(parts.pop()),
            _ => parts.push(part),
        }
    }
    for part in parts {
        out.push(backslash);
        out.extend_from_slice(part);
    }
    out
}

/// The host path, NUL-terminated, for `path` (without its NUL) if it is on
/// a virtual drive.
pub(crate) fn translate(path: &[u16]) -> Option<Vec<u16>> {
    let (prefix, rest) = match path.strip_prefix(&VERBATIM[..]) {
        Some(rest) => (&VERBATIM[..], rest),
        None => (&[][..], path),
    };
    let &[letter, colon, ..] = rest else {
        return None;
    };
    if colon != u16::from(b':') {
        return None;
    }
    let letter = u8::try_from(letter).ok().filter(u8::is_ascii_alphabetic)?;
    let letter = u16::from(letter.to_ascii_uppercase());
    let drives = drives();
    let (_, root) = drives.iter().find(|(l, _)| *l == letter)?;
    let tail = &rest[2..];
    if prefix.is_empty() && root.len() + tail.len() >= LONG {
        let mut out = verbatim(root, tail);
        out.push(0);
        return Some(out);
    }
    let mut out = prefix.to_vec();
    out.extend_from_slice(root);
    // `X:` and `X:name` are relative to the root.
    if !tail.first().is_some_and(|c| SEPARATORS.contains(c)) {
        out.push(b'\\'.into());
    }
    let verbatim = !prefix.is_empty();
    out.extend(tail.iter().map(|&c| {
        if verbatim && c == u16::from(b'/') {
            b'\\'.into()
        } else {
            c
        }
    }));
    out.push(0);
    Some(out)
}

/// A title's wide path: the host path when it is on a virtual drive, else
/// the title's own pointer.
struct Wide(*const u16, Option<Vec<u16>>);

impl Wide {
    fn ptr(&self) -> *const u16 {
        self.1.as_ref().map_or(self.0, |p| p.as_ptr())
    }
}

fn wide(path: *const u16) -> Wide {
    if path.is_null() {
        return Wide(path, None);
    }
    // SAFETY: the title passes a NUL-terminated path.
    let path_slice = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        std::slice::from_raw_parts(path, len)
    };
    let host = translate(path_slice);
    trace(path_slice, host.as_deref());
    Wide(path, host)
}

/// Every title path at debug level, with the host path if rewritten.
fn trace(path: &[u16], host: Option<&[u16]>) {
    tracing::debug!(
        path = %String::from_utf16_lossy(path),
        host = host.map(|h| String::from_utf16_lossy(&h[..h.len() - 1])),
        "title path"
    );
}

fn acp_to_wide(bytes: &[u8]) -> Option<Vec<u16>> {
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: sizing call, then a call with room for `n` units.
    unsafe {
        let n = MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0);
        let count = usize::try_from(n).ok().filter(|&c| c > 0)?;
        let mut wide = vec![0u16; count];
        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), n);
        Some(wide)
    }
}

fn widen_ansi(path: *const c_char) -> Option<Vec<u16>> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    let mut wide = acp_to_wide(bytes)?;
    wide.push(0);
    Some(wide)
}

/// A narrow path (ANSI code page, as the host decodes it) on a virtual
/// drive, as a wide host path.
fn narrow(path: *const c_char) -> Option<Vec<u16>> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    // The drive part is ASCII in every code page; decode only candidates.
    match bytes.strip_prefix(br"\\?\").unwrap_or(bytes) {
        [letter, b':', ..] if letter.is_ascii_alphabetic() => {}
        _ => {
            tracing::debug!(path = %String::from_utf8_lossy(bytes), "title path");
            return None;
        }
    }
    let wide = acp_to_wide(bytes)?;
    let host = translate(&wide);
    trace(&wide, host.as_deref());
    host
}

struct WidePath(Option<Vec<u16>>);

impl WidePath {
    fn ptr(&self) -> *const u16 {
        self.0.as_deref().map_or(std::ptr::null(), <[u16]>::as_ptr)
    }
}

/// Rewrites a `(from, to)` narrow path pair whenever at least one path is on
/// a virtual drive, widening any non-virtual partner so both can go to `*W`.
fn narrow_pair(from: *const c_char, to: *const c_char) -> Option<(WidePath, WidePath)> {
    match (narrow(from), narrow(to)) {
        (None, None) => None,
        (f, t) => Some((
            WidePath(f.or_else(|| widen_ansi(from))),
            WidePath(t.or_else(|| widen_ansi(to))),
        )),
    }
}

/// An ASCII mode string (`"rb"`, `"w, ccs=UTF-8"`) for a wide CRT call.
fn widen(mode: *const c_char) -> Vec<u16> {
    if mode.is_null() {
        return vec![0];
    }
    // SAFETY: a NUL-terminated mode string.
    let bytes = unsafe { CStr::from_ptr(mode) }.to_bytes();
    bytes.iter().map(|&b| u16::from(b)).chain([0]).collect()
}

type Handle = *mut c_void;
type File = *mut c_void;

// kernel32 and its file API sets.

pub extern "system" fn CreateFileW(
    path: *const u16,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
        p.ptr(),
        access,
        share,
        security,
        disposition,
        flags,
        template,
    )
}

pub extern "system" fn CreateFileA(
    path: *const c_char,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                p.as_ptr(),
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
        None => {
            host!("kernel32.dll", "CreateFileA": extern "system" fn(*const c_char, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                path,
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
    }
}

/// Used by Bink.
pub extern "system" fn CreateFile2(
    path: *const u16,
    access: u32,
    share: u32,
    disposition: u32,
    params: *const c_void,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "CreateFile2": extern "system" fn(*const u16, u32, u32, u32, *const c_void) -> Handle)(
        p.ptr(),
        access,
        share,
        disposition,
        params,
    )
}

pub extern "system" fn DeleteFileA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "DeleteFileW": extern "system" fn(*const u16) -> i32)(p.as_ptr())
        }
        None => {
            host!("kernel32.dll", "DeleteFileA": extern "system" fn(*const c_char) -> i32)(path)
        }
    }
}

pub extern "system" fn DeleteFileW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "DeleteFileW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn RemoveDirectoryA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "RemoveDirectoryW": extern "system" fn(*const u16) -> i32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "RemoveDirectoryA": extern "system" fn(*const c_char) -> i32)(
                path,
            )
        }
    }
}

pub extern "system" fn RemoveDirectoryW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "RemoveDirectoryW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn SetFileAttributesA(path: *const c_char, attributes: u32) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "SetFileAttributesW": extern "system" fn(*const u16, u32) -> i32)(
                p.as_ptr(),
                attributes,
            )
        }
        None => {
            host!("kernel32.dll", "SetFileAttributesA": extern "system" fn(*const c_char, u32) -> i32)(
                path, attributes,
            )
        }
    }
}

pub extern "system" fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "SetFileAttributesW": extern "system" fn(*const u16, u32) -> i32)(
        p.ptr(),
        attributes,
    )
}

pub extern "system" fn GetFileAttributesA(path: *const c_char) -> u32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "GetFileAttributesA": extern "system" fn(*const c_char) -> u32)(
                path,
            )
        }
    }
}

pub extern "system" fn CreateDirectoryA(path: *const c_char, security: *const c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "CreateDirectoryW": extern "system" fn(*const u16, *const c_void) -> i32)(
                p.as_ptr(),
                security,
            )
        }
        None => {
            host!("kernel32.dll", "CreateDirectoryA": extern "system" fn(*const c_char, *const c_void) -> i32)(
                path, security,
            )
        }
    }
}

pub extern "system" fn CreateDirectoryW(path: *const u16, security: *const c_void) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "CreateDirectoryW": extern "system" fn(*const u16, *const c_void) -> i32)(
        p.ptr(),
        security,
    )
}

pub extern "system" fn GetFileAttributesExA(
    path: *const c_char,
    level: i32,
    info: *mut c_void,
) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "GetFileAttributesExW": extern "system" fn(*const u16, i32, *mut c_void) -> i32)(
                p.as_ptr(),
                level,
                info,
            )
        }
        None => {
            host!("kernel32.dll", "GetFileAttributesExA": extern "system" fn(*const c_char, i32, *mut c_void) -> i32)(
                path, level, info,
            )
        }
    }
}

pub extern "system" fn GetFileAttributesExW(
    path: *const u16,
    level: i32,
    info: *mut c_void,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesExW": extern "system" fn(*const u16, i32, *mut c_void) -> i32)(
        p.ptr(),
        level,
        info,
    )
}

pub extern "system" fn GetFileAttributesW(path: *const u16) -> u32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(p.ptr())
}

pub extern "system" fn GetDiskFreeSpaceExA(
    path: *const c_char,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "GetDiskFreeSpaceExW": extern "system" fn(*const u16, *mut u64, *mut u64, *mut u64) -> i32)(
                p.as_ptr(),
                available,
                total,
                free,
            )
        }
        None => {
            host!("kernel32.dll", "GetDiskFreeSpaceExA": extern "system" fn(*const c_char, *mut u64, *mut u64, *mut u64) -> i32)(
                path, available, total, free,
            )
        }
    }
}

pub extern "system" fn GetDiskFreeSpaceExW(
    path: *const u16,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetDiskFreeSpaceExW": extern "system" fn(*const u16, *mut u64, *mut u64, *mut u64) -> i32)(
        p.ptr(),
        available,
        total,
        free,
    )
}

/// Wants a volume root: a virtual drive answers for the host volume its
/// folder is on.
pub extern "system" fn GetDiskFreeSpaceW(
    root: *const u16,
    sectors_per_cluster: *mut u32,
    bytes_per_sector: *mut u32,
    free_clusters: *mut u32,
    clusters: *mut u32,
) -> i32 {
    let mut p = wide(root);
    if let Some(host) = &mut p.1
        && let [letter, colon, sep, ..] = host[..]
        && colon == u16::from(b':')
        && SEPARATORS.contains(&sep)
    {
        *host = vec![letter, colon, sep, 0];
    }
    host!("kernel32.dll", "GetDiskFreeSpaceW": extern "system" fn(*const u16, *mut u32, *mut u32, *mut u32, *mut u32) -> i32)(
        p.ptr(),
        sectors_per_cluster,
        bytes_per_sector,
        free_clusters,
        clusters,
    )
}

pub extern "system" fn FindFirstFileW(path: *const u16, data: *mut c_void) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileW": extern "system" fn(*const u16, *mut c_void) -> Handle)(
        p.ptr(),
        data,
    )
}

pub extern "system" fn FindFirstFileExW(
    path: *const u16,
    level: i32,
    data: *mut c_void,
    search: i32,
    filter: *mut c_void,
    flags: u32,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileExW": extern "system" fn(*const u16, i32, *mut c_void, i32, *mut c_void, u32) -> Handle)(
        p.ptr(),
        level,
        data,
        search,
        filter,
        flags,
    )
}

pub extern "system" fn MoveFileA(from: *const c_char, to: *const c_char) -> i32 {
    match narrow_pair(from, to) {
        Some((from, to)) => {
            host!("kernel32.dll", "MoveFileW": extern "system" fn(*const u16, *const u16) -> i32)(
                from.ptr(),
                to.ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "MoveFileA": extern "system" fn(*const c_char, *const c_char) -> i32)(
                from, to,
            )
        }
    }
}

pub extern "system" fn MoveFileW(from: *const u16, to: *const u16) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "MoveFileW": extern "system" fn(*const u16, *const u16) -> i32)(
        from.ptr(),
        to.ptr(),
    )
}

pub extern "system" fn MoveFileExA(from: *const c_char, to: *const c_char, flags: u32) -> i32 {
    match narrow_pair(from, to) {
        Some((from, to)) => {
            host!("kernel32.dll", "MoveFileExW": extern "system" fn(*const u16, *const u16, u32) -> i32)(
                from.ptr(),
                to.ptr(),
                flags,
            )
        }
        None => {
            host!("kernel32.dll", "MoveFileExA": extern "system" fn(*const c_char, *const c_char, u32) -> i32)(
                from, to, flags,
            )
        }
    }
}

pub extern "system" fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "MoveFileExW": extern "system" fn(*const u16, *const u16, u32) -> i32)(
        from.ptr(),
        to.ptr(),
        flags,
    )
}

pub extern "system" fn CopyFileA(
    from: *const c_char,
    to: *const c_char,
    fail_if_exists: i32,
) -> i32 {
    match narrow_pair(from, to) {
        Some((from, to)) => {
            host!("kernel32.dll", "CopyFileW": extern "system" fn(*const u16, *const u16, i32) -> i32)(
                from.ptr(),
                to.ptr(),
                fail_if_exists,
            )
        }
        None => {
            host!("kernel32.dll", "CopyFileA": extern "system" fn(*const c_char, *const c_char, i32) -> i32)(
                from,
                to,
                fail_if_exists,
            )
        }
    }
}

pub extern "system" fn CopyFileW(from: *const u16, to: *const u16, fail_if_exists: i32) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "CopyFileW": extern "system" fn(*const u16, *const u16, i32) -> i32)(
        from.ptr(),
        to.ptr(),
        fail_if_exists,
    )
}

pub extern "system" fn SetCurrentDirectoryW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "SetCurrentDirectoryW": extern "system" fn(*const u16) -> i32)(p.ptr())
}

pub extern "system" fn SetCurrentDirectoryA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "SetCurrentDirectoryW": extern "system" fn(*const u16) -> i32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "SetCurrentDirectoryA": extern "system" fn(*const c_char) -> i32)(
                path,
            )
        }
    }
}

// The C runtime (ucrtbase, which the title's CRT imports bind to).

pub extern "C" fn _wfopen(path: *const u16, mode: *const u16) -> File {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfopen": extern "C" fn(*const u16, *const u16) -> File)(p.ptr(), mode)
}

pub extern "C" fn _wfopen_s(out: *mut File, path: *const u16, mode: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfopen_s": extern "C" fn(*mut File, *const u16, *const u16) -> i32)(
        out,
        p.ptr(),
        mode,
    )
}

pub extern "C" fn _wfsopen(path: *const u16, mode: *const u16, share: i32) -> File {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfsopen": extern "C" fn(*const u16, *const u16, i32) -> File)(
        p.ptr(),
        mode,
        share,
    )
}

// Kernel reads into title memory. The GPU side leaves blocks of title GPU
// buffers read-only to see the title's next write, and the kernel's writes
// fail there instead of faulting into its handler; Grand Theft Auto VI's
// synchronous reads (title+0x313fc0) read straight into 4 KiB-aligned
// destinations and retry a failed `ReadFile` forever. So the destination
// is opened first.

pub extern "system" fn ReadFile(
    file: *mut c_void,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    let _writing = victory_gpu::kernel_will_write(buffer as usize, len as usize);
    host!("kernel32.dll", "ReadFile": extern "system" fn(*mut c_void, *mut u8, u32, *mut u32, *mut c_void) -> i32)(
        file, buffer, len, read, overlapped,
    )
}

/// The host CRT reads large requests straight into the caller's buffer.
pub extern "C" fn fread(buffer: *mut u8, size: usize, count: usize, stream: File) -> usize {
    let _writing = victory_gpu::kernel_will_write(buffer as usize, size.saturating_mul(count));
    host!("ucrtbase.dll", "fread": extern "C" fn(*mut u8, usize, usize, File) -> usize)(
        buffer, size, count, stream,
    )
}

pub extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> File {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wfopen": extern "C" fn(*const u16, *const u16) -> File)(
            p.as_ptr(),
            widen(mode).as_ptr(),
        ),
        None => {
            host!("ucrtbase.dll", "fopen": extern "C" fn(*const c_char, *const c_char) -> File)(
                path, mode,
            )
        }
    }
}

pub extern "C" fn fopen_s(out: *mut File, path: *const c_char, mode: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wfopen_s": extern "C" fn(*mut File, *const u16, *const u16) -> i32)(
                out,
                p.as_ptr(),
                widen(mode).as_ptr(),
            )
        }
        None => {
            host!("ucrtbase.dll", "fopen_s": extern "C" fn(*mut File, *const c_char, *const c_char) -> i32)(
                out, path, mode,
            )
        }
    }
}

/// `_open(path, flags[, mode])` is variadic; on x64 the optional integer
/// travels in the same register either way, so it is passed on as is.
pub extern "C" fn _open(path: *const c_char, flags: i32, mode: i32) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wopen": extern "C" fn(*const u16, i32, i32) -> i32)(
            p.as_ptr(),
            flags,
            mode,
        ),
        None => host!("ucrtbase.dll", "_open": extern "C" fn(*const c_char, i32, i32) -> i32)(
            path, flags, mode,
        ),
    }
}

/// See [`_open`]; used by libHttpClient.
pub extern "C" fn _wopen(path: *const u16, flags: i32, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wopen": extern "C" fn(*const u16, i32, i32) -> i32)(
        p.ptr(),
        flags,
        mode,
    )
}

pub extern "C" fn _wstat64(path: *const u16, stat: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wstat64": extern "C" fn(*const u16, *mut c_void) -> i32)(p.ptr(), stat)
}

pub extern "C" fn _stat64(path: *const c_char, stat: *mut c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wstat64": extern "C" fn(*const u16, *mut c_void) -> i32)(
                p.as_ptr(),
                stat,
            )
        }
        None => host!("ucrtbase.dll", "_stat64": extern "C" fn(*const c_char, *mut c_void) -> i32)(
            path, stat,
        ),
    }
}

pub extern "C" fn _wstat64i32(path: *const u16, stat: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wstat64i32": extern "C" fn(*const u16, *mut c_void) -> i32)(
        p.ptr(),
        stat,
    )
}

pub extern "C" fn _stat64i32(path: *const c_char, stat: *mut c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wstat64i32": extern "C" fn(*const u16, *mut c_void) -> i32)(
                p.as_ptr(),
                stat,
            )
        }
        None => {
            host!("ucrtbase.dll", "_stat64i32": extern "C" fn(*const c_char, *mut c_void) -> i32)(
                path, stat,
            )
        }
    }
}

pub extern "C" fn remove(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wremove": extern "C" fn(*const u16) -> i32)(p.as_ptr()),
        None => host!("ucrtbase.dll", "remove": extern "C" fn(*const c_char) -> i32)(path),
    }
}

pub extern "C" fn _wremove(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wremove": extern "C" fn(*const u16) -> i32)(p.ptr())
}

pub extern "C" fn _unlink(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wunlink": extern "C" fn(*const u16) -> i32)(p.as_ptr()),
        None => host!("ucrtbase.dll", "_unlink": extern "C" fn(*const c_char) -> i32)(path),
    }
}

pub extern "C" fn _wunlink(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wunlink": extern "C" fn(*const u16) -> i32)(p.ptr())
}

pub extern "C" fn rename(from: *const c_char, to: *const c_char) -> i32 {
    match narrow_pair(from, to) {
        Some((from, to)) => {
            host!("ucrtbase.dll", "_wrename": extern "C" fn(*const u16, *const u16) -> i32)(
                from.ptr(),
                to.ptr(),
            )
        }
        None => {
            host!("ucrtbase.dll", "rename": extern "C" fn(*const c_char, *const c_char) -> i32)(
                from, to,
            )
        }
    }
}

pub extern "C" fn _wrename(from: *const u16, to: *const u16) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("ucrtbase.dll", "_wrename": extern "C" fn(*const u16, *const u16) -> i32)(
        from.ptr(),
        to.ptr(),
    )
}

pub extern "C" fn _mkdir(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wmkdir": extern "C" fn(*const u16) -> i32)(p.as_ptr()),
        None => host!("ucrtbase.dll", "_mkdir": extern "C" fn(*const c_char) -> i32)(path),
    }
}

pub extern "C" fn _wmkdir(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wmkdir": extern "C" fn(*const u16) -> i32)(p.ptr())
}

pub extern "C" fn _rmdir(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wrmdir": extern "C" fn(*const u16) -> i32)(p.as_ptr()),
        None => host!("ucrtbase.dll", "_rmdir": extern "C" fn(*const c_char) -> i32)(path),
    }
}

pub extern "C" fn _wrmdir(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wrmdir": extern "C" fn(*const u16) -> i32)(p.ptr())
}

pub extern "C" fn _access(path: *const c_char, mode: i32) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_waccess": extern "C" fn(*const u16, i32) -> i32)(
            p.as_ptr(),
            mode,
        ),
        None => {
            host!("ucrtbase.dll", "_access": extern "C" fn(*const c_char, i32) -> i32)(path, mode)
        }
    }
}

pub extern "C" fn _waccess(path: *const u16, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_waccess": extern "C" fn(*const u16, i32) -> i32)(p.ptr(), mode)
}

pub extern "C" fn _access_s(path: *const c_char, mode: i32) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_waccess_s": extern "C" fn(*const u16, i32) -> i32)(
            p.as_ptr(),
            mode,
        ),
        None => {
            host!("ucrtbase.dll", "_access_s": extern "C" fn(*const c_char, i32) -> i32)(path, mode)
        }
    }
}

pub extern "C" fn _waccess_s(path: *const u16, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_waccess_s": extern "C" fn(*const u16, i32) -> i32)(p.ptr(), mode)
}

pub extern "C" fn _chmod(path: *const c_char, mode: i32) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wchmod": extern "C" fn(*const u16, i32) -> i32)(
            p.as_ptr(),
            mode,
        ),
        None => {
            host!("ucrtbase.dll", "_chmod": extern "C" fn(*const c_char, i32) -> i32)(path, mode)
        }
    }
}

pub extern "C" fn _wchmod(path: *const u16, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wchmod": extern "C" fn(*const u16, i32) -> i32)(p.ptr(), mode)
}

pub extern "C" fn _utime64(path: *const c_char, times: *mut c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wutime64": extern "C" fn(*const u16, *mut c_void) -> i32)(
                p.as_ptr(),
                times,
            )
        }
        None => {
            host!("ucrtbase.dll", "_utime64": extern "C" fn(*const c_char, *mut c_void) -> i32)(
                path, times,
            )
        }
    }
}

pub extern "C" fn _wutime64(path: *const u16, times: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wutime64": extern "C" fn(*const u16, *mut c_void) -> i32)(
        p.ptr(),
        times,
    )
}

// The C++ library: `std::basic_filebuf::open` goes through `_Fiopen`.

const FIOPEN: &str = "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z";
const FIOPEN_WIDE: &str = "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z";

/// `_Fiopen(const wchar_t* path, ios_base::openmode, int protection)`.
extern "C" fn fiopen_wide(path: *const u16, mode: i32, protection: i32) -> File {
    let p = wide(path);
    host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z": extern "C" fn(*const u16, i32, i32) -> File)(
        p.ptr(),
        mode,
        protection,
    )
}

/// `_Fiopen(const char* path, ios_base::openmode, int protection)`.
extern "C" fn fiopen(path: *const c_char, mode: i32, protection: i32) -> File {
    match narrow(path) {
        Some(p) => {
            host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z": extern "C" fn(*const u16, i32, i32) -> File)(
                p.as_ptr(),
                mode,
                protection,
            )
        }
        None => {
            host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z": extern "C" fn(*const c_char, i32, i32) -> File)(
                path, mode, protection,
            )
        }
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "files",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-file-*",
            "api-ms-win-core-processenvironment-*",
            "ucrtbase.dll",
            "api-ms-win-crt-stdio-*",
            "api-ms-win-crt-filesystem-*",
            "api-ms-win-crt-time-*",
            "msvcp140.dll",
        ],
        exports: vec![
            hle_export!(ReadFile),
            hle_export!(fread),
            hle_export!(CreateFileW),
            hle_export!(CreateFileA),
            hle_export!(CreateFile2),
            hle_export!(DeleteFileA),
            hle_export!(DeleteFileW),
            hle_export!(RemoveDirectoryA),
            hle_export!(RemoveDirectoryW),
            hle_export!(SetFileAttributesA),
            hle_export!(SetFileAttributesW),
            hle_export!(GetFileAttributesA),
            hle_export!(CreateDirectoryA),
            hle_export!(CreateDirectoryW),
            hle_export!(GetFileAttributesW),
            hle_export!(GetFileAttributesExA),
            hle_export!(GetFileAttributesExW),
            hle_export!(GetDiskFreeSpaceExA),
            hle_export!(GetDiskFreeSpaceExW),
            hle_export!(GetDiskFreeSpaceW),
            hle_export!(FindFirstFileW),
            hle_export!(FindFirstFileExW),
            hle_export!(MoveFileA),
            hle_export!(MoveFileW),
            hle_export!(MoveFileExA),
            hle_export!(MoveFileExW),
            hle_export!(CopyFileA),
            hle_export!(CopyFileW),
            hle_export!(SetCurrentDirectoryA),
            hle_export!(SetCurrentDirectoryW),
            hle_export!(_wfopen),
            hle_export!(_wfopen_s),
            hle_export!(_wfsopen),
            hle_export!(fopen),
            hle_export!(fopen_s),
            hle_export!(_open),
            hle_export!(_wopen),
            hle_export!(_wstat64),
            hle_export!(_stat64),
            hle_export!(_wstat64i32),
            hle_export!(_stat64i32),
            hle_export!(remove),
            hle_export!(_wremove),
            hle_export!(_unlink),
            hle_export!(_wunlink),
            hle_export!(rename),
            hle_export!(_wrename),
            hle_export!(_mkdir),
            hle_export!(_wmkdir),
            hle_export!(_rmdir),
            hle_export!(_wrmdir),
            hle_export!(_access),
            hle_export!(_waccess),
            hle_export!(_access_s),
            hle_export!(_waccess_s),
            hle_export!(_chmod),
            hle_export!(_wchmod),
            hle_export!(_utime64),
            hle_export!(_wutime64),
            HleExport {
                name: FIOPEN,
                address: fiopen as *const () as usize,
            },
            HleExport {
                name: FIOPEN_WIDE,
                address: fiopen_wide as *const () as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn kernel_reads_reach_the_buffer() {
        use std::os::windows::io::AsRawHandle;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"0123456789").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let (mut out, mut n) = ([0u8; 10], 0);
        let handle = file.as_raw_handle().cast();
        assert_ne!(
            ReadFile(handle, out.as_mut_ptr(), 10, &mut n, std::ptr::null_mut()),
            0
        );
        assert_eq!((&out, n), (b"0123456789", 10));
    }

    fn text(v: &[u16]) -> String {
        String::from_utf16_lossy(&v[..v.len() - 1])
    }

    #[test]
    fn rewrites_only_paths_on_mounted_drives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let letter = mount(dir.path()).unwrap();
        assert_eq!(
            mount(dir.path()),
            Some(letter),
            "mounting twice reuses the letter"
        );
        let tr = |s: String| translate(&w(&s)[..s.encode_utf16().count()]).map(|v| text(&v));
        assert_eq!(
            tr(format!("{letter}:/a/b.txt")),
            Some(format!("{root}/a/b.txt"))
        );
        let lower = letter.to_ascii_lowercase();
        assert_eq!(tr(format!("{lower}:\\a")), Some(format!("{root}\\a")));
        assert_eq!(tr(format!("{letter}:")), Some(format!("{root}\\")));
        assert_eq!(
            tr(format!(r"\\?\{letter}:/a/b")),
            Some(format!(r"\\?\{root}\a\b"))
        );
        assert_eq!(tr(r"C:\Windows".into()), None);
        assert_eq!(tr("relative/path".into()), None);
        assert_eq!(tr(String::new()), None);

        // Fixed letters, as the console's package drive; a later mount of
        // the same letter replaces the earlier one.
        let package = tempfile::tempdir().unwrap();
        mount_at('g', dir.path());
        mount_at('G', package.path());
        assert_eq!(
            tr(r"G:\appdata0.rpf".into()),
            Some(format!(r"{}\appdata0.rpf", package.path().display()))
        );
        assert_ne!(mount(dir.path()), Some('G'), "free letters skip fixed ones");

        // Long ones take the `\\?\` form, cleaned up as the host would.
        let long = "d".repeat(250);
        assert_eq!(
            tr(format!(r"G:/a/./b/../{long}//f")),
            Some(format!(r"\\?\{}\a\{long}\f", package.path().display()))
        );
        assert_eq!(
            verbatim(&w(r"\\server\share\")[..15], &w("/x/../../y")[..10]),
            w(r"\\?\UNC\server\share\y")[..22].to_vec()
        );
    }

    /// A title run by its file name alone has an empty folder: the drive
    /// over it is the current directory, not the current drive's root.
    #[test]
    fn an_empty_root_is_the_current_directory() {
        let letter = mount(Path::new("")).unwrap();
        let path = format!(r"{letter}:\a.rpf");
        let host = translate(&w(&path)[..path.len()]).map(|v| text(&v));
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(host, Some(cwd.join("a.rpf").display().to_string()));
    }

    #[test]
    fn long_title_paths_work_on_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        let deep = format!("{letter}:/{}/{}", "a".repeat(200), "b".repeat(100));
        let null = std::ptr::null_mut();
        let first = w(&deep[..203]);
        assert_ne!(CreateDirectoryW(first.as_ptr(), null), 0);
        assert_ne!(CreateDirectoryW(w(&deep).as_ptr(), null), 0);
        let file = format!("{deep}/../{}/f.txt", "b".repeat(100));
        let f = _wfopen(w(&file).as_ptr(), w("wb").as_ptr());
        assert!(!f.is_null(), "{file}");
        host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32)(f);
        assert_ne!(DeleteFileW(w(&format!("{deep}/f.txt")).as_ptr()), 0);
        assert_ne!(RemoveDirectoryW(w(&deep).as_ptr()), 0);
    }

    #[test]
    fn title_file_calls_land_in_the_host_folder() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let close = host!("kernel32.dll", "CloseHandle": extern "system" fn(Handle) -> i32);
        let fclose = host!("ucrtbase.dll", "fclose": extern "C" fn(File) -> i32);

        let path = w(&format!(r"{letter}:\created.bin"));
        let null = std::ptr::null_mut();
        let h = CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_ne!(h as isize, -1);
        close(h);
        assert!(dir.path().join("created.bin").is_file());

        let path = format!("{letter}:/opened.txt\0");
        let f = fopen(path.as_ptr().cast(), c"wb".as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("opened.txt").is_file());

        let path = format!("{letter}:/stream.txt\0");
        const OUT: i32 = 2;
        let f = fiopen(path.as_ptr().cast(), OUT, 0x40);
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("stream.txt").is_file());

        assert_eq!(remove(path.as_ptr().cast()), 0);
        assert!(!dir.path().join("stream.txt").exists());
        let f = fiopen_wide(w(&format!("{letter}:/wide.txt")).as_ptr(), OUT, 0x40);
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("wide.txt").is_file());

        const OPEN_EXISTING: u32 = 3;
        let path = w(&format!(r"{letter}:\created.bin"));
        let h = CreateFile2(path.as_ptr(), GENERIC_WRITE, 0, OPEN_EXISTING, null);
        assert_ne!(h as isize, -1);
        close(h);
        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        assert_ne!(GetFileAttributesW(path.as_ptr()), INVALID_FILE_ATTRIBUTES);

        const O_WRONLY: i32 = 1;
        const O_CREAT: i32 = 0x100;
        const S_IWRITE: i32 = 0x80;
        let path = format!("{letter}:/low.bin\0");
        let fd = _open(path.as_ptr().cast(), O_WRONLY | O_CREAT, S_IWRITE);
        assert!(fd >= 0);
        host!("ucrtbase.dll", "_close": extern "C" fn(i32) -> i32)(fd);
        assert!(dir.path().join("low.bin").is_file());

        // Other paths pass through untouched.
        let host_path = format!("{}\0", dir.path().join("plain.txt").display());
        let f = fopen(host_path.as_ptr().cast(), c"wb".as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("plain.txt").is_file());
    }

    /// A narrow path is rewritten once, as its wide twin is. The inner
    /// drive's folder lies on the outer virtual drive, as the title folder
    /// does when it sits on a host `G:` drive and is mounted as `G:`. The
    /// outer letter is no host drive, so the host finds nothing there; a
    /// narrow call that reaches the outer folder rewrote the path twice.
    #[test]
    fn narrow_paths_are_rewritten_once() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("inner");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("present.txt"), b"").unwrap();
        let outer = mount(dir.path()).unwrap();
        let inner = mount(Path::new(&format!(r"{outer}:\inner"))).unwrap();
        let path = |name: &str| format!("{inner}:/{name}\0");

        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        let wide = GetFileAttributesW(w(&format!("{inner}:/present.txt")).as_ptr());
        assert_eq!(wide, INVALID_FILE_ATTRIBUTES);
        let narrow = GetFileAttributesA(path("present.txt").as_ptr().cast());
        assert_eq!(narrow, wide, "GetFileAttributesA agrees with its wide twin");

        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let null = std::ptr::null_mut();
        let h = CreateFileA(
            path("a.bin").as_ptr().cast(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_eq!(h as isize, -1);
        assert!(fopen(path("b.txt").as_ptr().cast(), c"wb".as_ptr()).is_null());
        let mut f: File = std::ptr::null_mut();
        let error = fopen_s(&mut f, path("c.txt").as_ptr().cast(), c"wb".as_ptr());
        assert_ne!(error, 0);
        const OUT: i32 = 2;
        assert!(fiopen(path("d.txt").as_ptr().cast(), OUT, 0x40).is_null());
        assert_eq!(CreateDirectoryA(path("dir_a").as_ptr().cast(), null), 0);
        assert_eq!(_mkdir(path("dir_crt").as_ptr().cast()), -1);
        assert_eq!(_access(path("present.txt").as_ptr().cast(), 0), -1);
        assert_eq!(SetCurrentDirectoryA(path("").as_ptr().cast()), 0);
        assert_eq!(SetCurrentDirectoryW(w(&format!("{inner}:")).as_ptr()), 0);
        assert_eq!(
            CopyFileA(
                path("present.txt").as_ptr().cast(),
                path("copy.txt").as_ptr().cast(),
                0
            ),
            0
        );
        assert_eq!(
            MoveFileA(
                path("present.txt").as_ptr().cast(),
                path("moved.txt").as_ptr().cast()
            ),
            0
        );
        assert_eq!(
            rename(
                path("present.txt").as_ptr().cast(),
                path("renamed.txt").as_ptr().cast()
            ),
            -1
        );

        // Nothing was made behind a path rewritten twice.
        let names: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["present.txt"]);
    }

    #[test]
    fn narrow_and_wide_file_and_directory_wrappers_work_on_virtual_drives() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        let cstr = |s: String| format!("{s}\0");
        let null = std::ptr::null_mut();

        // Win32 directory, attribute, and disk space APIs.
        let sub_a = cstr(format!("{letter}:/sub_a"));
        assert_ne!(CreateDirectoryA(sub_a.as_ptr().cast(), null), 0);
        assert!(dir.path().join("sub_a").is_dir());

        let mut attr_data = [0u64; 8];
        assert_ne!(
            GetFileAttributesExA(sub_a.as_ptr().cast(), 0, attr_data.as_mut_ptr().cast()),
            0
        );
        let mut avail = 0u64;
        assert_ne!(
            GetDiskFreeSpaceExA(
                cstr(format!("{letter}:\\")).as_ptr().cast(),
                &mut avail,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            0
        );
        assert_ne!(RemoveDirectoryA(sub_a.as_ptr().cast()), 0);
        assert!(!dir.path().join("sub_a").exists());

        // CopyFileW/A and MoveFileW/A/ExA, including mixed virtual/host pairs.
        let src_v = w(&format!("{letter}:\\src.txt"));
        std::fs::write(dir.path().join("src.txt"), b"hello").unwrap();
        let dst_vw = w(&format!("{letter}:\\dst_w.txt"));
        assert_ne!(CopyFileW(src_v.as_ptr(), dst_vw.as_ptr(), 0), 0);
        assert_eq!(
            std::fs::read(dir.path().join("dst_w.txt")).unwrap(),
            b"hello"
        );

        let host_dir = tempfile::tempdir().unwrap();
        let host_file = cstr(host_dir.path().join("from_host.txt").display().to_string());
        std::fs::write(host_dir.path().join("from_host.txt"), b"host").unwrap();
        let dst_va = cstr(format!("{letter}:/from_host_copy.txt"));
        assert_ne!(
            CopyFileA(host_file.as_ptr().cast(), dst_va.as_ptr().cast(), 0),
            0
        );
        assert_eq!(
            std::fs::read(dir.path().join("from_host_copy.txt")).unwrap(),
            b"host"
        );

        let moved_host = cstr(host_dir.path().join("moved_out.txt").display().to_string());
        assert_ne!(
            MoveFileA(dst_va.as_ptr().cast(), moved_host.as_ptr().cast()),
            0
        );
        assert!(!dir.path().join("from_host_copy.txt").exists());
        assert_eq!(
            std::fs::read(host_dir.path().join("moved_out.txt")).unwrap(),
            b"host"
        );

        let moved_w = w(&format!("{letter}:\\moved_w.txt"));
        assert_ne!(MoveFileW(dst_vw.as_ptr(), moved_w.as_ptr()), 0);
        assert!(!dir.path().join("dst_w.txt").exists());
        let moved_ex = cstr(format!("{letter}:/moved_ex.txt"));
        assert_ne!(
            MoveFileExA(
                cstr(format!("{letter}:/moved_w.txt")).as_ptr().cast(),
                moved_ex.as_ptr().cast(),
                1 // MOVEFILE_REPLACE_EXISTING
            ),
            0
        );
        assert!(dir.path().join("moved_ex.txt").is_file());

        // SetFileAttributesA round-trips read-only then restores normal.
        const FILE_ATTRIBUTE_READONLY: u32 = 1;
        const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
        assert_ne!(
            SetFileAttributesA(moved_ex.as_ptr().cast(), FILE_ATTRIBUTE_READONLY),
            0
        );
        assert_ne!(
            GetFileAttributesA(moved_ex.as_ptr().cast()) & FILE_ATTRIBUTE_READONLY,
            0
        );
        assert_ne!(
            SetFileAttributesA(moved_ex.as_ptr().cast(), FILE_ATTRIBUTE_NORMAL),
            0
        );
        assert_ne!(DeleteFileA(moved_ex.as_ptr().cast()), 0);

        // UCRT directory, access, stat, utime, chmod, rename, and remove twins.
        let crt_a = cstr(format!("{letter}:/crt_a"));
        let crt_w = w(&format!("{letter}:\\crt_w"));
        assert_eq!(_mkdir(crt_a.as_ptr().cast()), 0);
        assert_eq!(_wmkdir(crt_w.as_ptr()), 0);
        assert_eq!(_access(crt_a.as_ptr().cast(), 0), 0);
        assert_eq!(_waccess(crt_w.as_ptr(), 0), 0);
        assert_eq!(_access_s(crt_a.as_ptr().cast(), 0), 0);
        assert_eq!(_waccess_s(crt_w.as_ptr(), 0), 0);
        assert_eq!(_rmdir(crt_a.as_ptr().cast()), 0);
        assert_eq!(_wrmdir(crt_w.as_ptr()), 0);

        let src_a = cstr(format!("{letter}:/src.txt"));
        let mut stat_buf = [0u64; 8];
        assert_eq!(
            _stat64(src_a.as_ptr().cast(), stat_buf.as_mut_ptr().cast()),
            0
        );
        assert_eq!(_wstat64i32(src_v.as_ptr(), stat_buf.as_mut_ptr().cast()), 0);

        let mut times = [1_700_000_000i64; 2];
        assert_eq!(
            _utime64(src_a.as_ptr().cast(), times.as_mut_ptr().cast()),
            0
        );
        const S_IREAD_WRITE: i32 = 0x180;
        assert_eq!(_chmod(src_a.as_ptr().cast(), S_IREAD_WRITE), 0);

        let ren_w = w(&format!("{letter}:\\ren_w.txt"));
        assert_eq!(_wrename(src_v.as_ptr(), ren_w.as_ptr()), 0);
        let ren_a = cstr(format!("{letter}:/ren_a.txt"));
        assert_eq!(
            rename(
                cstr(format!("{letter}:/ren_w.txt")).as_ptr().cast(),
                ren_a.as_ptr().cast()
            ),
            0
        );
        assert_eq!(_unlink(ren_a.as_ptr().cast()), 0);
        assert!(!dir.path().join("ren_a.txt").exists());

        std::fs::write(dir.path().join("u_w.bin"), b"1").unwrap();
        assert_eq!(_wunlink(w(&format!("{letter}:\\u_w.bin")).as_ptr()), 0);
        std::fs::write(dir.path().join("r_w.bin"), b"2").unwrap();
        assert_eq!(_wremove(w(&format!("{letter}:\\r_w.bin")).as_ptr()), 0);
        assert!(!dir.path().join("u_w.bin").exists());
        assert!(!dir.path().join("r_w.bin").exists());
    }

    /// Run in a child process so mutating the process working directory does
    /// not race with parallel tests. Does nothing otherwise.
    #[test]
    fn set_current_directory_child() {
        let Ok(root) = std::env::var("VICTORY_FILES_CWD_CHILD") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let letter = mount(&root).unwrap();
        let sub_a = format!("{letter}:\\dir_a");
        let sub_b = format!("{letter}:\\dir_b");
        assert_ne!(
            SetCurrentDirectoryA(format!("{sub_a}\0").as_ptr().cast()),
            0
        );
        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
            std::fs::canonicalize(root.join("dir_a")).unwrap()
        );
        assert_ne!(SetCurrentDirectoryW(w(&sub_b).as_ptr()), 0);
        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap(),
            std::fs::canonicalize(root.join("dir_b")).unwrap()
        );
        // Leave the temp directory before exit so the parent can remove it.
        let _ = std::env::set_current_dir(std::env::temp_dir());
    }

    #[test]
    fn set_current_directory_rewrites_virtual_drives() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dir_a")).unwrap();
        std::fs::create_dir(dir.path().join("dir_b")).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "modules::files::tests::set_current_directory_child",
                "--nocapture",
            ])
            .env("VICTORY_FILES_CWD_CHILD", dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "{status:?}");
    }
}
