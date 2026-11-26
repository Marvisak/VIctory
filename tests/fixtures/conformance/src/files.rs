//! Files on the console's drives: the package at `G:` (read-only), and
//! temporary storage at `T:`.
//!
//! The behaviour checked is Win32's and the CRT's (Microsoft Learn,
//! fileapi.h and the CRT reference), reached through the paths a console
//! title uses: every path-taking import VIctory binds (CreateFileW, -A,
//! CreateFile2, GetFileAttributes*, FindFirstFile*, DeleteFileW,
//! MoveFileExW, CreateDirectoryW, RemoveDirectoryW, fopen, _wfopen,
//! _wstat64) must reach the folder behind the drive letter. The harness
//! mounts both drives; `G:\conformance.txt` holds `CONTENTS`.

use std::ffi::c_void;

use crate::tap::Tap;

pub const CONTENTS: &[u8] = b"conformance: public documentation only\n";

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const MOVEFILE_REPLACE_EXISTING: u32 = 1;
const INVALID_HANDLE: *mut c_void = -1isize as *mut c_void;

#[repr(C)]
struct FindData {
    attributes: u32,
    times: [u32; 6],
    size_high: u32,
    size_low: u32,
    reserved: [u32; 2],
    name: [u16; 260],
    alternate: [u16; 14],
    // Obsolete fields some headers declare; room for them.
    _rest: [u32; 4],
}

/// _stat64: the fields we read, and room for the rest.
#[repr(C)]
struct Stat64 {
    dev: u32,
    ino: u16,
    mode: u16,
    nlink: i16,
    uid: i16,
    gid: i16,
    rdev: u32,
    size: i64,
    times: [i64; 3],
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn CreateFileA(
        name: *const u8,
        access: u32,
        share: u32,
        security: *const c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn CreateFile2(
        name: *const u16,
        access: u32,
        share: u32,
        disposition: u32,
        params: *const c_void,
    ) -> *mut c_void;
    fn ReadFile(
        file: *mut c_void,
        buffer: *mut u8,
        size: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        file: *mut c_void,
        buffer: *const u8,
        size: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetFileAttributesW(name: *const u16) -> u32;
    fn GetFileAttributesA(name: *const u8) -> u32;
    fn FindFirstFileW(name: *const u16, data: *mut FindData) -> *mut c_void;
    fn FindClose(find: *mut c_void) -> i32;
    fn DeleteFileW(name: *const u16) -> i32;
    fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    fn CreateDirectoryW(name: *const u16, security: *const c_void) -> i32;
    fn RemoveDirectoryW(name: *const u16) -> i32;
    fn GetLastError() -> u32;
}

#[link(name = "ucrtbase", kind = "raw-dylib")]
unsafe extern "C" {
    fn fopen(name: *const u8, mode: *const u8) -> *mut c_void;
    fn _wfopen(name: *const u16, mode: *const u16) -> *mut c_void;
    fn fread(buffer: *mut u8, size: usize, count: usize, file: *mut c_void) -> usize;
    fn fclose(file: *mut c_void) -> i32;
    fn _wstat64(name: *const u16, stat: *mut Stat64) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn narrow(s: &str) -> Vec<u8> {
    s.bytes().chain(Some(0)).collect()
}

/// Reads all of an open handle (closing it).
///
/// # Safety
/// A file handle open for reading.
unsafe fn read_all(file: *mut c_void) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let mut n = 0;
        // SAFETY: (caller) an open handle; our buffer.
        let ok = unsafe {
            ReadFile(
                file,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..n as usize]);
    }
    // SAFETY: (caller) ours to close.
    unsafe { CloseHandle(file) };
    out
}

pub fn run(t: &mut Tap) {
    // SAFETY: (all calls) NUL-terminated paths; handles we opened.
    unsafe {
        let file = CreateFileW(
            wide(r"G:\conformance.txt").as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        t.check(
            file != INVALID_HANDLE,
            r"files: CreateFileW opens G:\conformance.txt",
            || format!("error {}", GetLastError()),
        );
        if file != INVALID_HANDLE {
            t.eq(
                read_all(file).as_slice(),
                CONTENTS,
                "files: ReadFile returns the package file's bytes",
            );
        }
        let file = CreateFileA(
            narrow("G:/conformance.txt").as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        t.check(
            file != INVALID_HANDLE,
            "files: CreateFileA opens the same file with forward slashes",
            || format!("error {}", GetLastError()),
        );
        if file != INVALID_HANDLE {
            CloseHandle(file);
        }
        let file = CreateFile2(
            wide(r"\\?\G:\conformance.txt").as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            OPEN_EXISTING,
            std::ptr::null(),
        );
        t.check(
            file != INVALID_HANDLE,
            r"files: CreateFile2 opens it as \\?\G:\...",
            || format!("error {}", GetLastError()),
        );
        if file != INVALID_HANDLE {
            CloseHandle(file);
        }
        let missing = CreateFileW(
            wide(r"G:\no-such-file").as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        let error = GetLastError();
        t.check(
            missing == INVALID_HANDLE && error == ERROR_FILE_NOT_FOUND,
            "files: a missing file is INVALID_HANDLE_VALUE and ERROR_FILE_NOT_FOUND",
            || format!("{missing:?} error {error}"),
        );

        t.eq(
            GetFileAttributesW(wide(r"G:\conformance.txt").as_ptr()) & FILE_ATTRIBUTE_DIRECTORY,
            0,
            "files: GetFileAttributesW sees a file",
        );
        t.check(
            GetFileAttributesA(narrow(r"G:\").as_ptr()) & FILE_ATTRIBUTE_DIRECTORY != 0,
            r"files: GetFileAttributesA sees G:\ as a folder",
            || "not a folder".into(),
        );
        let attributes = GetFileAttributesW(wide(r"G:\no\such\file").as_ptr());
        let error = GetLastError();
        t.check(
            attributes == INVALID_FILE_ATTRIBUTES
                && (error == ERROR_PATH_NOT_FOUND || error == ERROR_FILE_NOT_FOUND),
            "files: GetFileAttributesW of a missing path is INVALID_FILE_ATTRIBUTES",
            || format!("{attributes:#x} error {error}"),
        );

        let mut data: FindData = std::mem::zeroed();
        let find = FindFirstFileW(wide(r"G:\conformance.*").as_ptr(), &mut data);
        let name = String::from_utf16_lossy(
            &data.name[..data.name.iter().position(|&c| c == 0).unwrap_or(0)],
        );
        t.check(
            find != INVALID_HANDLE
                && name == "conformance.txt"
                && data.size_low as usize == CONTENTS.len(),
            "files: FindFirstFileW finds the file by pattern, with its size",
            || format!("{find:?} {name:?} {}", data.size_low),
        );
        if find != INVALID_HANDLE {
            FindClose(find);
        }

        let f = fopen(
            narrow(r"G:\conformance.txt").as_ptr(),
            narrow("rb").as_ptr(),
        );
        t.check(!f.is_null(), "files: fopen opens the package file", || {
            "null".into()
        });
        if !f.is_null() {
            let mut buffer = [0u8; 128];
            let n = fread(buffer.as_mut_ptr(), 1, buffer.len(), f);
            t.eq(&buffer[..n], CONTENTS, "files: fread reads its bytes");
            fclose(f);
        }
        let f = _wfopen(wide(r"G:\conformance.txt").as_ptr(), wide("rb").as_ptr());
        t.check(
            !f.is_null(),
            "files: _wfopen opens the package file",
            || "null".into(),
        );
        if !f.is_null() {
            fclose(f);
        }
        let mut stat: Stat64 = std::mem::zeroed();
        t.check(
            _wstat64(wide(r"G:\conformance.txt").as_ptr(), &mut stat) == 0
                && stat.size as usize == CONTENTS.len(),
            "files: _wstat64 gives the file's size",
            || format!("size {}", stat.size),
        );

        // Temporary storage: make a folder, write, rename over, delete.
        t.check(
            CreateDirectoryW(wide(r"T:\conformance").as_ptr(), std::ptr::null()) != 0,
            r"files: CreateDirectoryW makes T:\conformance",
            || format!("error {}", GetLastError()),
        );
        let path = wide(r"T:\conformance\a.bin");
        let file = CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null(),
            CREATE_ALWAYS,
            0,
            std::ptr::null_mut(),
        );
        let mut written = 0;
        let ok = file != INVALID_HANDLE
            && WriteFile(
                file,
                CONTENTS.as_ptr(),
                CONTENTS.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            ) != 0;
        if file != INVALID_HANDLE {
            CloseHandle(file);
        }
        t.check(
            ok && written as usize == CONTENTS.len(),
            "files: WriteFile writes a file on T:",
            || format!("error {}", GetLastError()),
        );
        let renamed = wide(r"T:\conformance\b.bin");
        t.check(
            MoveFileExW(path.as_ptr(), renamed.as_ptr(), MOVEFILE_REPLACE_EXISTING) != 0,
            "files: MoveFileExW renames it",
            || format!("error {}", GetLastError()),
        );
        t.eq(
            GetFileAttributesW(path.as_ptr()),
            INVALID_FILE_ATTRIBUTES,
            "files: the old name is gone",
        );
        let file = CreateFileW(
            renamed.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        t.check(
            file != INVALID_HANDLE && read_all(file) == CONTENTS,
            "files: the new name has the bytes",
            || "missing or different".into(),
        );
        t.check(
            DeleteFileW(renamed.as_ptr()) != 0,
            "files: DeleteFileW deletes it",
            || format!("error {}", GetLastError()),
        );
        t.check(
            RemoveDirectoryW(wide(r"T:\conformance").as_ptr()) != 0,
            "files: RemoveDirectoryW removes the emptied folder",
            || format!("error {}", GetLastError()),
        );
        t.eq(
            GetFileAttributesW(wide(r"T:\conformance").as_ptr()),
            INVALID_FILE_ATTRIBUTES,
            "files: the folder is gone",
        );
    }
}
