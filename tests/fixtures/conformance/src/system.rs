//! Networking hints, the package and HDR display mode.
//!
//! Public reference:
//! - reference/networking/xnetworking: XNetworkingConnectivityHint (level,
//!   cost, IANA interface type, networkInitialized, three flags); levels
//!   Unknown 0 to ConstrainedInternetAccess 4, costs Unknown 0 to Variable 3;
//!   the preferred local UDP multiplayer port "defaults to UDP 3074".
//! - reference/system/xpackage: XPackageIsPackagedProcess is true for a
//!   process with an installation identity; the identifier is a string.
//! - reference/system/xdisplay: XDisplayTryEnableHdrMode returns Unknown 0,
//!   Enabled 1 or Disabled 2; enabled, its info defaults to 0.01, 1000 and
//!   1000 nits.
//!
//! The areas' ids and method slots are VIctory's (`xnetworking.rs`,
//! `xpackage.rs`, `display.rs` in `crates/victory-hle/src/modules/`).

use std::ffi::c_void;

use crate::rt::{Guid, Hresult, S_OK, area};
use crate::tap::Tap;
use crate::xasync::{Block, wait};

const NETWORKING: Guid = Guid(
    0x37e5_6907,
    0x2f10,
    0x41e8,
    [0xb7, 0x2f, 0x36, 0xed, 0xb1, 0x85, 0x33, 0x1a],
);
const PACKAGE: Guid = Guid(
    0xaf40_6016,
    0xe850,
    0x4aa8,
    [0xa8, 0x8d, 0x2f, 0x3d, 0xcb, 0x9d, 0xac, 0x7e],
);
const PACKAGE_IID: Guid = Guid(
    0x0b9d_556e,
    0xab70,
    0x43f4,
    [0xba, 0xa0, 0x99, 0x99, 0xac, 0x4c, 0xb0, 0x40],
);
const HDR: Guid = Guid(
    0x03f0_fe74,
    0xfdd9,
    0x4e5c,
    [0xb6, 0x30, 0xf9, 0x33, 0x9c, 0x47, 0xac, 0xc5],
);
const HDR_IID: Guid = Guid(
    0x35f0_7670,
    0x706e,
    0x4bfb,
    [0x94, 0x76, 0x09, 0x07, 0x98, 0xc5, 0xeb, 0xf3],
);

/// XNetworkingConnectivityHint.
#[repr(C)]
#[derive(Default, Debug)]
struct Hint {
    level: u32,
    cost: u32,
    iana_interface_type: u32,
    network_initialized: bool,
    approaching_data_limit: bool,
    over_data_limit: bool,
    roaming: bool,
}

/// XDisplayHdrModeInfo.
#[repr(C)]
#[derive(Default, Debug, PartialEq)]
struct HdrInfo {
    min_tone_map_luminance: f32,
    max_tone_map_luminance: f32,
    max_full_frame_tone_map_luminance: f32,
}

pub fn run(t: &mut Tap) {
    match area(NETWORKING, NETWORKING) {
        Ok(o) => {
            let mut hint = Hint::default();
            // SAFETY: (all slots) the signatures in VIctory's tables.
            let get: extern "system" fn(*mut c_void, *mut Hint) -> Hresult =
                unsafe { o.method(0x78) };
            t.eq(
                get(o.this(), &mut hint),
                S_OK,
                "xnetworking: XNetworkingGetConnectivityHint succeeds",
            );
            t.check(
                hint.network_initialized,
                "xnetworking: the hint says the network is initialised",
                || format!("{hint:?}"),
            );
            t.check(
                hint.level <= 4 && hint.cost <= 3,
                "xnetworking: the level and cost hints are documented values",
                || format!("{hint:?}"),
            );
            let mut port = 0u16;
            let sync: extern "system" fn(*mut c_void, *mut u16) -> Hresult =
                unsafe { o.method(0x18) };
            t.eq(
                (sync(o.this(), &mut port), port),
                (S_OK, 3074),
                "xnetworking: the preferred local UDP multiplayer port is the documented default 3074",
            );
            let mut block = Block::on_process_queue();
            let begin: extern "system" fn(*mut c_void, *mut Block) -> Hresult =
                unsafe { o.method(0x20) };
            let result: extern "system" fn(*mut c_void, *mut Block, *mut u16) -> Hresult =
                unsafe { o.method(0x28) };
            let mut port = 0u16;
            let hr = begin(o.this(), &mut block);
            if hr == S_OK {
                wait(&mut block);
            }
            t.eq(
                (hr, result(o.this(), &mut block, &mut port), port),
                (S_OK, S_OK, 3074),
                "xnetworking: the async port query gives the same port",
            );
        }
        Err(hr) => t.check(false, "xnetworking: the networking area is served", || {
            format!("QueryApiImpl {hr:#010x}")
        }),
    }

    match area(PACKAGE, PACKAGE_IID) {
        Ok(o) => {
            let packaged: extern "system" fn(*mut c_void) -> bool = unsafe { o.method(0x20) };
            t.eq(
                packaged(o.this()),
                true,
                "xpackage: XPackageIsPackagedProcess is true for an installed title",
            );
            let id: extern "system" fn(*mut c_void, usize, *mut u8) -> Hresult =
                unsafe { o.method(0x18) };
            let mut buffer = [0u8; 128];
            let hr = id(o.this(), buffer.len(), buffer.as_mut_ptr());
            let len = buffer.iter().position(|&b| b == 0);
            t.check(
                hr == S_OK && len.is_some_and(|n| n > 0),
                "xpackage: the package identifier is a non-empty string",
                || format!("{hr:#010x} {len:?}"),
            );
            let mut short = [0u8; 1];
            t.check(
                id(o.this(), short.len(), short.as_mut_ptr()) != S_OK,
                "xpackage: an identifier buffer too small for it is refused",
                || "accepted".into(),
            );
        }
        Err(hr) => t.check(false, "xpackage: the package area is served", || {
            format!("QueryApiImpl {hr:#010x}")
        }),
    }

    match area(HDR, HDR_IID) {
        Ok(o) => {
            let mut info = HdrInfo::default();
            let try_enable: extern "system" fn(*mut c_void, u32, *mut HdrInfo) -> u32 =
                unsafe { o.method(0x20) };
            let result = try_enable(o.this(), 0, &mut info);
            t.check(
                result == 1 || result == 2,
                "xdisplay: XDisplayTryEnableHdrMode says Enabled or Disabled",
                || format!("{result}"),
            );
            if result == 1 {
                t.eq(
                    info,
                    HdrInfo {
                        min_tone_map_luminance: 0.01,
                        max_tone_map_luminance: 1000.0,
                        max_full_frame_tone_map_luminance: 1000.0,
                    },
                    "xdisplay: enabled HDR reports the documented default luminances",
                );
            }
            t.check(
                (try_enable(o.this(), 1, std::ptr::null_mut()) as i32) <= 2,
                "xdisplay: the mode info is optional",
                || "out of range".into(),
            );
        }
        Err(hr) => t.check(false, "xdisplay: the HDR area is served", || {
            format!("QueryApiImpl {hr:#010x}")
        }),
    }
}
