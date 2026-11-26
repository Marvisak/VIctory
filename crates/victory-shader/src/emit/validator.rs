//! The DXIL validator in `dxil.dll` (`IDxcValidator`), for tests and the
//! offline batch. Translated shaders do not need it: they carry the
//! container hash we compute (`container::hash`).
//!
//! The DLL is found at `VICTORY_DXIL` if set, else the newest Windows SDK's
//! `bin\<version>\x64\dxil.dll`, else wherever `LoadLibrary` finds it.

use std::ffi::c_void;
use std::path::PathBuf;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type Hresult = i32;

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const CLSID_DXC_VALIDATOR: Guid = Guid(
    0x8ca3_e215,
    0xf728,
    0x4cf3,
    [0x8c, 0xdd, 0x88, 0xaf, 0x91, 0x75, 0x87, 0xa1],
);
const IID_IDXC_VALIDATOR: Guid = Guid(
    0xa6e8_2bd2,
    0x1fd7,
    0x4826,
    [0x98, 0x11, 0x28, 0x57, 0xe7, 0x97, 0xf4, 0x9a],
);
/// `DxcValidatorFlags_InPlaceEdit`: sign the container we pass.
const IN_PLACE_EDIT: u32 = 1;

type CreateInstance =
    unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> Hresult;

/// COM object: a pointer to a table of method pointers.
type Object = *mut *const usize;

unsafe fn method<F: Copy>(obj: Object, slot: usize) -> F {
    // SAFETY: the caller names a slot of the object's interface.
    unsafe { std::mem::transmute_copy(&*(*obj).add(slot)) }
}

unsafe fn release(obj: Object) {
    if !obj.is_null() {
        // SAFETY: slot 2 of every COM interface is Release.
        unsafe { method::<unsafe extern "system" fn(Object) -> u32>(obj, 2)(obj) };
    }
}

/// An `IDxcBlob` over our bytes: QueryInterface, AddRef, Release,
/// GetBufferPointer, GetBufferSize. Lives on the caller's stack.
#[repr(C)]
struct Blob {
    vtable: *const BlobVtable,
    data: *mut u8,
    len: usize,
}

/// IUnknown and IDxcBlob, as raw GUID bytes.
const IUNKNOWN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46];
const IDXC_BLOB: [u8; 16] = [
    0x08, 0xfb, 0xa5, 0x8b, 0x95, 0x51, 0xe2, 0x40, 0xac, 0x58, 0x0d, 0x98, 0x9c, 0x3a, 0x01, 0x02,
];

unsafe extern "system" fn blob_qi(
    this: *mut Blob,
    iid: *const [u8; 16],
    out: *mut *mut c_void,
) -> Hresult {
    // SAFETY: the validator passes valid pointers.
    unsafe {
        if *iid == IUNKNOWN || *iid == IDXC_BLOB {
            *out = this.cast();
            0
        } else {
            *out = std::ptr::null_mut();
            0x8000_4002_u32 as i32 // E_NOINTERFACE
        }
    }
}
unsafe extern "system" fn blob_ref(_: *mut Blob) -> u32 {
    1
}
unsafe extern "system" fn blob_ptr(this: *mut Blob) -> *mut u8 {
    // SAFETY: `this` is our blob.
    unsafe { (*this).data }
}
unsafe extern "system" fn blob_len(this: *mut Blob) -> usize {
    // SAFETY: `this` is our blob.
    unsafe { (*this).len }
}

#[repr(C)]
struct BlobVtable {
    qi: unsafe extern "system" fn(*mut Blob, *const [u8; 16], *mut *mut c_void) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut Blob) -> u32,
    release: unsafe extern "system" fn(*mut Blob) -> u32,
    ptr: unsafe extern "system" fn(*mut Blob) -> *mut u8,
    len: unsafe extern "system" fn(*mut Blob) -> usize,
}

static BLOB_VTABLE: BlobVtable = BlobVtable {
    qi: blob_qi,
    add_ref: blob_ref,
    release: blob_ref,
    ptr: blob_ptr,
    len: blob_len,
};

pub struct Validator {
    object: Object,
    /// The DLL it came from, for its file version.
    path: Option<PathBuf>,
}

// SAFETY: the validator object is free-threaded (DXC's validator has no
// thread affinity); calls are serialised by `&mut self`.
unsafe impl Send for Validator {}

/// The version resource's fixed information, as `version.dll` defines it
/// (`VS_FIXEDFILEINFO`): the one field this needs is the file version, and
/// the binding crate this module otherwise uses does not carry it.
#[repr(C)]
struct FixedFileInfo {
    signature: u32,
    struct_version: u32,
    file_version_ms: u32,
    file_version_ls: u32,
    product_version_ms: u32,
    product_version_ls: u32,
    file_flags_mask: u32,
    file_flags: u32,
    file_os: u32,
    file_type: u32,
    file_subtype: u32,
    file_date_ms: u32,
    file_date_ls: u32,
}

#[link(name = "version")]
unsafe extern "system" {
    fn GetFileVersionInfoSizeW(name: *const u16, handle: *mut u32) -> u32;
    fn GetFileVersionInfoW(name: *const u16, handle: u32, len: u32, data: *mut c_void) -> i32;
    fn VerQueryValueW(
        block: *const c_void,
        sub: *const u16,
        out: *mut *mut c_void,
        len: *mut u32,
    ) -> i32;
}

/// The `(major, minor)` file version of the DLL at `path`, through the
/// version resource Windows puts in it (`VS_FIXEDFILEINFO`). The SDK's
/// `dxil.dll` prints `1.8.2502` there while it is the 1.7 validator, which
/// is why a report should state this next to the path (#1945).
fn file_version(path: &std::path::Path) -> Option<(u16, u16)> {
    const ROOT: [u16; 2] = [b'\\' as u16, 0];
    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    // SAFETY: plain version-resource calls with valid out pointers, on a
    // NUL-terminated path and a block of the size just asked for.
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(wide.as_ptr(), &mut handle);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(wide.as_ptr(), handle, size, data.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let mut info: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if VerQueryValueW(data.as_ptr().cast(), ROOT.as_ptr(), &mut info, &mut len) == 0
            || info.is_null()
            || (len as usize) < size_of::<FixedFileInfo>()
        {
            return None;
        }
        let fixed = &*info.cast::<FixedFileInfo>();
        if fixed.signature != 0xFEEF_04BD {
            return None;
        }
        Some((
            (fixed.file_version_ms >> 16) as u16,
            (fixed.file_version_ms & 0xffff) as u16,
        ))
    }
}
/// The engine version a `VS_FIXEDFILEINFO` pair names. SDK builds prefix the
/// series with 100 — `101.7` is the 1.7 engine — so a `major >= 1` or
/// `>= 1.8` gate accepts a validator that rejects what 1.8 accepts (#1945).
fn engine_version((major, minor): (u16, u16)) -> (u16, u16) {
    if major >= 100 {
        (major - 100, minor)
    } else {
        (major, minor)
    }
}

impl Validator {
    /// The file version of the DLL the validator was loaded from, as its
    /// own resource states it: `(major, minor)`. DXC's `dxil.dll` from the
    /// Windows SDK reports `1.8.2502` in `FileVersion` while it is the 1.7
    /// validator, and that version prints the same in every SDK since, so
    /// the pair is what tells a report which engine produced its numbers
    /// (#1945). `None` when the version resource cannot be read.
    ///
    /// The 1.7 validator regenerates a program's `PSV0` and compares it with
    /// what the module carries, so a writer that deduplicates the semantic
    /// name table (as ours does, #1208) is rejected on any shader with a
    /// repeated semantic name; 1.8 compares the content instead and never
    /// rebuilds it. A verdict from a validator before 1.8 is therefore worth
    /// less than the line it prints.
    pub fn version(&self) -> Option<(u16, u16)> {
        // The engine's own answer first: `IDxcValidator::GetVersion` is the
        // pair `dxc -dumpbin` prints as "Validator version", and the only one
        // that is the engine rather than the stamp the SDK put on the file.
        let mut major = 0u32;
        let mut minor = 0u32;
        // SAFETY: slot 4 of `IDxcValidator` is GetVersion, which writes two
        // u32s; the object outlives the call.
        let get_version = unsafe {
            method::<unsafe extern "system" fn(Object, *mut u32, *mut u32) -> Hresult>(self.object, 4)
        };
        // SAFETY: as above; `self.object` is a live `IDxcValidator`.
        if unsafe { get_version(self.object, &mut major, &mut minor) } >= 0 {
            return Some((major as u16, minor as u16));
        }
        // A validator whose table is short: the file stamp, with the SDK's
        // 100-prefixed series levelled back to the engine's (#1945).
        let path = self.path.as_ref()?;
        file_version(path).map(engine_version)
    }

    /// The DLL's `VS_FIXEDFILEINFO` version, for a report that has to say
    /// where the engine came from: the SDK stamps `1.8.2502` on a 1.7
    /// engine, so this is not the engine version (#1945).
    pub fn file_version(&self) -> Option<(u16, u16)> {
        file_version(self.path.as_ref()?)
    }

    /// Where `dxil.dll` is looked for.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("VICTORY_DXIL") {
            return Some(p.into());
        }
        let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
        let mut versions: Vec<PathBuf> = std::fs::read_dir(&kits)
            .ok()?
            .filter_map(Result::ok)
            .map(|e| e.path().join(r"x64\dxil.dll"))
            .filter(|p| p.exists())
            .collect();
        versions.sort();
        versions.pop()
    }

    /// Loads `path`'s validator (or `dxil.dll` from the DLL search path).
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, String> {
        let name: Vec<u16> = path
            .map_or_else(|| "dxil.dll".into(), |p| p.as_os_str().to_owned())
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: plain Win32 calls with a NUL-terminated name.
        unsafe {
            let module = LoadLibraryW(name.as_ptr());
            if module.is_null() {
                return Err(format!("cannot load {}", String::from_utf16_lossy(&name)));
            }
            let create = GetProcAddress(module, c"DxcCreateInstance".as_ptr().cast())
                .ok_or("dxil.dll lacks DxcCreateInstance")?;
            let create: CreateInstance = std::mem::transmute(create);
            let mut object = std::ptr::null_mut();
            let hr = create(&CLSID_DXC_VALIDATOR, &IID_IDXC_VALIDATOR, &mut object);
            if hr < 0 || object.is_null() {
                return Err(format!("DxcCreateInstance(validator) failed: {hr:#x}"));
            }
            Ok(Validator {
                object: object.cast(),
                path: path.map(std::path::Path::to_path_buf),
            })
        }
    }

    /// Validates `container` and signs it in place; the error text if it
    /// is rejected.
    pub fn validate(&mut self, container: &mut [u8]) -> Result<(), String> {
        let mut blob = Blob {
            vtable: &BLOB_VTABLE,
            data: container.as_mut_ptr(),
            len: container.len(),
        };
        // SAFETY: IDxcValidator::Validate (slot 3) with our blob; then
        // IDxcOperationResult GetStatus (3) and GetErrorBuffer (5), and
        // IDxcBlob GetBufferPointer (3) / GetBufferSize (4).
        unsafe {
            let mut result: Object = std::ptr::null_mut();
            let validate: unsafe extern "system" fn(
                Object,
                *mut Blob,
                u32,
                *mut Object,
            ) -> Hresult = method(self.object, 3);
            let hr = validate(self.object, &mut blob, IN_PLACE_EDIT, &mut result);
            if hr < 0 || result.is_null() {
                return Err(format!("Validate failed: {hr:#x}"));
            }
            let mut status = 0;
            let get_status: unsafe extern "system" fn(Object, *mut Hresult) -> Hresult =
                method(result, 3);
            get_status(result, &mut status);
            let mut text = String::new();
            if status < 0 {
                let mut errors: Object = std::ptr::null_mut();
                let get_errors: unsafe extern "system" fn(Object, *mut Object) -> Hresult =
                    method(result, 5);
                if get_errors(result, &mut errors) >= 0 && !errors.is_null() {
                    let ptr: unsafe extern "system" fn(Object) -> *const u8 = method(errors, 3);
                    let len: unsafe extern "system" fn(Object) -> usize = method(errors, 4);
                    let bytes = std::slice::from_raw_parts(ptr(errors), len(errors));
                    text = String::from_utf8_lossy(bytes)
                        .trim_end_matches('\0')
                        .trim()
                        .into();
                    release(errors);
                }
                if text.is_empty() {
                    text = format!("rejected ({status:#x})");
                }
            }
            release(result);
            if status < 0 { Err(text) } else { Ok(()) }
        }
    }
}

impl Drop for Validator {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { release(self.object) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `IDxcValidator::GetVersion` is the engine, not the stamp the SDK put
    /// on the file: a DLL stamped `1.8.2502` that is the 1.7 engine answers
    /// `(1, 7)` here, which is the pair a warning has to look at (#1945).
    #[test]
    fn the_engine_version_is_the_validators_own_answer() {
        let Some(v) = Validator::load(Validator::default_path().as_deref()).ok() else {
            return; // no dxil.dll on this machine: nothing to ask.
        };
        let (major, minor) = v.version().expect("IDxcValidator answers GetVersion");
        assert!((1..=9).contains(&major), "engine {major}.{minor}");
        if let Some((file_major, file_minor)) = v.file_version() {
            // The two agree on this machine's SDK; what matters is that the
            // engine is what a report uses, and that the stamp is not lost.
            assert!(file_major >= 1, "file {file_major}.{file_minor}");
        }
    }

    /// The Windows SDK stamps the engine series with 100: `101.7` is the 1.7
    /// engine, and a plain `>= 1` (or `>= 1.8`) gate accepts it (#1945).
    #[test]
    fn a_prefixed_series_names_the_same_engine() {
        assert_eq!(engine_version((101, 7)), (1, 7));
        assert_eq!(engine_version((101, 6)), (1, 6));
        assert_eq!(engine_version((1, 8)), (1, 8));
        assert_eq!(engine_version((2, 0)), (2, 0));
    }

    /// The version printed next to the path is the DLL's own, through its
    /// version resource: `VS_FIXEDFILEINFO` and not the string Windows shows
    /// in Explorer, which is what made a 1.7 validator look like 1.8 (#1945).
    #[test]
    fn the_validator_reports_the_version_in_its_resource() {
        let Some(path) = Validator::default_path() else {
            eprintln!("no dxil.dll; skipped");
            return;
        };
        let Ok(validator) = Validator::load(Some(&path)) else {
            eprintln!("dxil.dll did not load; skipped");
            return;
        };
        match validator.version() {
            // Every DXC release validator is 1.x, and the SDK's prints 1.9.
            Some((major, minor)) => assert!(
                major >= 1,
                "{} says version {major}.{minor}",
                path.display()
            ),
            // A DLL without a version resource is possible; the line then has
            // no version and the warning is not raised, which is the honest
            // outcome and not a failure.
            None => eprintln!("{} has no version resource", path.display()),
        }
    }

    /// A path that is not there answers nothing rather than a wrong version.
    #[test]
    fn a_missing_dll_has_no_version() {
        assert_eq!(
            file_version(std::path::Path::new(r"Z:\no\such\dxil.dll")),
            None
        );
    }
}

