//! AMD Radeon Anti-Lag 2 (`VICTORY_ANTILAG2=1`; off when unset or `0`).
//!
//! The title asks for none; the layer asks the driver for it on the host
//! device. Anti-Lag 2 is a driver feature with a header-only SDK, so there is
//! no DLL to ship. The ABI used here follows AMD's `ffx_antilag2_dx12.h`
//! (FidelityFX SDK, MIT License, Copyright (C) Advanced Micro Devices, Inc.):
//!
//! - the Radeon user-mode driver, `amdxc64.dll`, already in the process once
//!   a D3D12 device exists on an AMD card, exports
//!   `AmdExtD3DCreateInterface(IUnknown* outer, REFIID, void**)`;
//! - asked for `IAmdExtAntiLagApi` on the device, it gives a COM object with
//!   one method after `IUnknown`: `UpdateAntiLagState(void* data)`;
//! - `data` points to a 32-byte structure to set the state (mode 1 on, 2 off,
//!   and a frame rate limit), or is null to take the frame's delay.
//!
//! [`frame_start`] is the header's `Update`: once per frame, right after a
//! present, before the title goes on to read the input for the next frame,
//! under no lock of the layer's. The driver sleeps in it for as long as it
//! judges the frame would otherwise wait behind the card.
//!
//! Without the driver, the export or the interface (another vendor's card, an
//! old driver, a card the driver gives no Anti-Lag 2 on), one line says
//! `antilag2: unavailable (...)` and nothing else happens. The interface is
//! kept for the life of the process.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Once, OnceLock};

use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::{GUID, Interface, PCSTR, w};

/// `IAmdExtAntiLagApi`.
static ANTI_LAG_API: GUID = GUID::from_u128(0x44085fbe_e839_40c5_bf38_0ebc5ab4d0a6);

/// The header's `APIData_v1`: sets the state.
#[repr(C)]
struct StateData {
    size: u32,
    version: u32,
    mode: u32,
    control: *const u8,
    control_length: u32,
    max_fps: u32,
}

const _: () = {
    assert!(size_of::<StateData>() == 32);
    assert!(std::mem::offset_of!(StateData, control) == 16);
    assert!(std::mem::offset_of!(StateData, max_fps) == 28);
};

const MODE_ON: u32 = 1;
const MODE_OFF: u32 = 2;

/// What sets Anti-Lag on or off, with no frame rate limit.
fn state_data(enabled: bool) -> StateData {
    StateData {
        size: size_of::<StateData>() as u32,
        version: 1,
        mode: if enabled { MODE_ON } else { MODE_OFF },
        control: std::ptr::null(),
        control_length: 0,
        max_fps: 0,
    }
}

/// Whether the driver took an update: `S_OK`, or `S_FALSE` (nothing to do).
fn taken(hresult: i32) -> bool {
    hresult == 0 || hresult == 1
}

/// `IAmdExtAntiLagApi`'s methods: `IUnknown`'s, then its one.
#[repr(C)]
struct Methods {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    update: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
}

type CreateInterface = unsafe extern "C" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32;

/// One reference to the driver's interface.
struct Api(*mut c_void);

// SAFETY: a driver object that AMD's header lets any thread call; the layer
// calls it from the presenting thread.
unsafe impl Send for Api {}
// SAFETY: see above.
unsafe impl Sync for Api {}

impl Api {
    /// The driver's interface on `device`, with Anti-Lag left off, as the
    /// header's `Initialize` does. Why not, otherwise.
    fn new(device: *mut c_void) -> Result<Self, String> {
        // SAFETY: a module name; the driver's DLL is only looked up, never
        // loaded (it is there if the device is AMD's).
        let module = unsafe { GetModuleHandleW(w!("amdxc64.dll")) }
            .map_err(|_| "no AMD Radeon driver in the process".to_owned())?;
        // SAFETY: a loaded module and a nul-terminated name.
        let create =
            unsafe { GetProcAddress(module, PCSTR(c"AmdExtD3DCreateInterface".as_ptr().cast())) }
                .ok_or("the driver has no AmdExtD3DCreateInterface")?;
        // SAFETY: the driver's export, of this type (`ffx_antilag2_dx12.h`).
        let create = unsafe {
            std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateInterface>(create)
        };
        let mut api = std::ptr::null_mut();
        // SAFETY: the driver's call with a live device and our out pointer.
        let hr = unsafe { create(device, &raw const ANTI_LAG_API, &raw mut api) };
        if hr != 0 || api.is_null() {
            return Err(format!("no Anti-Lag 2 interface on this card: {hr:#010x}"));
        }
        let api = Self(api);
        let hr = api.set(false);
        if hr != 0 {
            // SAFETY: our one reference, released once.
            unsafe { (api.methods().release)(api.0) };
            return Err(format!(
                "the driver refused the Anti-Lag 2 state: {hr:#010x}"
            ));
        }
        Ok(api)
    }

    fn methods(&self) -> &Methods {
        // SAFETY: a COM object: its first word points to its methods, laid
        // out as `Methods` says.
        unsafe { &**self.0.cast::<*const Methods>() }
    }

    /// Switches Anti-Lag on or off.
    fn set(&self, enabled: bool) -> i32 {
        let mut data = state_data(enabled);
        // SAFETY: the live interface and the header's structure.
        unsafe { (self.methods().update)(self.0, (&raw mut data).cast()) }
    }

    /// Takes the frame's delay: the driver may sleep here.
    fn delay(&self) -> i32 {
        // SAFETY: the live interface; null asks for the frame's delay.
        unsafe { (self.methods().update)(self.0, std::ptr::null_mut()) }
    }
}

/// The interface on the host device if `VICTORY_ANTILAG2` asks for it and
/// the driver gives it; asked once, and the answer logged once.
fn api() -> Option<&'static Api> {
    static ASKED: OnceLock<bool> = OnceLock::new();
    static API: OnceLock<Option<Api>> = OnceLock::new();
    if !*ASKED.get_or_init(|| crate::switch::on("VICTORY_ANTILAG2")) {
        return None;
    }
    if let Some(api) = API.get() {
        return api.as_ref();
    }
    let host = crate::host::host()?;
    API.get_or_init(|| match Api::new(host.device.as_raw()) {
        Ok(api) => {
            tracing::info!("antilag2: active on the host device (VICTORY_ANTILAG2; driver interface IAmdExtAntiLagApi)");
            Some(api)
        }
        Err(why) => {
            tracing::warn!("antilag2: unavailable ({why}); VICTORY_ANTILAG2 does nothing");
            None
        }
    })
    .as_ref()
}

/// The header's `Update`, at a frame's start: Anti-Lag switched on the first
/// time, then the driver's delay for this frame. Does nothing unless asked.
pub(crate) fn frame_start() {
    let Some(api) = api() else {
        return;
    };
    static ENABLED: AtomicBool = AtomicBool::new(false);
    if !ENABLED.swap(true, Ordering::Relaxed) {
        let hr = api.set(true);
        if !taken(hr) {
            tracing::warn!("antilag2: the driver did not switch Anti-Lag on: {hr:#010x}");
        }
    }
    let hr = api.delay();
    if !taken(hr) {
        static REPORTED: Once = Once::new();
        REPORTED.call_once(|| {
            tracing::warn!("antilag2: the driver refused a frame's update: {hr:#010x}")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_is_set_as_the_header_does() {
        let on = state_data(true);
        assert_eq!(
            (on.size, on.version, on.mode, on.max_fps, on.control_length),
            (32, 1, MODE_ON, 0, 0)
        );
        assert!(on.control.is_null());
        assert_eq!(state_data(false).mode, MODE_OFF);
    }

    #[test]
    fn nothing_to_do_is_not_a_refusal() {
        assert!(taken(0) && taken(1));
        // E_NOINTERFACE, E_INVALIDARG.
        assert!(!taken(0x8000_4002_u32 as i32) && !taken(0x8007_0057_u32 as i32));
    }

    /// The driver itself, on the default adapter: run by hand on an AMD card
    /// with `cargo test -p victory-gpu antilag2 -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs an AMD Radeon driver"]
    fn the_radeon_driver_takes_frames() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device};

        let mut device: Option<ID3D12Device> = None;
        // SAFETY: the default adapter and an out pointer of ours.
        unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device) }
            .expect("a D3D12 device");
        let device = device.expect("made");
        let api = Api::new(device.as_raw()).unwrap_or_else(|why| panic!("{why}"));
        assert!(taken(api.set(true)));
        let start = std::time::Instant::now();
        for _ in 0..120 {
            assert!(taken(api.delay()));
        }
        println!("120 frame updates taken in {:?}", start.elapsed());
        assert!(taken(api.set(false)));
        // SAFETY: our one reference, released once.
        unsafe { (api.methods().release)(api.0) };
    }
}
