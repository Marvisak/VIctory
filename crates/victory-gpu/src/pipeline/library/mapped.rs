//! A file kept mapped in memory, for the pipeline library's bytes: read
//! (`FILE_MAP_READ`) so that a library of 1 to 2.4 GB is not committed
//! memory (its pages come from the file when used, and go back to it), and
//! written (`FILE_MAP_WRITE`) so that a save serialises straight into the
//! file instead of into a buffer of that size.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, FlushViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile,
};

/// A whole file mapped for as long as this lives. The file's handle, the
/// mapping's and the view all stay open; they close in that order's
/// reverse (view, mapping, file), which is also what lets Windows delete
/// the file afterwards.
pub(super) struct MappedFile {
    view: *mut u8,
    len: usize,
    mapping: HANDLE,
    writable: bool,
    /// Closed last, after `Drop::drop` released the view and the mapping.
    _file: File,
}

// SAFETY: the view is plain memory, read through `&self` and written
// through `&mut self` only; the handles are only closed on drop.
unsafe impl Send for MappedFile {}
// SAFETY: as above: a shared `MappedFile` only reads.
unsafe impl Sync for MappedFile {}

impl MappedFile {
    /// `path`, mapped for reading. A file with no bytes cannot be mapped.
    pub(super) fn open(path: &Path) -> io::Result<MappedFile> {
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "an empty file"));
        }
        MappedFile::map(file, len, false)
    }

    /// `path`, made (or emptied) and sized to `len` bytes, mapped for
    /// writing.
    pub(super) fn create(path: &Path, len: usize) -> io::Result<MappedFile> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "an empty mapping"));
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        file.set_len(len as u64)?;
        MappedFile::map(file, len, true)
    }

    fn map(file: File, len: usize, writable: bool) -> io::Result<MappedFile> {
        let (protect, access) =
            if writable { (PAGE_READWRITE, FILE_MAP_WRITE) } else { (PAGE_READONLY, FILE_MAP_READ) };
        // SAFETY: a live file handle; no name, and the whole file (size 0).
        let mapping = unsafe {
            CreateFileMappingW(file.as_raw_handle(), ptr::null(), protect, 0, 0, ptr::null())
        };
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the mapping just made, viewed whole.
        let view = unsafe { MapViewOfFile(mapping, access, 0, 0, 0) };
        if view.Value.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: the mapping made above, not used again.
            unsafe { CloseHandle(mapping) };
            return Err(error);
        }
        Ok(MappedFile { view: view.Value.cast(), len, mapping, writable, _file: file })
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        // SAFETY: `len` bytes mapped from `view` for as long as `self`
        // lives.
        unsafe { std::slice::from_raw_parts(self.view, self.len) }
    }

    /// The bytes to write, of a file made with `create`.
    pub(super) fn as_mut_slice(&mut self) -> &mut [u8] {
        assert!(self.writable, "a mapping for reading");
        // SAFETY: as `as_slice`, and `&mut self` is the only way in.
        unsafe { std::slice::from_raw_parts_mut(self.view, self.len) }
    }

    /// Writes the changed pages to the file.
    pub(super) fn flush(&self) -> io::Result<()> {
        // SAFETY: the view of this mapping.
        match unsafe { FlushViewOfFile(self.view.cast(), 0) } {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // SAFETY: the view and mapping of `map`, released once.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast() });
            CloseHandle(self.mapping);
        }
        // `_file` closes after this.
    }
}
