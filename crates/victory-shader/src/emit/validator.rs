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
}

// SAFETY: the validator object is free-threaded (DXC's validator has no
// thread affinity); calls are serialised by `&mut self`.
unsafe impl Send for Validator {}

fn parse_sdk_version(name: &std::ffi::OsStr) -> Option<Vec<u64>> {
    let text = name.to_str()?;
    if text.is_empty() {
        return None;
    }
    text.split('.').map(|part| part.parse::<u64>().ok()).collect()
}

fn newest_sdk_dxil(kits_dir: &std::path::Path) -> Option<PathBuf> {
    let mut versions: Vec<(Option<Vec<u64>>, std::ffi::OsString, PathBuf)> =
        std::fs::read_dir(kits_dir)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let dll = e.path().join(r"x64\dxil.dll");
                dll.exists().then(|| {
                    let name = e.file_name();
                    let parsed = parse_sdk_version(&name);
                    (parsed, name, dll)
                })
            })
            .collect();
    versions.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    versions.pop().map(|(_, _, path)| path)
}

fn resolve_default_path(
    env_val: Option<std::ffi::OsString>,
    kits_dir: &std::path::Path,
) -> Option<PathBuf> {
    if let Some(p) = env_val.filter(|v| !v.is_empty()) {
        return Some(p.into());
    }
    newest_sdk_dxil(kits_dir)
}

impl Validator {
    /// Where `dxil.dll` is looked for.
    pub fn default_path() -> Option<PathBuf> {
        let kits = std::path::Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
        resolve_default_path(std::env::var_os("VICTORY_DXIL"), kits)
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
                let shown = String::from_utf16_lossy(&name[..name.len() - 1]);
                return Err(format!("cannot load {shown}"));
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
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "victory-validator-test-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn newest_sdk_dxil_orders_versions_numerically() {
        let dir = TempDir::new("versions");
        for ver in ["10.0.9999.0", "10.0.26100.0", "10.0.19041.0", "misc"] {
            let x64 = dir.path().join(ver).join("x64");
            std::fs::create_dir_all(&x64).unwrap();
            std::fs::write(x64.join("dxil.dll"), b"").unwrap();
        }
        assert_eq!(
            newest_sdk_dxil(dir.path()),
            Some(dir.path().join("10.0.26100.0").join(r"x64\dxil.dll"))
        );
    }

    #[test]
    fn resolve_default_path_prefers_non_empty_env_and_ignores_empty() {
        let dir = TempDir::new("env");
        let x64 = dir.path().join("10.0.22621.0").join("x64");
        std::fs::create_dir_all(&x64).unwrap();
        let expected = x64.join("dxil.dll");
        std::fs::write(&expected, b"").unwrap();

        assert_eq!(
            resolve_default_path(Some(OsString::from(r"C:\custom\dxil.dll")), dir.path()),
            Some(PathBuf::from(r"C:\custom\dxil.dll"))
        );
        assert_eq!(
            resolve_default_path(Some(OsString::new()), dir.path()),
            Some(expected.clone())
        );
        assert_eq!(resolve_default_path(None, dir.path()), Some(expected));
    }

    #[test]
    fn load_missing_dll_error_omits_trailing_nul() {
        let dir = TempDir::new("missing");
        let missing = dir.path().join("definitely-missing-dxil.dll");
        let err = Validator::load(Some(&missing)).err().unwrap();
        assert!(!err.contains('\0'), "error must not contain NUL: {err:?}");
        assert!(err.contains("definitely-missing-dxil.dll"), "{err}");
    }
}
