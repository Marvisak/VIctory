//! System information.
//!
//! Public reference: reference/system/xsystem (XSystemGetConsoleId,
//! XSystemGetAnalyticsInfo, XSystemAnalyticsInfo).

use std::ffi::c_void;

use crate::rt::{Guid, Hresult, S_OK, area};
use crate::tap::Tap;

/// VIctory's `xsystem::PROVIDER` and `IID`, and its analytics area.
const PROVIDER: Guid = Guid(
    0xe349_bd1a,
    0xfc20,
    0x4e40,
    [0xb9, 0x9c, 0x41, 0x78, 0xcc, 0x6b, 0x40, 0x9f],
);
const IID: Guid = Guid(
    0x6fd7_1f09,
    0x7513,
    0x49f0,
    [0x89, 0xbc, 0xbf, 0xaf, 0x5d, 0xf6, 0xf8, 0x52],
);
const ANALYTICS: Guid = Guid(
    0xb884_675d,
    0xb738,
    0x4a9c,
    [0x81, 0x5d, 0x9a, 0x9a, 0x1e, 0x0c, 0x6c, 0x9b],
);

/// HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER).
const INSUFFICIENT_BUFFER: Hresult = 0x8007_007A_u32 as i32;

/// "In RETAIL, this function will always return
/// 00000000.00000000.00000000.00000000.00" (XSystemGetConsoleId).
const RETAIL_CONSOLE_ID: &str = "00000000.00000000.00000000.00000000.00";

/// XVersion.
#[repr(C)]
#[derive(Clone, Copy)]
struct Version {
    major: u16,
    minor: u16,
    build: u16,
    revision: u16,
}

/// XSystemAnalyticsInfo.
#[repr(C)]
struct AnalyticsInfo {
    os: Version,
    hosting_os: Version,
    family: [u8; 64],
    form: [u8; 64],
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn run(t: &mut Tap) {
    match area(ANALYTICS, ANALYTICS) {
        Ok(object) => {
            let mut info = std::mem::MaybeUninit::<AnalyticsInfo>::zeroed();
            // SAFETY: the slot's signature in VIctory's table (the struct is
            // returned through a hidden pointer, as the ABI does for it).
            let get: extern "system" fn(*mut c_void, *mut AnalyticsInfo) -> *mut AnalyticsInfo =
                unsafe { object.method(0x18) };
            get(object.this(), info.as_mut_ptr());
            // SAFETY: written by the call (zeroed before).
            let info = unsafe { info.assume_init() };
            t.eq(
                text(&info.family).as_str(),
                "Xbox",
                "xsystem: analytics family is \"Xbox\" on a console",
            );
            t.check(
                !text(&info.form).is_empty(),
                "xsystem: analytics form names the hardware",
                || "empty".into(),
            );
            t.check(
                info.os.major != 0 || info.os.build != 0,
                "xsystem: analytics OS version is set",
                || "zero".into(),
            );
            let _ = info.hosting_os;
        }
        Err(hr) => t.check(false, "xsystem: the analytics area is served", || {
            format!("QueryApiImpl {hr:#010x}")
        }),
    }

    match area(PROVIDER, IID) {
        Ok(object) => {
            // SAFETY: the slot's signature in VIctory's table.
            let get: extern "system" fn(*mut c_void, usize, *mut u8, *mut usize) -> Hresult =
                unsafe { object.method(0x18) };
            let mut buffer = [0u8; 64];
            let mut used = 0;
            let hr = get(object.this(), buffer.len(), buffer.as_mut_ptr(), &mut used);
            t.eq(hr, S_OK, "xsystem: XSystemGetConsoleId succeeds");
            t.eq(
                text(&buffer).as_str(),
                RETAIL_CONSOLE_ID,
                "xsystem: a retail console id is the documented zero id",
            );
            t.eq(
                used,
                RETAIL_CONSOLE_ID.len() + 1,
                "xsystem: ... and its size counts the NUL",
            );
            let mut short = [0u8; 8];
            let hr = get(object.this(), short.len(), short.as_mut_ptr(), &mut used);
            t.eq(
                hr,
                INSUFFICIENT_BUFFER,
                "xsystem: a short console id buffer is ERROR_INSUFFICIENT_BUFFER",
            );
        }
        Err(hr) => t.check(false, "xsystem: the system area is served", || {
            format!("QueryApiImpl {hr:#010x}")
        }),
    }
}
