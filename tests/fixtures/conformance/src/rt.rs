//! Reaching the runtime as code linked into a title does: load
//! `xgameruntime.dll`, initialise, then ask `QueryApiImpl` for each area's
//! object.

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::tap::Tap;

pub type Hresult = i32;
pub const S_OK: Hresult = 0;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn LoadLibraryExA(name: *const u8, file: *mut c_void, flags: u32) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

#[link(name = "xgameruntime", kind = "raw-dylib")]
unsafe extern "system" {
    /// Public: XGameRuntimeInitialize (reference/system/xgameruntimeinit).
    fn XGameRuntimeInitialize() -> Hresult;
}

type Query = extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> Hresult;

static QUERY: OnceLock<Option<Query>> = OnceLock::new();

pub fn init(t: &mut Tap) {
    // Public: XGameRuntimeInitialize returns S_OK, and every module of a
    // process calls it, so a second call succeeds too.
    // SAFETY: plain calls into the runtime.
    let (first, second) = unsafe { (XGameRuntimeInitialize(), XGameRuntimeInitialize()) };
    t.eq(first, S_OK, "runtime: XGameRuntimeInitialize succeeds");
    t.eq(
        second,
        S_OK,
        "runtime: XGameRuntimeInitialize succeeds again (each module calls it)",
    );
    let query = QUERY.get_or_init(|| {
        // SAFETY: NUL-terminated names; the flag is LOAD_LIBRARY_SEARCH_SYSTEM32.
        unsafe {
            let module = LoadLibraryExA(
                c"xgameruntime.dll".as_ptr().cast(),
                std::ptr::null_mut(),
                0x800,
            );
            if module.is_null() {
                return None;
            }
            let f = GetProcAddress(module, c"QueryApiImpl".as_ptr().cast());
            (!f.is_null()).then(|| std::mem::transmute::<*mut c_void, Query>(f))
        }
    });
    t.check(
        query.is_some(),
        "runtime: the title's stub finds QueryApiImpl",
        || "not found".into(),
    );
}

/// An area's object: its method table's slots, from `QueryInterface` at 0.
pub struct Object(*mut c_void);

impl Object {
    /// The method at byte offset `offset` of the table, as `F`.
    ///
    /// # Safety
    /// `F` is the slot's signature.
    pub unsafe fn method<F: Copy>(&self, offset: usize) -> F {
        // SAFETY: (caller) an object's first word is its method table.
        unsafe {
            let table = *self.0.cast::<*const *const c_void>();
            std::mem::transmute_copy(&*table.add(offset / 8))
        }
    }

    pub fn this(&self) -> *mut c_void {
        self.0
    }
}

pub fn area(provider: Guid, iid: Guid) -> Result<Object, Hresult> {
    let Some(Some(query)) = QUERY.get() else {
        return Err(-1);
    };
    let mut out = std::ptr::null_mut();
    match query(&provider, &iid, &mut out) {
        S_OK if !out.is_null() => Ok(Object(out)),
        hr => Err(hr),
    }
}
