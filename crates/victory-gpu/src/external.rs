//! Host work that a host library records into a title's command list in
//! place of a console one (the host's FidelityFX upscaler, say): the host
//! device, a title texture's host copy in the state the library is told,
//! and the host list itself.

use windows::Win32::Graphics::Direct3D12::{
    ID3D12Device, ID3D12GraphicsCommandList, ID3D12Resource,
};

use crate::object::Object;

mod packed_ffx;

/// Writable float output for an FSR dispatch whose title target is packed R9.
pub fn ffx_float_output(list: *const Object, width: u32, height: u32) -> Option<ID3D12Resource> {
    packed_ffx::temporary(list, width, height)
}

/// Encode that float output into the title's R32_UINT packed-word writer.
pub fn ffx_pack_output(list: *const Object, float: &ID3D12Resource, packed: &ID3D12Resource) -> bool {
    packed_ffx::finish(list, float, packed)
}

/// The host D3D12 device, created on first use; `None` without a host GPU.
pub fn host_device() -> Option<ID3D12Device> {
    crate::host::host().map(|h| h.device.clone())
}

/// A title texture's host copy, moved to the host resource `state` in the
/// title list `list`, and the state it is in: `state` without the bits the
/// host copy cannot take (see `host::legal_state`). What the list records
/// next finds it there and must leave it there. The list holds the texture
/// until it is reset. `None` for a buffer, or without a host list.
pub fn host_texture(
    list: *const Object,
    resource: *const Object,
    state: u32,
) -> Option<(ID3D12Resource, u32)> {
    let host = crate::list::host_list(list)?;
    let texture = crate::resource::host_resource(resource)?.clone();
    crate::list::ensure_state(host, resource, state);
    let state = crate::host::legal_state(&texture, state);
    Some((texture, state))
}

/// Runs `f` with the host list behind the title list `list` (open for
/// recording); `None` without one. What `f` binds (descriptor heaps, root
/// signatures, pipeline) is replaced before the list's next draw or
/// dispatch.
pub fn record<R>(
    list: *const Object,
    f: impl FnOnce(&ID3D12GraphicsCommandList) -> R,
) -> Option<R> {
    let mut out = None;
    crate::list::with_state(list, |host, state| {
        out = Some(f(&host.list));
        state.host_state_lost();
    });
    out
}

/// Tag a host-library texture for a bounded post-submission diagnostic dump.
#[cfg(feature = "diagnostics")]
pub fn take_ffx_capture() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TAKEN: AtomicBool = AtomicBool::new(false);
    static OFFSET: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    if std::env::var_os("VICTORY_CAPTURE_FFX").is_none() {
        return false;
    }
    if std::env::var_os("VICTORY_CAPTURE_FFX_AFTER_LIT").is_some()
        && crate::capture::lit_frame().is_none()
    {
        return false;
    }
    let offset = *OFFSET.get_or_init(|| std::env::var("VICTORY_CAPTURE_FFX_OFFSET")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    let base = if std::env::var_os("VICTORY_CAPTURE_577_INPUTS").is_some() {
        crate::capture::lit_frame()
    } else {
        crate::capture::lit_frame().or_else(crate::capture::world_frame)
    };
    // FidelityFX can likewise skip a frame while the title changes scenes.
    if offset > 0 && base.is_none_or(|world| crate::queue::presented() + 1 < world + offset) {
        return false;
    }
    !TAKEN.swap(true, Ordering::AcqRel)
}

/// Tag a host-library texture for a bounded post-submission diagnostic dump.
#[cfg(feature = "diagnostics")]
pub fn snapshot_ffx_input_pre(list: *const Object, resource: *const Object) {
    if resource.is_null() {
        return;
    }
    if let Some(host) = crate::list::host_list(list) {
        crate::capture::snapshot_ffx_input_pre(host, resource);
    }
}

#[cfg(feature = "diagnostics")]
pub fn note_diagnostic_target(list: *const Object, resource: *const Object, label: &str) {
    if resource.is_null() {
        return;
    }
    if label == "ffx-output" {
        crate::capture::mark_ffx_output(resource);
    } else if label == "ffx-color-input" {
        crate::capture::retain_ffx_input(resource);
    }
    if let Some(host) = crate::list::host_list(list) {
        host.note_target(resource as usize, label);
    }
}
