#![cfg(windows)]

//! Optional DirectX 12 Agility SDK loading through `ID3D12SDKConfiguration1`.
//!
//! Windows 10's inbox `D3D12Core.dll` stops at Shader Model 6.5. On Windows 10
//! 22H2 (19045.3393+, KB5029331) and Windows 11, an application can supply a newer
//! app-local runtime without static PE exports by calling
//! `ID3D12SDKConfiguration1::CreateDeviceFactory`.
//!
//! Search order, first folder with a valid x64 `D3D12Core.dll` exporting
//! `D3D12SDKVersion` wins:
//!
//! 1. `%VICTORY_D3D12_DIR%` (explicit; a bad path is reported, not ignored),
//! 2. `<exe_dir>\D3D12`,
//! 3. `<exe_dir>`,
//! 4. `%LOCALAPPDATA%\Victory\D3D12`.
//!
//! When no SDK is found, or the OS cannot activate it, device creation falls
//! back to inbox `D3D12CreateDevice`.

use std::ffi::{CStr, CString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCSTR, Param};

/// Environment variable pointing at the folder that contains `D3D12Core.dll`.
pub const ENV: &str = "VICTORY_D3D12_DIR";

#[derive(Debug)]
pub enum State {
    Loaded {
        factory: ID3D12DeviceFactory,
        version: u32,
        dir: PathBuf,
    },
    NotFound,
    Unavailable,
}

static STATE: OnceLock<State> = OnceLock::new();

pub fn state() -> &'static State {
    STATE.get_or_init(init)
}

fn clean_env_path(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    (!path.as_os_str().is_empty()).then_some(path)
}

fn candidates(env_dir: Option<PathBuf>, exe_dir: Option<PathBuf>, local_app_data: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = env_dir {
        dirs.push(dir);
        return dirs;
    }
    if let Some(dir) = exe_dir {
        dirs.push(dir.join("D3D12"));
        dirs.push(dir);
    }
    if let Some(dir) = local_app_data {
        dirs.push(dir.join("Victory").join("D3D12"));
    }
    dirs
}

/// Map an RVA to a file offset through the PE section table.
fn rva_to_offset(pe: &victory_pe::ImageInfo, rva: u32) -> Option<usize> {
    for section in &pe.sections {
        let span = section.virtual_size.max(section.file_size);
        if rva >= section.rva && rva - section.rva < span {
            let delta = rva - section.rva;
            if delta >= section.file_size {
                return None;
            }
            let offset = section.file_offset.checked_add(delta)? as usize;
            return Some(offset);
        }
    }
    Some(rva as usize)
}

fn read_sdk_version(bytes: &[u8]) -> Result<u32, String> {
    let pe = victory_pe::ImageInfo::parse(bytes).map_err(|e| format!("invalid PE: {e}"))?;
    let rva = pe
        .exports
        .iter()
        .find(|e| e.name.as_deref() == Some("D3D12SDKVersion"))
        .and_then(|e| e.rva)
        .ok_or("missing D3D12SDKVersion export")?;
    if rva == 0 {
        return Err("D3D12SDKVersion RVA is 0".into());
    }
    let offset = rva_to_offset(&pe, rva).ok_or("D3D12SDKVersion RVA outside sections")?;
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or("D3D12SDKVersion RVA outside image")?;
    let version = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    (version != 0).then_some(version).ok_or("D3D12SDKVersion is 0".into())
}

fn probe_dir(dir: &Path) -> Result<u32, String> {
    let dll = dir.join("D3D12Core.dll");
    let bytes = std::fs::read(&dll).map_err(|e| format!("{}: {e}", dll.display()))?;
    read_sdk_version(&bytes).map_err(|e| format!("{}: {e}", dll.display()))
}

#[derive(Debug, PartialEq, Eq)]
enum DirResolution {
    Found { dir: PathBuf, version: u32 },
    NotFound,
    Unavailable(String),
}

fn resolve_dir(env_dir: Option<PathBuf>, exe_dir: Option<PathBuf>, local_app_data: Option<PathBuf>) -> DirResolution {
    let explicit = env_dir.is_some();
    let mut last_err = None;
    for dir in candidates(env_dir, exe_dir, local_app_data) {
        if !dir.join("D3D12Core.dll").is_file() {
            if explicit {
                return DirResolution::Unavailable(format!("{ENV}={} has no D3D12Core.dll", dir.display()));
            }
            continue;
        }
        match probe_dir(&dir) {
            Ok(version) => return DirResolution::Found { dir, version },
            Err(err) => {
                last_err = Some(err);
                if explicit {
                    break;
                }
            }
        }
    }
    match last_err {
        Some(err) => DirResolution::Unavailable(err),
        None => DirResolution::NotFound,
    }
}

/// `CreateDeviceFactory` needs a NUL-terminated ANSI path with a trailing separator.
fn format_sdk_path(dir: &Path) -> Result<CString, String> {
    let mut text = dir
        .to_str()
        .ok_or_else(|| format!("Agility SDK path is not valid UTF-8: {}", dir.display()))?
        .replace('/', "\\");
    if !text.ends_with('\\') {
        text.push('\\');
    }
    CString::new(text).map_err(|e| format!("Agility SDK path contains NUL: {e}"))
}

type D3D12GetInterfaceFn = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn resolve_get_interface() -> Result<D3D12GetInterfaceFn, String> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

    let name: Vec<u16> = std::ffi::OsStr::new("d3d12.dll")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: valid NUL-terminated module name and no loader flags beyond the system search.
    let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err("failed to load d3d12.dll".into());
    }
    // SAFETY: `GetProcAddress` returns a valid function or null.
    let symbol = unsafe { GetProcAddress(module, b"D3D12GetInterface\0".as_ptr()) }
        .ok_or("D3D12GetInterface unavailable (update Windows 10 to 22H2 KB5029331+)")?;
    // SAFETY: the Win32 export has this calling convention/signature.
    Ok(unsafe { std::mem::transmute::<_, D3D12GetInterfaceFn>(symbol) })
}

fn create_factory(version: u32, dir: &Path, path: &CStr) -> Result<ID3D12DeviceFactory, String> {
    // SAFETY: `D3D12GetInterface` writes an owned COM pointer into `raw`.
    unsafe {
        let get_interface = resolve_get_interface()?;
        let mut raw: *mut c_void = std::ptr::null_mut();
        get_interface(&CLSID_D3D12SDKConfiguration, &ID3D12SDKConfiguration1::IID, &mut raw)
            .ok()
            .map_err(|e| format!("ID3D12SDKConfiguration1 unavailable: {e}"))?;
        if raw.is_null() {
            return Err("D3D12GetInterface returned null configuration".into());
        }
        let config = ID3D12SDKConfiguration1::from_raw(raw);
        let factory: ID3D12DeviceFactory = config
            .CreateDeviceFactory(version, PCSTR(path.as_ptr().cast()))
            .map_err(|e| format!("CreateDeviceFactory(v{version}, {}): {e}", dir.display()))?;
        factory
            .SetFlags(D3D12_DEVICE_FACTORY_FLAG_ALLOW_RETURNING_EXISTING_DEVICE)
            .map_err(|e| format!("ID3D12DeviceFactory::SetFlags: {e}"))?;
        factory
            .ApplyToGlobalState()
            .map_err(|e| format!("ID3D12DeviceFactory::ApplyToGlobalState: {e}"))?;
        Ok(factory)
    }
}

fn init() -> State {
    let env_dir = clean_env_path(std::env::var_os(ENV));
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let local_app_data = clean_env_path(std::env::var_os("LOCALAPPDATA"));

    let (dir, version) = match resolve_dir(env_dir, exe_dir, local_app_data) {
        DirResolution::Found { dir, version } => (dir, version),
        DirResolution::NotFound => return State::NotFound,
        DirResolution::Unavailable(reason) => {
            tracing::warn!(error = %reason, "Agility SDK unavailable; using inbox D3D12");
            return State::Unavailable;
        }
    };
    let abs_dir = std::path::absolute(&dir).unwrap_or(dir);
    let path = match format_sdk_path(&abs_dir) {
        Ok(path) => path,
        Err(reason) => {
            tracing::warn!(error = %reason, "Agility SDK path invalid; using inbox D3D12");
            return State::Unavailable;
        }
    };
    match create_factory(version, &abs_dir, &path) {
        Ok(factory) => {
            tracing::info!(version, dir = %abs_dir.display(), "D3D12 Agility SDK loaded");
            State::Loaded { factory, version, dir: abs_dir }
        }
        Err(reason) => {
            tracing::warn!(error = %reason, "Agility SDK activation failed; using inbox D3D12");
            State::Unavailable
        }
    }
}

/// Create a device through the Agility factory when active, otherwise inbox.
pub fn create_device<P>(adapter: P, feature_level: D3D_FEATURE_LEVEL) -> windows::core::Result<ID3D12Device>
where
    P: Param<IUnknown>,
{
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: valid out-pointer; the factory is kept alive for the process lifetime.
    unsafe {
        match state() {
            State::Loaded { factory, .. } => factory.CreateDevice(adapter, feature_level, &mut device)?,
            _ => D3D12CreateDevice(adapter, feature_level, &mut device)?,
        }
    }
    device.ok_or_else(windows::core::Error::empty)
}

/// Get a D3D12 configuration interface from the active factory when possible.
pub fn configuration_interface<T: Interface>(clsid: &GUID) -> windows::core::Result<T> {
    // SAFETY: `clsid` is a valid GUID reference and `T` matches the requested interface.
    unsafe {
        match state() {
            State::Loaded { factory, .. } => factory.GetConfigurationInterface::<T>(clsid),
            _ => {
                let mut out: Option<T> = None;
                D3D12GetDebugInterface(&mut out)?;
                out.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_NOINTERFACE))
            }
        }
    }
}

/// Active Agility SDK version and directory, when one was loaded.
pub fn loaded() -> Option<(u32, &'static Path)> {
    match state() {
        State::Loaded { version, dir, .. } => Some((*version, dir.as_path())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_directory_is_used_alone() {
        let dirs = candidates(Some(PathBuf::from("explicit")), Some(PathBuf::from("exe")), None);
        assert_eq!(dirs, vec![PathBuf::from("explicit")]);
    }

    #[test]
    fn sdk_paths_get_a_trailing_separator() {
        let path = format_sdk_path(Path::new(r"C:\sdk\D3D12")).unwrap();
        assert_eq!(path.to_bytes(), br"C:\sdk\D3D12\");
    }

    /// Set `VICTORY_D3D12_DIR` to check the PE export parser against a real SDK.
    #[test]
    fn configured_sdk_exports_a_version() {
        let Some(dir) = clean_env_path(std::env::var_os(ENV)) else {
            return;
        };
        let version = probe_dir(&dir).unwrap();
        assert!(version >= 600, "unexpected SDK version {version}");
    }
}
