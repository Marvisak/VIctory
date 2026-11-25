//! XPackage: the title's own installation, complete and launchable, with
//! every chunk ready; no other package (content, add-ons) is installed.
//! The package is the folder of the title's executable (`G:` to the title).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CString, c_char, c_void};
use std::path::Path;
use std::sync::OnceLock;

use super::xasync::{self, AsyncBlock};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, put_string, text, token,
};
use crate::runtime;

pub const PROVIDER: Guid = Guid::new(
    0xaf40_6016,
    0xe850,
    0x4aa8,
    [0xa8, 0x8d, 0x2f, 0x3d, 0xcb, 0x9d, 0xac, 0x7e],
);

/// As Grand Theft Auto VI (2026) asks for it. Its slots up to 0xd0 take the
/// arguments the public API lists in the same order; 0x110 uninstalls a
/// package, 0x128/0x130 mount one with UI, 0x138 enumerates packages.
pub const IID_2026: Guid = Guid::new(
    0x0b9d_556e,
    0xab70,
    0x43f4,
    [0xba, 0xa0, 0x99, 0x99, 0xac, 0x4c, 0xb0, 0x40],
);

pub static OBJECT: Object = Object::new("package", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID_2026, &OBJECT)];

/// The documented "could not resolve an identifier for the package".
pub const NO_PACKAGE: HRESULT = 0x8924_5208_u32 as i32;
const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;

/// Our installation's identifier: the title id in hex (titles keep it in 33
/// bytes and hand it back to us).
pub fn identifier() -> String {
    format!("{:08X}", runtime::title().id)
}

fn is_ours(id: *const c_char) -> bool {
    text(id).is_some_and(|id| id == identifier())
}

/// Bytes in the package folder, counted once.
fn package_bytes() -> u64 {
    fn size(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => size(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let exe = runtime::main_image().and_then(runtime::image_path);
        exe.as_deref().and_then(Path::parent).map_or(0, size)
    })
}

type This = *const Object;

extern "system" fn current_identifier(_: This, size: usize, buffer: *mut u8) -> HRESULT {
    put_string(&identifier(), size, buffer, std::ptr::null_mut())
}

extern "system" fn is_packaged(_: This) -> bool {
    true
}

/// Installation monitors all watch the same finished installation, so the
/// handle is a fixed address.
static MONITOR: u8 = 0;
fn monitor() -> *const c_void {
    (&raw const MONITOR).cast()
}

extern "system" fn create_monitor(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _queue: *const c_void,
    out: *mut *const c_void,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, monitor())
}
extern "system" fn close_monitor(_: This, _: *const c_void) {}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub total_bytes: u64,
    pub installed_bytes: u64,
    pub launch_bytes: u64,
    pub launchable: bool,
    pub completed: bool,
}

extern "system" fn progress(_: This, _: *const c_void, out: *mut Progress) {
    let bytes = package_bytes();
    put(
        out,
        Progress {
            total_bytes: bytes,
            installed_bytes: bytes,
            launch_bytes: bytes,
            launchable: true,
            completed: true,
        },
    );
}

/// The installation is complete, so progress never changes.
extern "system" fn register_progress(
    _: This,
    _monitor: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put(out, token())
}

/// The completed installation has no pending progress callbacks to wait for.
extern "system" fn unregister_progress(
    _: This,
    _monitor: *const c_void,
    _token: u64,
    _wait: bool,
) -> bool {
    true
}

/// 0: ready.
// shortcut: every selector counts as installed, including ones the
// package's layout does not have (the console answers those with an
// invalid-selector error); Grand Theft Auto VI asks only about its own
// chunks, which a full install has.
extern "system" fn find_availability(
    _: This,
    id: *const c_char,
    _selector_count: u32,
    _selectors: *const c_void,
    out: *mut u32,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    put(out, 0)
}

extern "system" fn change_order(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
) -> HRESULT {
    if is_ours(id) { S_OK } else { NO_PACKAGE }
}

static INSTALL: u8 = 0;

/// Everything is installed: done at once, the result is a monitor.
extern "system" fn install_chunks_async(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    _interval_ms: u32,
    _suppress_confirmation: bool,
    block: *mut AsyncBlock,
) -> HRESULT {
    let result = if is_ours(id) {
        Ok(xasync::bytes(monitor()))
    } else {
        Err(NO_PACKAGE)
    };
    xasync::finished_with(block, (&raw const INSTALL).cast(), result)
}
extern "system" fn install_chunks_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *const c_void,
) -> HRESULT {
    xasync::result(block, (&raw const INSTALL).cast(), out)
}

/// Nothing to download.
extern "system" fn estimate_download(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
    bytes: *mut u64,
    confirm: *mut bool,
) -> HRESULT {
    if !is_ours(id) {
        return NO_PACKAGE;
    }
    if !confirm.is_null() {
        // SAFETY: checked non-null.
        unsafe { confirm.write(false) };
    }
    put(bytes, 0)
}

// shortcut: accepted but nothing is removed (the package folder is the
// user's), so the chunks stay ready; Grand Theft Auto VI's one call site
// (title+0x293b4b0, its chunk manager) would then see them still installed.
extern "system" fn uninstall_chunks(
    _: This,
    id: *const c_char,
    _count: u32,
    _selectors: *const c_void,
) -> HRESULT {
    if is_ours(id) { S_OK } else { NO_PACKAGE }
}

/// Mount handles only come from mounting another package, and there is
/// none.
extern "system" fn mount_path(_: This, _: *const c_void, _: usize, _: *mut u8) -> HRESULT {
    xasync::E_INVALIDARG
}
extern "system" fn close_mount(_: This, _: *const c_void) {}

/// Only other packages are uninstalled this way, and none is installed.
extern "system" fn uninstall_package(_: This, id: *const c_char) -> bool {
    tracing::info!(
        id = text(id).unwrap_or_default(),
        "package uninstall refused"
    );
    false
}

static MOUNT: u8 = 0;

extern "system" fn mount_async(_: This, id: *const c_char, block: *mut AsyncBlock) -> HRESULT {
    tracing::info!(
        id = text(id).unwrap_or_default(),
        "package mount: not installed"
    );
    xasync::finished(block, (&raw const MOUNT).cast(), NO_PACKAGE)
}
extern "system" fn mount_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut *const c_void,
) -> HRESULT {
    xasync::result(block, (&raw const MOUNT).cast(), out)
}

/// The details record the title's enumeration callback reads.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PackageDetails {
    pub package_identifier: *const c_char,
    pub version: u64,
    pub kind: u32,
    pub display_name: *const c_char,
    pub description: *const c_char,
    pub publisher: *const c_char,
    pub store_id: *const c_char,
    pub installing: bool,
    pub index: u32,
    pub count: u32,
    pub age_restricted: bool,
    pub title_id: *const c_char,
}

type EnumerateCallback = extern "system" fn(*mut c_void, *const PackageDetails) -> bool;

/// Enumerates packages of `kind`: `0` (`Game`) yields this installation;
/// `1` (`Content`) yields none.
extern "system" fn enumerate(
    _: This,
    kind: u32,
    _scope: u32,
    context: *mut c_void,
    callback: *const c_void,
) -> HRESULT {
    if callback.is_null() {
        return E_INVALIDARG;
    }
    if kind != 0 {
        tracing::debug!(kind, "packages enumerated: none");
        return S_OK;
    }
    let id = CString::new(identifier()).unwrap_or_default();
    let store = CString::new(runtime::title().store_id).unwrap_or_default();
    let title_id = CString::new(format!("{:08X}", runtime::title().id)).unwrap_or_default();
    let empty = c"";
    let details = PackageDetails {
        package_identifier: id.as_ptr(),
        version: 0,
        kind: 0,
        display_name: empty.as_ptr(),
        description: empty.as_ptr(),
        publisher: empty.as_ptr(),
        store_id: store.as_ptr(),
        installing: false,
        index: 0,
        count: 1,
        age_restricted: false,
        title_id: title_id.as_ptr(),
    };
    // SAFETY: `callback` is non-null and follows the title's enumeration callback ABI;
    // `id`, `store`, `title_id`, and `empty` live for the duration of the synchronous call.
    let cb: EnumerateCallback = unsafe { std::mem::transmute(callback) };
    let _ = cb(context, &details);
    S_OK
}

static VTABLE: Methods<40> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    current_identifier as *const (),    // 0x18
    is_packaged as *const (),           // 0x20
    create_monitor as *const (),        // 0x28
    close_monitor as *const (),         // 0x30
    progress as *const (),              // 0x38
    missing::<8> as *const (),          // 0x40 update a monitor
    register_progress as *const (),     // 0x48
    unregister_progress as *const (),   // 0x50
    missing::<11> as *const (),         // 0x58 user locale
    find_availability as *const (),     // 0x60
    missing::<13> as *const (),         // 0x68 enumerate chunk availability
    change_order as *const (),          // 0x70
    missing::<15> as *const (),         // 0x78 install chunks
    install_chunks_async as *const (),  // 0x80
    install_chunks_result as *const (), // 0x88
    estimate_download as *const (),     // 0x90
    uninstall_chunks as *const (),      // 0x98
    missing::<20> as *const (),         // 0xa0
    missing::<21> as *const (),         // 0xa8
    missing::<22> as *const (),         // 0xb0
    missing::<23> as *const (),         // 0xb8 mount (deprecated)
    missing::<24> as *const (),         // 0xc0 mount path size
    mount_path as *const (),            // 0xc8
    close_mount as *const (),           // 0xd0
    missing::<27> as *const (),         // 0xd8
    missing::<28> as *const (),         // 0xe0 enumerate packages (older)
    missing::<29> as *const (),         // 0xe8
    missing::<30> as *const (),         // 0xf0
    missing::<31> as *const (),         // 0xf8
    missing::<32> as *const (),         // 0x100
    missing::<33> as *const (),         // 0x108
    uninstall_package as *const (),     // 0x110
    missing::<35> as *const (),         // 0x118
    missing::<36> as *const (),         // 0x120
    mount_async as *const (),           // 0x128
    mount_result as *const (),          // 0x130
    enumerate as *const (),             // 0x138
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    fn completed_installation_unregisters_progress_immediately() {
        type Register =
            extern "system" fn(This, *const c_void, *mut c_void, *const c_void, *mut u64) -> HRESULT;
        type Unregister = extern "system" fn(This, *const c_void, u64, bool) -> bool;
        extern "system" fn callback(_: *mut c_void, _: *const c_void) {}

        let this: This = &OBJECT;
        let register = method::<Register>(&OBJECT, 0x48);
        assert_eq!(VTABLE.0[0x50 / 8], unregister_progress as *const ());
        let unregister = method::<Unregister>(&OBJECT, 0x50);
        for wait in [false, true] {
            let mut token = 0;
            assert_eq!(
                register(
                    this,
                    monitor(),
                    std::ptr::null_mut(),
                    callback as *const c_void,
                    &mut token
                ),
                S_OK
            );
            assert_ne!(token, 0);
            assert!(unregister(this, monitor(), token, wait));
        }
    }

    #[test]
    fn the_installation_is_complete_and_alone() {
        let this: This = &OBJECT;
        type Id = extern "system" fn(This, usize, *mut u8) -> HRESULT;
        let mut id = [0u8; 33];
        assert_eq!(method::<Id>(&OBJECT, 0x18)(this, 33, id.as_mut_ptr()), S_OK);
        let len = id.iter().position(|&b| b == 0).unwrap();
        assert_eq!(std::str::from_utf8(&id[..len]).unwrap(), identifier());
        assert!(method::<extern "system" fn(This) -> bool>(&OBJECT, 0x20)(
            this
        ));

        type Find =
            extern "system" fn(This, *const c_char, u32, *const c_void, *mut u32) -> HRESULT;
        let mut availability = 9;
        let ours = format!("{}\0", identifier());
        let find = method::<Find>(&OBJECT, 0x60);
        let none = std::ptr::null();
        assert_eq!(
            find(this, ours.as_ptr().cast(), 0, none, &mut availability),
            S_OK
        );
        assert_eq!(availability, 0, "ready");
        assert_eq!(
            find(this, c"dlc".as_ptr(), 0, none, &mut availability),
            NO_PACKAGE
        );

        type Progress_ = extern "system" fn(This, *const c_void, *mut Progress);
        let mut p = Progress {
            total_bytes: 1,
            installed_bytes: 0,
            launch_bytes: 0,
            launchable: false,
            completed: false,
        };
        method::<Progress_>(&OBJECT, 0x38)(this, monitor(), &mut p);
        assert!(p.completed && p.launchable && p.installed_bytes == p.total_bytes);

        type Mount = extern "system" fn(This, *const c_char, *mut AsyncBlock) -> HRESULT;
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            method::<Mount>(&OBJECT, 0x128)(this, c"dlc".as_ptr(), &mut block),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut block, true), NO_PACKAGE);

        type Enum = extern "system" fn(This, u32, u32, *mut c_void, *const c_void) -> HRESULT;
        let enum_fn = method::<Enum>(&OBJECT, 0x138);
        assert_eq!(enum_fn(this, 0, 0, std::ptr::null_mut(), std::ptr::null()), E_INVALIDARG);

        #[derive(Default)]
        struct Seen {
            calls: usize,
            id: String,
            title_id: String,
            kind: u32,
            index: u32,
            count: u32,
            installing: bool,
            age_restricted: bool,
        }
        extern "system" fn on_pkg(ctx: *mut c_void, details: *const PackageDetails) -> bool {
            // SAFETY: test passes `&mut Seen` and a valid `PackageDetails`.
            let (seen, d) = unsafe { (&mut *ctx.cast::<Seen>(), &*details) };
            seen.calls += 1;
            seen.id = text(d.package_identifier).unwrap_or_default().to_owned();
            seen.title_id = text(d.title_id).unwrap_or_default().to_owned();
            seen.kind = d.kind;
            seen.index = d.index;
            seen.count = d.count;
            seen.installing = d.installing;
            seen.age_restricted = d.age_restricted;
            true
        }
        let mut seen = Seen::default();
        assert_eq!(
            enum_fn(this, 0, 0, (&raw mut seen).cast(), on_pkg as *const c_void),
            S_OK
        );
        assert_eq!(seen.calls, 1);
        assert_eq!(seen.id, identifier());
        assert_eq!(seen.title_id, format!("{:08X}", runtime::title().id));
        assert_eq!((seen.kind, seen.index, seen.count), (0, 0, 1));
        assert!(!seen.installing && !seen.age_restricted);

        let mut content_seen = Seen::default();
        assert_eq!(
            enum_fn(this, 1, 0, (&raw mut content_seen).cast(), on_pkg as *const c_void),
            S_OK
        );
        assert_eq!(content_seen.calls, 0);
    }
}
