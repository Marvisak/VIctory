//! Which Direct3D 12 runtime the host uses: the one in Windows, or, only when
//! that one cannot do the job, the DirectX Agility SDK's.
//!
//! The translated shaders need root signatures with the shader model 6.6
//! flags `CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED` and `SAMPLER_HEAP_DIRECTLY_INDEXED`.
//! The runtime in Windows 10 refuses them ("Unsupported bit-flag set"), so no
//! host pipeline can be made; Windows 11's accepts them.
//!
//! The first use tries the system runtime: if it serializes such a root
//! signature, nothing else is ever loaded. If it does not, and
//! `D3D12\D3D12Core.dll` (from the `Microsoft.Direct3D.D3D12` package,
//! `build/native/bin/x64`) is next to the executable, a device factory for that
//! runtime makes the devices and serializes the root signatures. Without the
//! file the system runtime stays in use and [`description`] says what to add.
//!
//! Every host device and root signature of the host goes through
//! [`create_device`] and [`serialize_versioned_root_signature`], so they
//! cannot end up on different runtimes.

use std::ffi::CString;
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{IUnknown, Interface, PCSTR, Param};

/// The Agility SDK release (1.`AGILITY_SDK_VERSION`.x) whose `D3D12Core.dll`
/// is expected.
pub const AGILITY_SDK_VERSION: u32 = 620;

/// Where the Agility runtime is looked for, next to the executable.
const AGILITY_FOLDER: &str = "D3D12";

enum Runtime {
    /// The runtime in Windows.
    System { why: String },
    /// The Agility SDK's, through a device factory.
    Agility {
        factory: ID3D12DeviceFactory,
        config: ID3D12DeviceConfiguration,
        path: String,
    },
}

// SAFETY: Direct3D 12 objects are free-threaded.
unsafe impl Send for Runtime {}
// SAFETY: as above.
unsafe impl Sync for Runtime {}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(choose)
}

/// A root signature without parameters that carries the flags the translated
/// shaders need.
fn probe_description() -> D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
    D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                    | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
                ..Default::default()
            },
        },
    }
}

fn choose() -> Runtime {
    let probe = probe_description();
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: the description is live and has no arrays; the blob is dropped.
    let system = unsafe { D3D12SerializeVersionedRootSignature(&probe, &mut blob, None) };
    let Err(refused) = system else {
        return Runtime::System {
            why: "the runtime in Windows accepts the root signature flags".into(),
        };
    };
    match agility(&probe) {
        Ok(runtime) => runtime,
        Err(why) => Runtime::System {
            why: format!(
                "the runtime in Windows refuses the root signature flags ({refused}) and the \
                 Agility SDK is not usable: {why}"
            ),
        },
    }
}

/// The Agility runtime, if its file is there and it serializes the probe.
fn agility(probe: &D3D12_VERSIONED_ROOT_SIGNATURE_DESC) -> Result<Runtime, String> {
    let exe = std::env::current_exe().map_err(|e| format!("no executable path: {e}"))?;
    let folder = exe
        .parent()
        .ok_or("the executable has no folder")?
        .join(AGILITY_FOLDER);
    if !folder.join("D3D12Core.dll").is_file() {
        return Err(format!(
            "{} not found; copy D3D12Core.dll from the Microsoft.Direct3D.D3D12 NuGet package \
             {AGILITY_SDK_VERSION}.x (build/native/bin/x64) there",
            folder.join("D3D12Core.dll").display()
        ));
    }
    let mut path = folder.to_string_lossy().trim_start_matches(r"\\?\").to_string();
    path.push('\\');
    let c_path = CString::new(path.clone()).map_err(|_| "the folder name has a NUL")?;

    // SAFETY: plain interface queries with valid out pointers; the path
    // outlives the call.
    unsafe {
        let mut sdk: Option<ID3D12SDKConfiguration1> = None;
        D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut sdk)
            .map_err(|e| format!("the runtime in Windows has no device factories: {e}"))?;
        let sdk = sdk.ok_or("no SDK configuration")?;
        let factory: ID3D12DeviceFactory = sdk
            .CreateDeviceFactory(AGILITY_SDK_VERSION, PCSTR(c_path.as_ptr().cast()))
            .map_err(|e| format!("{path}D3D12Core.dll was refused (wrong version?): {e}"))?;
        let config: ID3D12DeviceConfiguration = factory
            .cast::<ID3D12DeviceConfiguration>()
            .or_else(|e1| {
                factory
                    .GetConfigurationInterface::<ID3D12DeviceConfiguration>(
                        &CLSID_D3D12DeviceFactory,
                    )
                    .map_err(|e2| format!("{e1}; {e2}"))
            })
            .map_err(|e| format!("no device configuration: {e}"))?;
        let mut blob: Option<ID3DBlob> = None;
        config
            .SerializeVersionedRootSignature(probe, &mut blob, None)
            .map_err(|e| format!("the Agility runtime refuses the root signature flags too: {e}"))?;
        Ok(Runtime::Agility {
            factory,
            config,
            path,
        })
    }
}

/// Whether the Agility runtime is the one in use: the system runtime's
/// serializer then cannot serve callers outside this module either.
pub fn uses_agility() -> bool {
    matches!(runtime(), Runtime::Agility { .. })
}

/// Which runtime is in use and why, for the log.
pub fn description() -> String {
    match runtime() {
        Runtime::System { why } => format!("Direct3D 12 runtime of Windows: {why}"),
        Runtime::Agility { path, .. } => format!(
            "DirectX Agility SDK {AGILITY_SDK_VERSION} from {path}: the runtime in Windows \
             refuses the root signature flags the translated shaders need"
        ),
    }
}

/// Creates a feature level 12.0 device on `adapter` (`None`: the default
/// adapter) on the chosen runtime.
pub fn create_device<P: Param<IUnknown>>(adapter: P) -> windows::core::Result<ID3D12Device> {
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: `device` is a valid out pointer.
    unsafe {
        match runtime() {
            Runtime::System { .. } => {
                D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?
            }
            Runtime::Agility { factory, .. } => {
                factory.CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?
            }
        }
    }
    device.ok_or_else(windows::core::Error::empty)
}

/// `D3D12SerializeVersionedRootSignature` on the chosen runtime.
///
/// # Safety
/// The description's arrays are live; the pointers are valid for writes.
pub unsafe fn serialize_versioned_root_signature(
    desc: *const D3D12_VERSIONED_ROOT_SIGNATURE_DESC,
    blob: *mut Option<ID3DBlob>,
    error: Option<*mut Option<ID3DBlob>>,
) -> windows::core::Result<()> {
    // SAFETY: the caller's.
    unsafe {
        match runtime() {
            Runtime::System { .. } => D3D12SerializeVersionedRootSignature(desc, blob, error),
            Runtime::Agility { config, .. } => {
                config.SerializeVersionedRootSignature(desc, blob, error)
            }
        }
    }
}
