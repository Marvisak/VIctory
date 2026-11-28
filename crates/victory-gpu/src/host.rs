//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap;
//! overlap them with per-submission fences once more than clears run.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_FEATURE_LEVEL_12_0, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCSTR, PCWSTR, s, w};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    pub(crate) depth_bounds_supported: bool,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    /// A ring of CPU-only views, for the CPU handles of unordered access
    /// clears, used as `rtvs`.
    cpu_views: ID3D12DescriptorHeap,
    cpu_view_size: u32,
    next_cpu_view: AtomicU32,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Presentation>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, bytes that differed), since the last
/// frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Diagnostics-only wall time outside the existing submission buckets:
/// submission lock wait, full execute after that lock, the gap before
/// copy-back, and work after copy-back. Reported once per five seconds.
#[cfg(feature = "diagnostics")]
static EXECUTE_PHASE_US: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// CPU time after the GPU fence: missed-work/range setup, marked-page
/// writeback, deferred texture views, explicit ranges, capture hooks, and
/// deferred callbacks.
#[cfg(feature = "diagnostics")]
static COPYBACK_PHASE_US: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

#[cfg(feature = "diagnostics")]
type AfterTimes = std::collections::HashMap<&'static str, (u64, u64, u64)>;

#[cfg(feature = "diagnostics")]
static AFTER_TIMES: OnceLock<Mutex<AfterTimes>> = OnceLock::new();

#[cfg(feature = "diagnostics")]
fn after_times() -> &'static Mutex<AfterTimes> {
    AFTER_TIMES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

#[cfg(feature = "diagnostics")]
fn log_after_times(frames: f64) {
    let mut by: Vec<_> = std::mem::take(&mut *after_times().lock().unwrap_or_else(|e| e.into_inner()))
        .into_iter().collect();
    by.sort_by_key(|entry| std::cmp::Reverse(entry.1.0));
    for (kind, (us, n, max)) in by.iter().take(10) {
        tracing::info!(
            "  copy-back callback {:.1} ms/frame, {:.1} calls/frame, {:.1} ms max: {kind}",
            *us as f64 / 1000.0 / frames,
            *n as f64 / frames,
            *max as f64 / 1000.0,
        );
    }
}

#[cfg(feature = "diagnostics")]
fn copyback_phase(index: usize, started: &mut Instant) {
    let now = Instant::now();
    COPYBACK_PHASE_US[index].fetch_add(now.duration_since(*started).as_micros() as u64, Ordering::Relaxed);
    *started = now;
}

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// Ends the run on a host GPU failure: what failed, the removal reason,
/// DRED data and the debug layer's first error, if any
/// (`VICTORY_D3D12_DEBUG=1` in a diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    fail_after(what, &[])
}

fn fail_after(what: std::fmt::Arguments, runs: &[&HostList]) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        let removed = unsafe { host.device.GetDeviceRemovedReason() };
        text = failure_text(text, removed, || host.removal_data(runs));
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => {
                text.push_str("; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command")
            }
            None => {}
        }
    }
    tracing::error!("{text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

fn failure_text(
    mut text: String,
    removed: windows::core::Result<()>,
    removal_data: impl FnOnce() -> String,
) -> String {
    if let Err(e) = removed {
        text.push_str(&format!("; device removed: {e}"));
        text.push_str(&removal_data());
    }
    text
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
pub(crate) fn host() -> Option<&'static Host> {
    static HOST: OnceLock<Option<Host>> = OnceLock::new();
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

fn create() -> windows::core::Result<Host> {
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // The debug layer only in builds with diagnostics.
        let debug = cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_DEBUG");
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            // GPU-based validation (`VICTORY_D3D12_GBV`): out-of-range
            // descriptor heap indices and the like become messages naming
            // the command instead of page faults. Without state tracking
            // (directly indexed textures are not transitioned).
            if crate::switch::on("VICTORY_D3D12_GBV")
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                }
                if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_on() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
        }
        let mut device: Option<ID3D12Device> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        if cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_WARP") {
            let adapter = warp_adapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            // Whether a GPU hang ends the run or freezes the PC (#1054).
            crate::tdr::log();
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
        let depth_bounds_supported = device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS2,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .is_ok()
            && options.DepthBoundsTestSupported.as_bool();
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        let cpu_views: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let cpu_view_size =
            device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        tracing::info!("host D3D12 device ready");
        // The console fuses its multiply-adds (`v_fma_f32`, `v_fmac_f32`,
        // `v_fma_mix*`); the lowering's DXIL `FMad` is fused only where the
        // host chooses (the RX 6700 XT does, WARP does not).
        match victory_shader::emit::host::Host::from_device(device.clone()).fuses_fmad() {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                "the host GPU does not fuse multiply-adds: results of the console's fused v_fma and v_fmac can differ in kind here"
            ),
            Err(e) => tracing::debug!(error = %e, "multiply-add fusing not checked"),
        }
        Ok(Host {
            device,
            depth_bounds_supported,
            queue,
            fence,
            submitted: Mutex::new(0),
            event,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            cpu_views,
            cpu_view_size,
            next_cpu_view: AtomicU32::new(0),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(Presentation::default()),
        })
    }
}

/// Diagnostics: our process's private commit and working set, bytes.
#[cfg(feature = "diagnostics")]
fn process_memory() -> (u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (c.PagefileUsage as u64, c.WorkingSetSize as u64)
}

/// DXGI may return hardware for `EnumWarpAdapter` (notably under Proton).
/// Reject it before creating a device in diagnostics' software-only mode.
fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

/// Diagnostics, `VICTORY_GPU_TIMING`: GPU time per kind of command and
/// pipeline (ms, commands) and lists' wall time against the sum of their
/// commands' (ms), since the last frame-rate line.
#[cfg(feature = "diagnostics")]
type GpuTimes = (std::collections::HashMap<String, (f64, u64)>, f64, f64);
#[cfg(feature = "diagnostics")]
static GPU_TIMES: Mutex<Option<GpuTimes>> = Mutex::new(None);

/// Sparse measured command intervals, by command kind and shader stages.
/// These are samples, not an estimate of the full frame's GPU time.
#[cfg(feature = "diagnostics")]
type GpuProfile = std::collections::HashMap<String, (f64, u64, f64)>;

#[cfg(feature = "diagnostics")]
static GPU_PROFILE: Mutex<Option<GpuProfile>> = Mutex::new(None);

#[cfg(feature = "diagnostics")]
fn gpu_profile(samples: &[(f64, String)]) {
    let mut profile = GPU_PROFILE.lock().unwrap_or_else(|e| e.into_inner());
    let by = profile.get_or_insert_with(Default::default);
    for (ms, note) in samples {
        let head = note.split(" | ").next().unwrap_or_default();
        let kind = head.split(' ').next().unwrap_or_default();
        let key = match head.find('[') {
            Some(at) => format!("{kind} {}", &head[at..]),
            None => kind.to_owned(),
        };
        let entry = by.entry(key).or_default();
        (entry.0, entry.1, entry.2) = (entry.0 + ms, entry.1 + 1, entry.2.max(*ms));
    }
}

#[cfg(feature = "diagnostics")]
fn log_gpu_profile(frames: f64) {
    let Some(by) = GPU_PROFILE.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let mut kinds = GpuProfile::new();
    for (key, &(ms, n, max)) in &by {
        let kind = key.split(' ').next().unwrap_or_default().to_owned();
        let entry = kinds.entry(kind).or_default();
        (entry.0, entry.1, entry.2) = (entry.0 + ms, entry.1 + n, entry.2.max(max));
    }
    let mut kinds: Vec<_> = kinds.into_iter().collect();
    kinds.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    let mut by: Vec<_> = by.into_iter().collect();
    by.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    let stride = profile_stride().unwrap_or(0);
    tracing::info!(stride, groups = by.len(), "sampled GPU command profile (measured samples only, not full-frame GPU time)");
    for (kind, (ms, n, max)) in &kinds {
        tracing::info!(
            "  GPU sampled kind {kind}: {:.2} ms/frame, {:.2} ms/sample average, {:.2} ms max in {:.1} samples/frame",
            ms / frames,
            ms / *n as f64,
            max,
            *n as f64 / frames,
        );
    }
    for (key, (ms, n, max)) in by.iter().take(25) {
        tracing::info!(
            "  GPU sampled {:.2} ms/frame, {:.2} ms/sample average, {:.2} ms max in {:.1} samples/frame: {key}",
            ms / frames,
            ms / *n as f64,
            max,
            *n as f64 / frames,
        );
    }
}

/// Adds a timed list's commands (from [`HostList::slowest`]) to [`GPU_TIMES`],
/// by the command's kind and pipeline: its note before the first ` | `,
/// without counts and addresses.
#[cfg(feature = "diagnostics")]
fn gpu_times(wall: Duration, commands: &[(f64, String)]) {
    let mut times = GPU_TIMES.lock().unwrap_or_else(|e| e.into_inner());
    let (by, wall_ms, sum) = times.get_or_insert_with(Default::default);
    *wall_ms += wall.as_secs_f64() * 1e3;
    for (ms, note) in commands {
        // "draw 3x1 [mesh 1f.., pixel 2e..] | ...": the kind and the pipeline.
        let head = note.split(" | ").next().unwrap_or_default();
        let kind = head.split(' ').next().unwrap_or_default();
        let key = match head.find('[') {
            Some(at) => format!("{kind} {}", &head[at..]),
            None => kind.to_owned(),
        };
        let e = by.entry(key).or_default();
        (e.0, e.1) = (e.0 + ms, e.1 + 1);
        *sum += ms;
    }
}

/// Logs [`GPU_TIMES`] per frame over `frames` frames: the 25 costliest.
#[cfg(feature = "diagnostics")]
fn log_gpu_times(frames: f64) {
    let Some((by, wall, sum)) = GPU_TIMES.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    tracing::info!(
        "GPU timing: lists {:.1} ms/frame of wall time, their commands {:.1} ms/frame",
        wall / frames,
        sum / frames
    );
    let mut by: Vec<_> = by.into_iter().collect();
    by.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
    for (key, (ms, n)) in by.iter().take(25) {
        tracing::info!(
            "  GPU {:.2} ms/frame in {:.1} commands/frame: {key}",
            ms / frames,
            *n as f64 / frames
        );
    }
}

/// Diagnostics: the host device's video memory use and budget, in MB, local
/// (on the card) and non-local (system memory the GPU reads), as the
/// system's memory manager sees them: use over budget makes it page
/// allocations at submissions.
pub(crate) fn video_memory() -> Option<[(f64, f64); 2]> {
    static ADAPTER: OnceLock<Option<IDXGIAdapter3>> = OnceLock::new();
    let adapter = ADAPTER
        .get_or_init(|| {
            let device = &host()?.device;
            // SAFETY: plain device and DXGI queries.
            unsafe {
                let luid = device.GetAdapterLuid();
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
                factory.EnumAdapterByLuid(luid).ok()
            }
        })
        .as_ref()?;
    let info = |group| {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: a valid out pointer.
        unsafe { adapter.QueryVideoMemoryInfo(0, group, &mut info) }.ok()?;
        Some((info.CurrentUsage as f64 / 1e6, info.Budget as f64 / 1e6))
    };
    Some([
        info(DXGI_MEMORY_SEGMENT_GROUP_LOCAL)?,
        info(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL)?,
    ])
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn failure_text_keeps_removal_data_once() {
        let reason = windows::core::Error::from_hresult(super::DXGI_ERROR_DEVICE_REMOVED);
        let data = "; page fault at 0x1000; inside [allocation]; breadcrumbs [0/1]";
        let expected = format!("host list did not close; device removed: {reason}{data}");
        let text = super::failure_text("host list did not close".into(), Err(reason), || {
            data.into()
        });
        assert_eq!(text, expected);
        assert_eq!(text.matches(data).count(), 1);
    }

    #[test]
    fn failure_text_skips_removal_data_on_a_live_device() {
        let text = super::failure_text("host list did not reset".into(), Ok(()), || {
            panic!("a live device has no removal data")
        });
        assert_eq!(text, "host list did not reset");
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn sparse_gpu_profile_pairs_only_selected_commands_and_closes_last() {
        let mut notes = super::ProfileNotes::default();
        let mut stamped = Vec::new();
        for i in 0..7 {
            notes.before(3, || format!("draw {i}"), |at| stamped.push(at));
        }
        notes.finish(|at| stamped.push(at));
        assert_eq!(notes.seen, 7);
        assert_eq!(stamped, [0, 1, 2, 3, 4, 5]);
        assert_eq!(notes.samples, [(0, 1, "draw 0".into()), (2, 3, "draw 3".into()), (4, 5, "draw 6".into())]);
        assert_eq!(notes.next, 6);
    }

    thread_local! {
        /// Every transition barrier built on this thread (resource, before,
        /// after), in order.
        pub(crate) static ISSUED: std::cell::RefCell<Vec<(usize, u32, u32)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// The barriers built on this thread since the last call.
    pub(crate) fn issued() -> Vec<(usize, u32, u32)> {
        ISSUED.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }

    /// Replays transitions in the order the GPU runs them from the states
    /// in `states` (resources not in it are not checked), by the debug
    /// layer's rule: each barrier's before-state is the resource's current
    /// one. The first that is not, else the states after.
    pub(crate) fn replay(
        issued: &[(usize, u32, u32)],
        states: &mut std::collections::HashMap<usize, u32>,
    ) -> Result<(), String> {
        for (k, &(resource, before, after)) in issued.iter().enumerate() {
            if let Some(state) = states.get_mut(&resource) {
                if *state != before {
                    return Err(format!(
                        "barrier {k} on {resource:#x}: before {before:#x}, but the resource is in {state:#x}"
                    ));
                }
                *state = after;
            }
        }
        Ok(())
    }

    #[test]
    fn swap_chain_failures_retry_once_per_second_and_success_clears_the_wait() {
        use super::{Duration, Instant, SwapChainRetry};

        let start = Instant::now();
        let mut retry = SwapChainRetry::default();
        let mut attempts = 0;
        // Two seconds at 60 Hz, with a failure on every eligible attempt.
        for frame in 0..120 {
            retry.attempt(start + Duration::from_millis(frame * 1000 / 60), || {
                attempts += 1;
                Err::<(), _>("desktop unavailable")
            });
        }
        assert_eq!(attempts, 2);
        let recovered = start + Duration::from_secs(2);
        assert_eq!(retry.attempt(recovered, || Ok::<_, ()>(42)), Some(Ok(42)));
        assert_eq!(retry.attempt(recovered, || Ok::<_, ()>(43)), Some(Ok(43)));
    }

    use super::*;

    /// Shaders that use title memory mark their reads in their own bitmap:
    /// read marks of both bitmaps are taken, each slot once until it is
    /// forgotten, and a forgotten slot's next read is taken again.
    #[test]
    fn read_marks_of_both_bitmaps_are_taken() {
        let Some(host) = host() else { return };
        // No submission takes the marks meanwhile.
        let _one = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let (plain, memory) = (
            victory_shader::emit::HEAP_VIEWS - 2,
            victory_shader::emit::HEAP_VIEWS - 1,
        );
        crate::shadow::tests::mark_read(false, plain);
        crate::shadow::tests::mark_read(true, memory);
        let taken = crate::shadow::take_reads();
        assert!(taken.contains(&plain), "a plain shader's read");
        assert!(
            taken.contains(&memory),
            "a read by a shader that uses title memory"
        );
        let again = crate::shadow::take_reads();
        assert!(
            !again.contains(&plain) && !again.contains(&memory),
            "each once"
        );
        crate::shadow::forget_read(memory);
        assert!(
            !crate::shadow::take_reads().contains(&memory),
            "forgotten: its mark cleared"
        );
        crate::shadow::tests::mark_read(true, memory);
        assert!(
            crate::shadow::take_reads().contains(&memory),
            "read again after it was forgotten"
        );
    }

    /// The textures shaders read through the heap since the last eviction
    /// pass are named again at each pass (a slot's read is reported as new
    /// only once), and the marks are cleared for the next.
    #[test]
    fn heap_reads_since_the_last_pass_name_their_textures() {
        let Some(host) = host() else { return };
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 4, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        assert_eq!(
            crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap),
            0
        );
        let title_heap = crate::descriptor::heap(heap);
        // A 4x1 RGBA8 texture, linear, in memory of its own, and a 2D view.
        let memory = vec![0u8; 1 << 16];
        let address = memory.as_ptr() as u64;
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let mut view = [0u32; 16];
        view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        (view[10], view[11]) = (address as u32, (address >> 32) as u32);
        crate::descriptor::create_placed_srv(null, &words, &view, title_heap.start);
        let heap_host = title_heap.host().expect("a host view heap");
        assert!(heap_host.mirror(0, title_heap.start, true).is_some());
        let texture = crate::resource::texture_at(address, &words).expect("its resource");
        let raw =
            windows::core::Interface::as_raw(&crate::resource::made_copy(texture).unwrap().texture)
                as usize;
        // No submission takes the marks meanwhile.
        let _one = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..2 {
            crate::shadow::tests::mark_read(false, 0);
            let slots = crate::shadow::reads_since();
            assert!(slots.contains(&0), "the slot read");
            assert!(
                crate::descriptor::read_textures(&slots).contains(&raw),
                "its texture named"
            );
            assert!(
                !crate::shadow::reads_since().contains(&0),
                "the marks cleared"
            );
        }
    }

    /// The idle rule against the heap reads that thrashed (run evict1): a
    /// texture shaders read through the heap in every frame stays over more
    /// frames and time than the idle limits; one read once is given up. A
    /// list of the title's naming a texture (a table read, a copy) stamps it
    /// as used; a list of ours (uploads) does not.
    #[test]
    fn textures_read_every_frame_stay_and_title_lists_stamp_theirs() {
        let Some(host) = host() else { return };
        let null = std::ptr::null();
        let heap_desc: [u32; 8] = [0, 8, 1, 0, 0, 0, 0, 0];
        let mut heap = null;
        let iid = crate::object::guid(
            0x8efb471d,
            0x616c,
            0x4f49,
            [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
        );
        assert_eq!(
            crate::descriptor::create_heap(null, heap_desc.as_ptr().cast(), &iid, &mut heap),
            0
        );
        let title_heap = crate::descriptor::heap(heap);
        let step = 32;
        // Two 4x1 RGBA8 textures, linear, in memory of their own, viewed at
        // slots 5 and 6.
        let memory = vec![0u8; 1 << 17];
        let words: [u32; 14] = [3, 0, 0, 0, 4, 0, 1, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let heap_host = title_heap.host().expect("a host view heap");
        let textures: Vec<crate::object::This> = [5u32, 6]
            .iter()
            .map(|&slot| {
                let address = memory.as_ptr() as u64 + u64::from(slot) * (1 << 13);
                let mut view = [0u32; 16];
                view[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
                (view[10], view[11]) = (address as u32, (address >> 32) as u32);
                let at = title_heap.start + u64::from(slot) * step;
                crate::descriptor::create_placed_srv(null, &words, &view, at);
                assert!(heap_host.mirror(slot, at, true).is_some());
                crate::resource::texture_at(address, &words).expect("its resource")
            })
            .collect();
        let owned: Vec<_> = textures
            .iter()
            .map(|&t| crate::resource::owned(t).expect("may be given up"))
            .collect();
        let (idle_frames, idle_ms) = (crate::resource::IDLE_FRAMES, crate::resource::IDLE_MS);
        {
            // No submission takes the marks meanwhile.
            let _one = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            // Passes a third of the idle limits apart, over three times them.
            for k in 0..10 {
                crate::shadow::tests::mark_read(false, 5);
                if k == 0 {
                    crate::shadow::tests::mark_read(false, 6);
                }
                let read = crate::descriptor::read_textures(&crate::shadow::reads_since());
                crate::resource::evict_among(
                    &owned,
                    &read,
                    1000 + k * idle_frames / 3,
                    1_000_000 + k * idle_ms / 3,
                    0,
                    u64::MAX,
                );
                assert!(
                    crate::resource::made_copy(textures[0]).is_some(),
                    "read every frame: stays (frame {k})"
                );
            }
        }
        assert!(
            crate::resource::made_copy(textures[1]).is_none(),
            "read once: given up"
        );
        drop(owned);
        // A title list naming the texture stamps it, an upload list does not.
        let copy = crate::resource::host_copy(textures[0]).unwrap();
        for (ours, stamped) in [(true, false), (false, true)] {
            copy.unstamp();
            let list = host.new_list().unwrap();
            if ours {
                list.set_ours();
            }
            let _ = list.transition(textures[0], 0x40);
            assert_eq!(copy.last_used().0 != u64::MAX, stamped, "ours: {ours}");
        }
    }

    /// Textures the heaps view are brought up to date only if the title used
    /// them lately: one whose pages changed but that no shader read is not
    /// uploaded; read in the last pass, it is.
    #[test]
    fn heap_textures_unread_lately_are_not_brought_up() {
        let Some(host) = host() else { return };
        let address = crate::watch::tests::pool(1) as u64;
        let words: [u32; 14] = [3, 0, 0, 0, 64, 0, 64, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
        let iid = crate::object::guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let mut object = std::ptr::null();
        let null = std::ptr::null();
        assert_eq!(
            crate::resource::create_at_address(
                null,
                address,
                words.as_ptr().cast(),
                0,
                std::ptr::null(),
                &iid,
                &mut object
            ),
            0
        );
        let copy = crate::resource::host_copy(object).expect("a host texture");
        let mut desc = [0u32; 10];
        desc[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
        let viewed = std::sync::Arc::new(crate::bindless::Viewed::default());
        viewed.set(
            0,
            Some(&crate::descriptor::View::Shader(
                object as usize,
                Some(desc),
            )),
            true,
        );
        // No submission uploads meanwhile.
        let _one = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the texture's memory, 256-byte rows of 64 texels.
        unsafe { std::ptr::write_bytes(address as *mut u8, 0x40, 64 * 64 * 4) };
        copy.unstamp();
        let _ = crate::bindless::uploads(std::slice::from_ref(&viewed), &[]);
        assert_eq!(
            crate::texture::uploads(object),
            0,
            "not read lately: not brought up"
        );
        copy.stamp();
        let _ = crate::bindless::uploads(std::slice::from_ref(&viewed), &[]);
        assert_eq!(
            crate::texture::uploads(object),
            1,
            "read in the last pass: brought up"
        );
    }

    /// Heap textures brought up in one submission are refreshed side by
    /// side, each into its own staging and its own upload.
    #[test]
    fn heap_textures_brought_up_together_each_get_their_upload() {
        let Some(host) = host() else { return };
        let iid = crate::object::guid(
            0x696442be,
            0xa72e,
            0x4059,
            [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
        );
        let pool = crate::watch::tests::pool(6);
        let viewed = std::sync::Arc::new(crate::bindless::Viewed::default());
        let objects: Vec<crate::object::This> = (0..6u32)
            .map(|k| {
                // RGBA8 128x128 (one page each), linear, each its own bytes.
                let address = (pool + k as usize * crate::watch::PAGE) as u64;
                // SAFETY: the texture's own page of the pool.
                unsafe {
                    std::ptr::write_bytes(address as *mut u8, 0x10 * k as u8, crate::watch::PAGE)
                };
                let words: [u32; 14] =
                    [3, 0, 0, 0, 128, 0, 128, 1 | 1 << 16, 28, 1, 0, 0x100, 0, 0];
                let mut object = std::ptr::null();
                assert_eq!(
                    crate::resource::create_at_address(
                        std::ptr::null(),
                        address,
                        words.as_ptr().cast(),
                        0,
                        std::ptr::null(),
                        &iid,
                        &mut object
                    ),
                    0
                );
                crate::resource::host_copy(object)
                    .expect("a host texture")
                    .stamp();
                let mut desc = [0u32; 10];
                desc[..6].copy_from_slice(&[28, 4, 0x1688, 0, 0, 1]);
                viewed.set(
                    k,
                    Some(&crate::descriptor::View::Shader(
                        object as usize,
                        Some(desc),
                    )),
                    true,
                );
                object
            })
            .collect();
        // No submission uploads meanwhile.
        let _one = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let _ = crate::bindless::uploads(std::slice::from_ref(&viewed), &[]);
        for &object in &objects {
            assert_eq!(crate::texture::uploads(object), 1, "each brought up once");
        }
        let _ = crate::bindless::uploads(std::slice::from_ref(&viewed), &[]);
        for &object in &objects {
            assert_eq!(crate::texture::uploads(object), 1, "unchanged: not again");
        }
    }

    #[test]
    fn list_scratch_grows_to_fit_and_reuses_fenced_chunks() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let first = list.scratch(1).unwrap();
        let second = list.scratch(1).unwrap();
        assert_eq!(second.0 - first.0, 256);
        assert_eq!(second.1 as usize - first.1 as usize, 256);
        assert_eq!(
            list.scratch.lock().unwrap().chunks[0].capacity,
            FIRST_SCRATCH_CHUNK
        );
        let large = list.scratch(96 << 10).unwrap();
        assert_eq!(list.scratch.lock().unwrap().chunks[1].capacity, 128 << 10);
        assert!(list.scratch(u64::MAX).is_none());
        assert!(list.scratch(SCRATCH_CHUNK + 1).is_some());
        list.close();
        host.execute(&[&list]);
        list.reset();
        assert_eq!(list.scratch(96 << 10).unwrap(), large);
        assert_eq!(list.scratch.lock().unwrap().chunks.len(), 2);
        list.close();
        host.execute(&[&list]);
        list.reset();
        assert_eq!(list.scratch(512).unwrap(), first);
        list.close();
    }

    /// Reset of a list that is still open. Grand Theft Auto VI resets lists
    /// without a Close on the host list that is still recording. Reset onto
    /// that allocator returns E_INVALIDARG (debug layer id 540) and removes
    /// the device unless the open list is closed first.
    #[test]
    fn reset_of_an_open_list_reuses_the_allocator() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        list.reset();
        list.reset();
        list.close();
        // SAFETY: a plain query.
        let reason = unsafe { host.device.GetDeviceRemovedReason() };
        assert!(reason.is_ok(), "{reason:?}");
    }

    #[test]
    fn present_compositor_builds_both_pipeline_states() {
        let Some(host) = host() else {
            eprintln!("skipping compositor initialization: no D3D12 device");
            return;
        };
        Compositor::new(&host.device, DXGI_FORMAT_R8G8B8A8_UNORM)
            .expect("create compositor root signature and both pipeline states");
        eprintln!("PresentX compositor initialized");
    }

    #[test]
    fn warp_adapter_accepts_only_software() {
        // This probes DXGI without creating a D3D12 device or running a title.
        // SAFETY: plain DXGI adapter queries.
        let raw = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            else {
                return;
            };
            factory.EnumWarpAdapter::<IDXGIAdapter>()
        };
        let Ok(raw) = raw else { return };
        // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
        // IDXGIAdapter1; `raw` owns that interface.
        let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
        eprintln!(
            "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
            desc.VendorId, desc.Flags
        );
        assert_eq!(
            warp_adapter().is_ok(),
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        );
    }

    /// A node of the device-removed data is a list by pointer or by name.
    /// The name is what still matches when the runtime records another
    /// object than the one we hold (the debug layer wraps ours), and a
    /// different list or an unnamed node never matches.
    #[test]
    fn a_breadcrumb_node_is_found_by_pointer_or_by_name() {
        let ours = [0x1000, 0x1008];
        let name = list_name(7);
        let is = |pointer, text| node_is_list(pointer, text, &ours, &name);
        assert_eq!(name, "victory host list 7");
        assert!(is(Some(0x1000), None));
        assert!(is(Some(0x1008), Some("other")));
        assert!(is(Some(0x9999), Some("victory host list 7")));
        assert!(is(None, Some("victory host list 7")));
        assert!(!is(Some(0x9999), Some("victory host list 70")));
        assert!(!is(Some(0x9999), None));
        assert!(!is(None, None));
        // The debug layer appends its own note to the name we gave.
        assert!(is(None, Some("victory host list 7 (debug layer indirect)")));
        assert!(is(None, Some("victory host list 7 (x)")));
        assert!(!is(
            None,
            Some("victory host list 70 (debug layer indirect)")
        ));
        assert!(!is(None, Some("victory host list 7 debug layer")));
        assert!(!is(None, Some("victory host list 7(")));
        assert!(is(None, Some("victory host list 7")));
        assert_eq!(list_name(70), "victory host list 70");
    }

    /// The pointers of one list: the interface pointers it answers with
    /// are all there, and another list's are not.
    #[test]
    fn a_list_knows_every_pointer_it_answers_with() {
        // SAFETY: plain device and list creation on the software adapter.
        unsafe {
            let Ok(adapter) = warp_adapter() else { return };
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                return;
            }
            let device = device.unwrap();
            // An open list holds its allocator: one each.
            let make = || -> ID3D12GraphicsCommandList {
                let allocator: ID3D12CommandAllocator = device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .unwrap();
                device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .unwrap()
            };
            let (a, b) = (make(), make());
            let pointers = identities(&a);
            assert!(pointers.contains(&(a.as_raw() as usize)));
            let unknown = a.cast::<windows::core::IUnknown>().unwrap();
            assert!(pointers.contains(&(unknown.as_raw() as usize)));
            let command_list = a.cast::<ID3D12CommandList>().unwrap();
            assert!(pointers.contains(&(command_list.as_raw() as usize)));
            let other = identities(&b);
            assert!(pointers.iter().all(|p| !other.contains(p)));
            // As a node: the same object by any of its pointers; a node
            // naming the other list by pointer and our name still matches.
            let name = list_name(3);
            let is = |pointer, text| node_is_list(pointer, text, &pointers, &name);
            assert!(is(Some(command_list.as_raw() as usize), None));
            assert!(!is(Some(b.as_raw() as usize), None));
            assert!(is(Some(b.as_raw() as usize), Some(&name)));
        }
    }

    /// A node as the runtime gives it, built by hand: `HostList::is_node`
    /// reads the pointer and the copied name, and the name alone finds a
    /// list the runtime records under another object.
    #[test]
    fn is_node_reads_the_name_the_runtime_copied() {
        let Some(host) = host() else { return };
        let Ok(list) = host.new_list() else { return };
        let mut wide: Vec<u16> = list_name(list.id).encode_utf16().collect();
        wide.push(0);
        let mut node = D3D12_AUTO_BREADCRUMB_NODE::default();
        assert!(!list.is_node(&node), "an empty node is no list");
        node.pCommandListDebugNameW = windows::core::PCWSTR(wide.as_ptr());
        assert!(list.is_node(&node), "found by its name alone");
        let mut other: Vec<u16> = list_name(list.id + 1).encode_utf16().collect();
        other.push(0);
        node.pCommandListDebugNameW = windows::core::PCWSTR(other.as_ptr());
        assert!(!list.is_node(&node), "another list's name is no match");
        // With the debug layer: our name and the layer's note, in either
        // name field; and a list whose number only starts with ours is not it.
        let note = std::ffi::CString::new(format!("{} (debug layer indirect)", list_name(list.id)))
            .unwrap();
        node.pCommandListDebugNameW = windows::core::PCWSTR::null();
        node.pCommandListDebugNameA = note.as_ptr().cast();
        assert!(
            list.is_node(&node),
            "found by its ANSI name with the layer's note"
        );
        let longer =
            std::ffi::CString::new(format!("{}0 (debug layer indirect)", list_name(list.id)))
                .unwrap();
        node.pCommandListDebugNameA = longer.as_ptr().cast();
        assert!(
            !list.is_node(&node),
            "a number that only starts with ours is no match"
        );
    }
}

/// GPU buffers one list uses for mesh fallback draws.
struct MeshGpu {
    index: ID3D12Resource,
    vertex: ID3D12Resource,
    draws: ID3D12Resource,
    index_bytes: u64,
    vertex_bytes: u64,
    index_state: D3D12_RESOURCE_STATES,
    vertex_state: D3D12_RESOURCE_STATES,
    draws_state: D3D12_RESOURCE_STATES,
}

/// Bytes of the draw-argument buffer ([`MeshGpu::draws`]).
const MESH_DRAWS: u64 = 8 * 20;
/// Mesh buffer sets kept after a list resets. Every list that had drawn
/// kept its own set, and a world load held about 450 of them on the card.
const MESH_KEPT: usize = 64;

/// Sets still alive, their bytes, for the statistics line.
static MESH_SETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static MESH_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Returned at reset and checked out by the next list that draws.
static MESH_POOL: Mutex<Vec<MeshGpu>> = Mutex::new(Vec::new());

impl Drop for MeshGpu {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        MESH_SETS.fetch_sub(1, Relaxed);
        MESH_BYTES.fetch_sub(
            self.index_bytes
                .saturating_add(self.vertex_bytes)
                .saturating_add(MESH_DRAWS),
            Relaxed,
        );
    }
}

fn mesh_born(index_bytes: u64, vertex_bytes: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    MESH_SETS.fetch_add(1, Relaxed);
    MESH_BYTES.fetch_add(
        index_bytes
            .saturating_add(vertex_bytes)
            .saturating_add(MESH_DRAWS),
        Relaxed,
    );
}

/// A set at least `index_bytes` and `vertex_bytes`, the smallest such.
fn mesh_checkout(index_bytes: u64, vertex_bytes: u64) -> Option<MeshGpu> {
    let mut pool = MESH_POOL.lock().unwrap_or_else(|e| e.into_inner());
    let i = pool
        .iter()
        .enumerate()
        .filter(|(_, g)| g.index_bytes >= index_bytes && g.vertex_bytes >= vertex_bytes)
        .min_by_key(|(_, g)| g.index_bytes.saturating_add(g.vertex_bytes))
        .map(|(i, _)| i)?;
    Some(pool.swap_remove(i))
}

/// Back into the pool, or dropped once [`MESH_KEPT`] sets are waiting.
fn mesh_checkin(gpu: MeshGpu) {
    let mut pool = MESH_POOL.lock().unwrap_or_else(|e| e.into_inner());
    if pool.len() < MESH_KEPT {
        pool.push(gpu);
    }
}

/// Live mesh sets, their bytes, and how many of those sit in the pool.
#[cfg(feature = "diagnostics")]
pub(crate) fn mesh_stats() -> (u64, u64, usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let pooled = MESH_POOL.lock().unwrap_or_else(|e| e.into_inner()).len();
    (MESH_SETS.load(Relaxed), MESH_BYTES.load(Relaxed), pooled)
}

/// A copy of those buffers and the state the list last left them in.
pub(crate) struct MeshSlot {
    pub(crate) index: ID3D12Resource,
    pub(crate) vertex: ID3D12Resource,
    pub(crate) draws: ID3D12Resource,
    pub(crate) index_bytes: u64,
    pub(crate) index_state: D3D12_RESOURCE_STATES,
    pub(crate) vertex_state: D3D12_RESOURCE_STATES,
    pub(crate) draws_state: D3D12_RESOURCE_STATES,
}

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    pub(crate) depth_bounds: Option<ID3D12GraphicsCommandList1>,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    states: Mutex<States>,
    /// Depth textures with a writable twin the list uses (`crate::twin`),
    /// by host copy: the side its first use needs and the side it leaves
    /// newer.
    twins: Mutex<std::collections::HashMap<usize, crate::twin::Use>>,
    /// A copy of ours replaced the list's compute root signature and
    /// pipeline: draws and dispatches set theirs again.
    rebind: std::sync::atomic::AtomicBool,
    /// A list of ours (uploads), whose uses are no use of the title's
    /// (`crate::resource::evict`).
    ours: std::sync::atomic::AtomicBool,
    /// The draw or dispatch being recorded runs a program using title
    /// memory ([`HostList::writer_uses_memory`]).
    memory_writer: std::sync::atomic::AtomicBool,
    /// Reset, close, and the prelude's own reset. Two of those at once on
    /// one allocator removes the device: debug layer id 540, `E_INVALIDARG`,
    /// "the command allocator is currently in-use by another command list
    /// or is being reset in another thread."
    gate: Mutex<()>,
    /// The main list is open for recording. Create and a successful Reset
    /// leave it open; Close leaves it closed.
    open: AtomicBool,
    /// Submissions from ExecuteCommandLists through the copy-back after the
    /// fence. Reset waits this out: an allocator reset while the GPU still
    /// has the list removes the device.
    flying: AtomicU32,
    /// The list that runs those transitions just before this one.
    prelude: (ID3D12CommandAllocator, ID3D12GraphicsCommandList),
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: Mutex<std::collections::BTreeSet<String>>,
    /// Which list this is, for the name that tells it apart in device-removed
    /// data (see [`HostList::is_node`]).
    id: u64,
    /// The work its indirect executions may still do (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted.
    budget: OnceLock<Option<(ID3D12Resource, usize)>>,
    /// Clamped indirect arguments (`crate::indirect`), reused by each
    /// execution in turn; replaced by a bigger one when too small.
    arguments: Mutex<Option<ID3D12Resource>>,
    /// Mesh fallback outputs for the recording in progress. Reset returns
    /// them to a pool: the GPU is finished (`flying` is 0), and keeping a
    /// set on every list that had drawn filled the card.
    mesh: Mutex<Option<MeshGpu>>,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    notes: Mutex<Vec<Note>>,
    /// Sparse timestamp pairs around sampled commands, without DRED.
    #[cfg(feature = "diagnostics")]
    profile: Mutex<ProfileNotes>,
    /// In a frame `crate::capture` lists: the textures the list renders to
    /// or writes, and the pipeline that first wrote each (diagnostics).
    #[cfg(feature = "diagnostics")]
    targets: Mutex<Vec<(usize, String)>>,
    /// Actual writes to the selected lighting target, retained until this
    /// list is submitted so the report uses GPU order, not recording order.
    #[cfg(feature = "diagnostics")]
    lit_writes: Mutex<Vec<(usize, String)>>,
    /// With `VICTORY_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
}

// SAFETY: D3D12 objects are free-threaded; a host list is recorded by the
// title list's one recording thread, then run by one submission at a time
// (the queues hold it between, `crate::hold`).
unsafe impl Send for HostList {}
// SAFETY: as above.
unsafe impl Sync for HostList {}

pub(crate) fn warn_depth_bounds() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::warn!("the host GPU has no depth bounds test; depth bounds are ignored")
    });
}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct ProfileNotes {
    seen: u64,
    next: u32,
    pending: Option<(u32, String)>,
    samples: Vec<(u32, u32, String)>,
}

#[cfg(feature = "diagnostics")]
impl ProfileNotes {
    /// Called before each noted command. The next note closes a sampled
    /// command's interval; state-only commands between notes are included.
    fn before(&mut self, stride: u64, text: impl FnOnce() -> String, mut stamp: impl FnMut(u32)) {
        self.finish(&mut stamp);
        let seen = self.seen;
        self.seen += 1;
        if !seen.is_multiple_of(stride) || self.next + 1 >= STAMPS {
            return;
        }
        let text = text();
        if text.starts_with("DROPPED") {
            return;
        }
        let start = self.next;
        stamp(start);
        self.next += 1;
        self.pending = Some((start, text));
    }

    fn finish(&mut self, mut stamp: impl FnMut(u32)) {
        if let Some((start, text)) = self.pending.take() {
            let end = self.next;
            stamp(end);
            self.next += 1;
            self.samples.push((start, end, text));
        }
    }
}

/// One host texture's states in a list being recorded. `primary_at` and
/// `alias_at` are the latest recorded states. An alias-first list also keeps
/// the primary's state at recording time, which its first in-list switch
/// from alias to primary assumes. The prelude must establish that state.
struct Tracked {
    first: u32,
    last: u32,
    first_alias: bool,
    last_alias: bool,
    /// An alias-first list later switched to the primary. Its recorded
    /// transition assumes `primary_initial`, which the submission prelude
    /// must establish even if an earlier list changed the real primary state.
    primary_after_alias: bool,
    /// State assumed by the first alias-to-primary transition. Unlike
    /// `primary_at`, this must not change as later uses are recorded.
    primary_initial: u32,
    primary_at: u32,
    alias_at: u32,
    copy: std::sync::Arc<crate::resource::HostTexture>,
}

type States = std::collections::HashMap<usize, Tracked>;

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_DRED"))
}

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`VICTORY_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `VICTORY_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_GPU_TIMING"))
}

/// Samples one noted host command in every `stride` without serializing the
/// title's command lists. DRED markers and per-list timing stay separate.
#[cfg(feature = "diagnostics")]
fn profile_stride() -> Option<u64> {
    static STRIDE: OnceLock<Option<u64>> = OnceLock::new();
    *STRIDE.get_or_init(|| {
        let raw = std::env::var("VICTORY_GPU_PROFILE_STRIDE").ok()?;
        let stride = raw.parse::<u64>().ok().filter(|n| (1..=256).contains(n));
        if stride.is_none() {
            tracing::warn!(value = %raw, "GPU profile stride must be 1..=256; profile disabled");
        } else if timing() || dred_on() {
            tracing::warn!("GPU sampled profile needs per-list timing and DRED disabled");
            return None;
        } else {
            tracing::info!(stride, "sampled GPU command profile enabled");
        }
        stride
    })
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    if dred_on()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// The id of the next host list (see [`HostList::is_node`]).
static NEXT_LIST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The name a host list gets with device-removed data on.
fn list_name(id: u64) -> String {
    format!("victory host list {id}")
}

/// The pointers one of our list objects answers to: its own and the one each
/// interface a runtime may record it by gives (COM identity is the
/// `IUnknown` one). Only our live object is queried: a node of the
/// device-removed data may name a list that is long released, so its pointer
/// is compared and never dereferenced.
fn identities(list: &impl Interface) -> Vec<usize> {
    let mut found = vec![list.as_raw() as usize];
    let mut add = |pointer: Option<*mut core::ffi::c_void>| {
        if let Some(pointer) = pointer {
            found.push(pointer as usize);
        }
    };
    add(list
        .cast::<windows::core::IUnknown>()
        .ok()
        .map(|i| i.as_raw()));
    add(list.cast::<ID3D12Object>().ok().map(|i| i.as_raw()));
    add(list.cast::<ID3D12DeviceChild>().ok().map(|i| i.as_raw()));
    add(list.cast::<ID3D12CommandList>().ok().map(|i| i.as_raw()));
    found.sort_unstable();
    found.dedup();
    found
}

/// Whether a node of the device-removed data is a list: by pointer, or by the
/// name the runtime copied when the list was submitted. With the debug layer
/// on, the object we hold is its wrapper and the runtime records another
/// one, so no pointer of ours matches, and it appends its own note to our
/// name ("victory host list 158 (debug layer indirect)"). The name matches
/// exactly or followed by " (": never as the start of another number.
fn node_is_list(
    node_pointer: Option<usize>,
    node_name: Option<&str>,
    pointers: &[usize],
    name: &str,
) -> bool {
    node_pointer.is_some_and(|p| pointers.contains(&p))
        || node_name.is_some_and(|n| {
            n == name
                || n.strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with(" ("))
        })
}

/// Upload chunks handed out front to back: the chunks, the one in use and
/// the bytes used in it.
#[derive(Default)]
struct Scratch {
    chunks: Vec<ScratchChunk>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

const SCRATCH_CHUNK: u64 = 4 << 20;
const FIRST_SCRATCH_CHUNK: u64 = 64 << 10;

struct ScratchChunk {
    _buffer: ID3D12Resource,
    mapped: usize,
    gpu: u64,
    capacity: u64,
}

/// One piece of submission-time work, in recorded order.
enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory.
    Fill(usize, usize, usize),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only.
    Host(Box<dyn Fn() + Send>),
    /// Shaders write a range (address, bytes) of a buffer's host copy;
    /// whether they mark the pages they write (no snapshot then), and
    /// whether their program uses title memory (then the range is withheld
    /// when such a list misses, `crate::memory::withholding`).
    GpuWrite(
        std::sync::Arc<crate::shadow::Shadow>,
        usize,
        usize,
        bool,
        bool,
    ),
    /// Brings a texture's staging buffer up to date: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    Texture(
        std::sync::Arc<crate::texture::Staging>,
        std::sync::Arc<crate::resource::HostTexture>,
    ),
    /// Brings every buffer range a view heap's views reach up to date
    /// (`crate::bindless`).
    Heap(std::sync::Arc<crate::bindless::Viewed>),
    /// Shaders read title memory through a view heap's page table (the
    /// heap's key, `crate::memory`): the table names every copy, each
    /// brought up to date.
    Memory(usize),
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload buffers its GPU commands read, kept until the list is reset.
#[derive(Default)]
struct Deferred {
    /// Destination, source, bytes.
    copies: Vec<Step>,
    uploads: Vec<ID3D12Resource>,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    kept: Vec<windows::core::IUnknown>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize)>,
    /// Textures whose contents the list brings up to date.
    textures: std::collections::HashSet<usize>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
    /// The memory values the list waits for before it starts (a list the
    /// title records with a wait after commands is cut there,
    /// `crate::list`).
    waits: Vec<crate::hold::ValueWait>,
    /// Whether a step zeroes the indirect-work budget.
    budget_zeroed: bool,
    /// Work done once the list has run, before the bottom-of-pipe writes
    /// (title memory shaders wrote at addresses they computed goes back).
    after: Vec<AfterWork>,
    /// Asked once the list has run, before anything goes back: whether its
    /// shaders read title memory with no copy (`crate::memory`); then what
    /// they wrote through views does not go back either.
    missed: Vec<Box<dyn Fn() -> bool + Send>>,
}

struct AfterWork {
    run: Box<dyn Fn() + Send>,
    #[cfg(feature = "diagnostics")]
    kind: &'static str,
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both); logged once.
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    /// Whether the host runs 64-bit atomics on typed resources in the
    /// descriptor heap (translated 64-bit image atomics); logged once.
    pub(crate) fn heap_atomics64(&self) -> bool {
        static ATOMICS: OnceLock<bool> = OnceLock::new();
        *ATOMICS.get_or_init(|| {
            let mut typed = D3D12_FEATURE_DATA_D3D12_OPTIONS9::default();
            let mut heap = D3D12_FEATURE_DATA_D3D12_OPTIONS11::default();
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS9,
                        (&raw mut typed).cast(),
                        size_of_val(&typed) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_D3D12_OPTIONS11,
                            (&raw mut heap).cast(),
                            size_of_val(&heap) as u32,
                        )
                        .is_ok()
            };
            let atomics = ok
                && typed.AtomicInt64OnTypedResourceSupported.as_bool()
                && heap.AtomicInt64OnDescriptorHeapResourceSupported.as_bool();
            tracing::info!(atomics, "host 64-bit atomics on typed heap resources");
            atomics
        })
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            let prelude_allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let prelude: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &prelude_allocator,
                None,
            )?;
            prelude.Close()?;
            let id = NEXT_LIST.fetch_add(1, Ordering::Relaxed);
            name(&list, || list_name(id));
            Ok(HostList {
                allocator,
                depth_bounds: self
                    .depth_bounds_supported
                    .then(|| list.cast())
                    .transpose()?,
                list,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                twins: Mutex::default(),
                rebind: false.into(),
                ours: false.into(),
                memory_writer: false.into(),
                gate: Mutex::new(()),
                open: AtomicBool::new(true),
                flying: AtomicU32::new(0),
                prelude: (prelude_allocator, prelude),
                pipelines: Mutex::default(),
                id,
                budget: OnceLock::new(),
                arguments: Mutex::new(None),
                mesh: Mutex::new(None),
                #[cfg(feature = "diagnostics")]
                notes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                profile: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                targets: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                lit_writes: Mutex::default(),
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
            })
        }
    }

    /// Runs closed lists and waits for them.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        // One submission at a time from preparation to copy-back: snapshots
        // and the ranges shaders write are per host copy, not per submission.
        #[cfg(feature = "diagnostics")]
        let waiting_for_submission = Instant::now();
        let mut submitted = self.submitted.lock().unwrap_or_else(|e| e.into_inner());
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        EXECUTE_PHASE_US[0].fetch_add(
            started.duration_since(waiting_for_submission).as_micros() as u64,
            Ordering::Relaxed,
        );
        #[cfg(feature = "diagnostics")]
        let faults = crate::copystat::page_faults();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::Submissions, 1);
        // Host textures the title has not used for a while (or past the
        // card's budget) are given up first: the last submission has run.
        crate::resource::evict_unused();
        // Descriptors the title copied into its heaps itself, before the
        // heaps' views are brought up to date below.
        #[cfg(feature = "diagnostics")]
        let syncing =
            crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::SyncHeapsUs);
        crate::descriptor::sync_heaps();
        #[cfg(feature = "diagnostics")]
        drop(syncing);
        // Textures heap-indexing shaders may read, uploaded first by a list
        // of ours where their title memory changed.
        let heaps: Vec<std::sync::Arc<crate::bindless::Viewed>> = runs
            .iter()
            .flat_map(|l| {
                l.deferred()
                    .copies
                    .iter()
                    .filter_map(|s| match s {
                        Step::Heap(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        // And textures read through tables, uploaded by the same list.
        let textures: Vec<_> = runs
            .iter()
            .flat_map(|l| {
                l.deferred()
                    .copies
                    .iter()
                    .filter_map(|s| match s {
                        Step::Texture(staging, copy) => Some((staging.clone(), copy.clone())),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        #[cfg(feature = "diagnostics")]
        let uploading =
            crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::UploadsUs);
        let uploads = (!heaps.is_empty() || !textures.is_empty())
            .then(|| crate::bindless::uploads(&heaps, &textures))
            .flatten();
        #[cfg(feature = "diagnostics")]
        drop(uploading);
        let runs: Vec<&HostList> = uploads
            .as_ref()
            .map(|u| u.list())
            .into_iter()
            .chain(runs.iter().copied())
            .collect();
        #[cfg(feature = "diagnostics")]
        let checkpoint_range = crate::capture::lit_ffx_checkpoint_range(&runs);
        #[cfg(feature = "diagnostics")]
        let checkpoints: Vec<HostList> = checkpoint_range
            .map(|(start, end)| {
                (start..end)
                    .filter_map(|n| {
                        let list = self.new_list().ok()?;
                        crate::capture::snapshot_lit_checkpoint(&list, n - start);
                        list.close();
                        Some(list)
                    })
                    .collect()
            })
            .unwrap_or_default();
        #[cfg(feature = "diagnostics")]
        let runs: Vec<&HostList> = if let Some((start, end)) = checkpoint_range
            && checkpoints.len() == end - start
        {
            let mut with_checkpoints = Vec::with_capacity(runs.len() + checkpoints.len());
            for (i, run) in runs.iter().enumerate() {
                with_checkpoints.push(*run);
                if (start..end).contains(&i) {
                    with_checkpoints.push(&checkpoints[i - start]);
                }
            }
            with_checkpoints
        } else {
            runs
        };
        #[cfg(feature = "diagnostics")]
        let writer_checkpoint_specs = crate::capture::lit_writer_checkpoint_indices(&runs);
        #[cfg(feature = "diagnostics")]
        for &(list_index, checkpoint) in &writer_checkpoint_specs {
            if checkpoint < 3 {
                continue;
            }
            let pipelines = runs[list_index].pipelines.lock().unwrap_or_else(|e| e.into_inner());
            tracing::info!(checkpoint, original_list = list_index, list_id = runs[list_index].id(),
                pipelines = ?*pipelines, "lighting checkpoint list pipelines");
            let targets = runs[list_index].targets.lock().unwrap_or_else(|e| e.into_inner());
            for &(resource, ref by) in targets.iter() {
                if crate::capture::lit_target_alias(resource) {
                    tracing::info!(checkpoint, original_list = list_index,
                        resource = format_args!("{resource:#x}"), %by,
                        "lighting checkpoint list target binding");
                }
            }
            let notes = runs[list_index].notes.lock().unwrap_or_else(|e| e.into_inner());
            for (operation, (note, _)) in notes.iter().enumerate() {
                tracing::info!(checkpoint, original_list = list_index, operation, %note,
                    "lighting checkpoint list operation");
            }
        }
        #[cfg(feature = "diagnostics")]
        let writer_checkpoints: Vec<(usize, HostList)> = writer_checkpoint_specs
            .iter()
            .filter_map(|&(index, number)| {
                let list = self.new_list().ok()?;
                crate::capture::snapshot_lit_writer_checkpoint(&list, number);
                list.close();
                Some((index, list))
            })
            .collect();
        #[cfg(feature = "diagnostics")]
        let runs: Vec<&HostList> = if writer_checkpoint_specs.len() == writer_checkpoints.len() {
            let mut with_checkpoints = Vec::with_capacity(runs.len() + writer_checkpoints.len());
            let mut next_checkpoint = writer_checkpoints.iter().peekable();
            for (index, run) in runs.iter().enumerate() {
                with_checkpoints.push(*run);
                if next_checkpoint.peek().is_some_and(|(after, _)| *after == index) {
                    with_checkpoints.push(&next_checkpoint.next().unwrap().1);
                }
            }
            with_checkpoints
        } else {
            tracing::warn!("could not allocate every lighting writer checkpoint list");
            runs
        };
        #[cfg(feature = "diagnostics")]
        let early_checkpoint_specs = crate::capture::early_lit_checkpoint_indices(&runs);
        #[cfg(feature = "diagnostics")]
        let early_checkpoints: Vec<(usize, HostList)> = early_checkpoint_specs
            .iter()
            .filter_map(|&(index, number)| {
                let list = self.new_list().ok()?;
                crate::capture::snapshot_early_lit_checkpoint(&list, number);
                list.close();
                Some((index, list))
            })
            .collect();
        #[cfg(feature = "diagnostics")]
        let runs: Vec<&HostList> = if early_checkpoint_specs.len() == early_checkpoints.len() {
            let mut with_checkpoints = Vec::with_capacity(runs.len() + early_checkpoints.len());
            let mut next_checkpoint = early_checkpoints.iter().peekable();
            for (index, run) in runs.iter().enumerate() {
                with_checkpoints.push(*run);
                if next_checkpoint.peek().is_some_and(|(after, _)| *after == index) {
                    with_checkpoints.push(&next_checkpoint.next().unwrap().1);
                }
            }
            with_checkpoints
        } else {
            tracing::warn!("could not allocate every early lighting checkpoint list");
            runs
        };
        let runs = &runs[..];
        // A frame `crate::capture` lists: every command of every list, in
        // the order the GPU runs them (with `VICTORY_D3D12_DRED`, which
        // notes them).
        #[cfg(feature = "diagnostics")]
        let traced = crate::capture::listed(crate::queue::presented() + 1)
            || crate::capture::trace_lit_submission(*submitted + 1);
        #[cfg(feature = "diagnostics")]
        if traced {
            for (i, r) in runs.iter().enumerate() {
                let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                for (k, (note, _)) in notes.iter().enumerate() {
                    tracing::info!(
                        target: "victory_gpu::trace",
                        "s{} l{i}/{} #{k}: {note}",
                        *submitted + 1,
                        runs.len()
                    );
                }
            }
        }
        let mut lists: Vec<Option<ID3D12CommandList>> = Vec::with_capacity(2 * runs.len());
        // Where each run's lists start in `lists`.
        let mut firsts = Vec::with_capacity(runs.len());
        let mut batch = Batch::default();
        for list in runs {
            list.run_deferred(&mut batch);
            firsts.push(lists.len());
            #[cfg(feature = "diagnostics")]
            let _timed =
                crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::PreludeUs);
            if let Some(prelude) = list.prelude() {
                lists.push(Some(prelude));
            }
            lists.push(list.list.cast().ok());
        }
        batch.flush();
        let prepared = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::FaultsPrep,
            crate::copystat::page_faults().saturating_sub(faults),
        );
        // Diagnostics: each list on its own, timed (`VICTORY_GPU_TIMING`).
        #[cfg(feature = "diagnostics")]
        let groups: Vec<&[Option<ID3D12CommandList>]> = if timing() {
            firsts
                .iter()
                .zip(firsts.iter().skip(1).chain([&lists.len()]))
                .map(|(&a, &b)| &lists[a..b])
                .collect()
        } else {
            vec![&lists[..]]
        };
        #[cfg(not(feature = "diagnostics"))]
        let groups = [&lists[..]];
        // Another thread's list reset often observes a removal first. Copy
        // the notes before the wait: that reset clears its own list and
        // then aborts the run, before the check below can report.
        *IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()) = flight_report(runs);
        // Held across the execute and the copy-back. Reset waits: the
        // allocator cannot be reused while the GPU still has the list.
        // Set only now, after prelude and deferred work, so that work can
        // itself reset a list without waiting on this submission.
        for list in runs {
            list.begin_flight();
        }
        let mut removed_in = None;
        for (i, group) in groups.iter().enumerate() {
            *submitted += 1;
            let started = std::time::Instant::now();
            #[cfg(feature = "diagnostics")]
            crate::resource::life::note(0, crate::resource::life::SUBMITTED, *submitted, 0);
            // SAFETY: closed lists; the fence and event are ours.
            unsafe {
                self.queue.ExecuteCommandLists(group);
                let waited = self
                    .queue
                    .Signal(&self.fence, *submitted)
                    .and_then(|()| self.fence.SetEventOnCompletion(*submitted, self.event));
                match waited {
                    Ok(()) => {
                        WaitForSingleObject(self.event, INFINITE);
                    }
                    Err(e) => tracing::warn!(error = %e, "host submission not waited for"),
                }
            }
            #[cfg(feature = "diagnostics")]
            crate::resource::life::note(0, crate::resource::life::RAN, *submitted, 0);
            #[cfg(feature = "diagnostics")]
            if groups.len() > 1 {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                gpu_times(started.elapsed(), &runs[i].slowest(frequency, usize::MAX));
            }
            #[cfg(feature = "diagnostics")]
            if groups.len() > 1 && started.elapsed() > std::time::Duration::from_millis(50) {
                // SAFETY: a plain query.
                let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
                let commands = runs[i]
                    .notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len();
                tracing::warn!(
                    ms = started.elapsed().as_millis(),
                    commands,
                    "slow host list"
                );
                for (ms, op) in runs[i].slowest(frequency, 6) {
                    tracing::warn!("slow list command, {ms:.1} ms: {op}");
                }
            }
            if groups.len() > 1
                && removed_in.is_none()
                && {
                    // SAFETY: querying the live device does not retain pointers.
                    unsafe { self.device.GetDeviceRemovedReason() }.is_err()
                }
            {
                removed_in = Some(i);
            }
            let _ = (i, started);
        }
        let _ = &firsts;
        let gpu_done = Instant::now();
        let gpu = gpu_done.duration_since(prepared);
        #[cfg(feature = "diagnostics")]
        if profile_stride().is_some() {
            // SAFETY: the queue has completed every list above.
            let frequency = unsafe { self.queue.GetTimestampFrequency() }.unwrap_or(1);
            for list in runs {
                gpu_profile(&list.profile_samples(frequency));
            }
        }
        SUBMISSION_US[0].fetch_add((prepared - started).as_micros() as u64, Ordering::Relaxed);
        SUBMISSION_US[1].fetch_add(gpu.as_micros() as u64, Ordering::Relaxed);
        // The host resets a device whose submission runs for about 2 s.
        if gpu > std::time::Duration::from_millis(250) {
            let pipelines: std::collections::BTreeSet<String> = runs
                .iter()
                .flat_map(|l| {
                    l.pipelines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                })
                .collect();
            tracing::warn!(
                ms = gpu.as_millis(),
                lists = runs.len(),
                ?pipelines,
                "slow host submission"
            );
        }
        // SAFETY: a plain query.
        if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
            fail_after(
                format_args!(
                    "host GPU device removed after a submission of {} lists{}{}",
                    runs.len(),
                    last_notes(runs),
                    removed_during(runs, removed_in)
                ),
                runs,
            );
        }
        // What shaders wrote goes back before the bottom-of-pipe writes,
        // which tell the title the GPU is done; each range once per
        // submission, however many dispatches declared it (the world's
        // passes declare the same big ranges dozens of times).
        let copying = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let mut copy_phase = copying;
        #[cfg(feature = "diagnostics")]
        EXECUTE_PHASE_US[2].fetch_add(
            copying.duration_since(gpu_done).as_micros() as u64,
            Ordering::Relaxed,
        );
        #[cfg(feature = "diagnostics")]
        let faults = crate::copystat::page_faults();
        // One list's lock at a time: a submission may name a list twice.
        let mut back: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize, bool)> = Vec::new();
        let mut index: std::collections::HashMap<(usize, usize, usize), usize> =
            std::collections::HashMap::new();
        // Shaders that read memory with no copy computed something else than
        // the console's: it stays out of title memory, where it broke the
        // title's own data (runs c1, c4), and so does what every program
        // using title memory wrote while any misses
        // (`crate::memory::withholding`). Only lists with such programs have
        // checks; all are asked before anything goes back. Other programs in
        // those lists write through bounded views from their own inputs:
        // their ranges go back (the imposter transforms beside the BVH
        // builder in one list, run light13).
        let missed: Vec<bool> = runs
            .iter()
            .map(|list| list.deferred().missed.iter().any(|check| check()))
            .collect();
        crate::memory::note_submission(missed.contains(&true));
        let withholding = crate::memory::withholding();
        for (list, &missed) in runs.iter().zip(&missed) {
            let deferred = list.deferred();
            let missed = missed || withholding && !deferred.missed.is_empty();
            for step in &deferred.copies {
                if let Step::GpuWrite(shadow, address, len, _, memory) = step {
                    let key = (std::sync::Arc::as_ptr(shadow) as usize, *address, *len);
                    // A range both kinds declare is withheld whole.
                    let goes_back = !(missed && *memory);
                    match index.get(&key) {
                        Some(&i) => back[i].3 &= goes_back,
                        None => {
                            index.insert(key, back.len());
                            back.push((shadow.clone(), *address, *len, goes_back));
                        }
                    }
                }
            }
        }
        // A range overlapping a withheld one of the same copy is withheld
        // too: its copy-back would take the other's bytes along (pages a
        // program using title memory and another both wrote).
        let held: Vec<(usize, usize, usize)> = back
            .iter()
            .filter(|b| !b.3)
            .map(|(s, a, n, _)| (std::sync::Arc::as_ptr(s) as usize, *a, *n))
            .collect();
        for b in back.iter_mut().filter(|b| b.3) {
            let copy = std::sync::Arc::as_ptr(&b.0) as usize;
            b.3 = !held
                .iter()
                .any(|&(c, a, n)| c == copy && a < b.1 + b.2 && b.1 < a + n);
        }
        // Pages shaders marked first: the copy-backs below end what the
        // copies keep for them.
        let withheld: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = back
            .iter()
            .filter(|b| !b.3)
            .map(|(s, a, n, _)| (s.clone(), *a, *n))
            .collect();
        #[cfg(feature = "diagnostics")]
        copyback_phase(0, &mut copy_phase);
        crate::shadow::copy_back_marks(&withheld, withholding);
        #[cfg(feature = "diagnostics")]
        copyback_phase(1, &mut copy_phase);
        // Texture views left null that shaders read: made for what follows.
        let read = crate::shadow::take_reads();
        if !read.is_empty() {
            crate::descriptor::make_read(&read);
        }
        #[cfg(feature = "diagnostics")]
        copyback_phase(2, &mut copy_phase);
        for (shadow, address, len, goes_back) in &back {
            let (address, len) = (*address, *len);
            if *goes_back {
                let written = shadow.copy_back(address, len);
                COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            } else {
                shadow.keep_writes(address, len);
                crate::memory::note_withheld(len as u64);
                crate::memory::drop_withheld(address, len);
            }
        }
        #[cfg(feature = "diagnostics")]
        copyback_phase(3, &mut copy_phase);
        #[cfg(feature = "diagnostics")]
        let (mut capture_hooks_us, mut callbacks_us) = (0u64, 0u64);
        #[cfg(feature = "diagnostics")]
        let mut callback_types: std::collections::HashMap<&'static str, (u64, u64, u64)> =
            std::collections::HashMap::new();
        for (list_index, list) in runs.iter().enumerate() {
            #[cfg(feature = "diagnostics")]
            let hooks_started = Instant::now();
            #[cfg(feature = "diagnostics")]
            crate::capture::snapshot_order(*submitted, list_index);
            #[cfg(not(feature = "diagnostics"))]
            let _ = list_index;
            #[cfg(feature = "diagnostics")]
            if crate::capture::trace_lit_writers_only_on() {
                let writes = list.lit_writes.lock().unwrap_or_else(|e| e.into_inner());
                for (write_index, (resource, pipeline)) in writes.iter().enumerate() {
                    if *resource == 0 {
                        continue;
                    }
                    tracing::info!(
                        target: "victory_gpu::lit_writes",
                        submission = *submitted,
                        list_index,
                        write_index,
                        resource = format_args!("{resource:#x}"),
                        %pipeline,
                        "lighting target write in submission order"
                    );
                }
            }
            #[cfg(feature = "diagnostics")]
            {
                capture_hooks_us += hooks_started.elapsed().as_micros() as u64;
            }
            for work in &list.deferred().after {
                #[cfg(feature = "diagnostics")]
                let callback_started = Instant::now();
                (work.run)();
                #[cfg(feature = "diagnostics")]
                {
                    let us = callback_started.elapsed().as_micros() as u64;
                    callbacks_us += us;
                    let entry = callback_types.entry(work.kind).or_default();
                    (entry.0, entry.1, entry.2) = (entry.0 + us, entry.1 + 1, entry.2.max(us));
                }
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            COPYBACK_PHASE_US[4].fetch_add(capture_hooks_us, Ordering::Relaxed);
            COPYBACK_PHASE_US[5].fetch_add(callbacks_us, Ordering::Relaxed);
            let mut all = after_times().lock().unwrap_or_else(|e| e.into_inner());
            for (kind, (us, n, max)) in callback_types {
                let entry = all.entry(kind).or_default();
                (entry.0, entry.1, entry.2) = (entry.0 + us, entry.1 + n, entry.2.max(max));
            }
        }
        COPY_BACK[0].fetch_add(copying.elapsed().as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        let copyback_done = Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::FaultsBack,
            crate::copystat::page_faults().saturating_sub(faults),
        );
        for list in runs {
            for &(at, what, bytes) in &list.deferred().writes {
                what.write(at as *mut u8, bytes);
            }
        }
        for list in runs {
            list.end_flight();
        }
        // The submission has run and the next waits for our lock. Host
        // copies the title gave up meanwhile may have their last holder here.
        release_retired();
        release_later(Box::new((batch, back, withheld, heaps)));
        self.log_messages();
        // A listed frame: what this submission's lists drew, as it is now
        // (the dumps submit lists of their own). A first 348e input capture
        // promotes only its three tagged SRVs, without turning on an
        // all-target dump for this long world frame.
        #[cfg(feature = "diagnostics")]
        if traced || crate::capture::lit_inputs_on() || crate::switch::on("VICTORY_CAPTURE_FFX") {
            let mut targets: Vec<(usize, String)> = Vec::new();
            for r in runs {
                for t in r.targets.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                    if traced || crate::capture::lit_input_target(&t.1)
                        || crate::capture::lit_packed_writer_target(&t.1)
                        || crate::capture::ffx_target(&t.1)
                    {
                        if let Some(old) = targets.iter_mut().find(|x| x.0 == t.0) {
                            if t.1.starts_with("ffx-") {
                                *old = t.clone();
                            }
                        } else {
                            targets.push(t.clone());
                        }
                    }
                }
            }
            if traced || !targets.is_empty() {
                let sub = *submitted;
                drop(submitted);
                crate::capture::dump_submission(sub, &targets);
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            let finished = Instant::now();
            EXECUTE_PHASE_US[1].fetch_add(
                finished.duration_since(started).as_micros() as u64,
                Ordering::Relaxed,
            );
            EXECUTE_PHASE_US[3].fetch_add(
                finished.duration_since(copyback_done).as_micros() as u64,
                Ordering::Relaxed,
            );
        }
    }

    /// With `VICTORY_D3D12_DRED`: the faulting address and the allocations
    /// it lies in (by the names we give them) or the title memory copy just
    /// below it; for each list that did not finish, the command it stopped
    /// at and what the draws and dispatches in flight there run (or, in
    /// lists that never started, the pipelines they bind).
    fn removal_data(&self, runs: &[&HostList]) -> String {
        if !dred_on() {
            return "; DRED disabled (set VICTORY_D3D12_DRED=1 for fault data)".into();
        }
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData1>() else {
            return String::new();
        };
        let mut text = String::new();
        let mut fault_va = None;
        // SAFETY: the outputs are the runtime's, valid while the device
        // lives; nodes are walked as linked, histories have their counts.
        unsafe {
            if let Ok(fault) = dred.GetPageFaultAllocationOutput1()
                && fault.PageFaultVA != 0
            {
                let va = fault.PageFaultVA;
                fault_va = Some(va);
                text.push_str(&format!("; page fault at {va:#x}"));
                for (what, head) in [
                    ("inside", fault.pHeadExistingAllocationNode),
                    ("inside freed", fault.pHeadRecentFreedAllocationNode),
                ] {
                    let mut names = Vec::new();
                    let mut node = head;
                    while let Some(n) = node.as_ref()
                        && names.len() < 8
                    {
                        let name = if n.ObjectNameW.is_null() {
                            "unnamed".to_owned()
                        } else {
                            n.ObjectNameW.to_string().unwrap_or_default()
                        };
                        names.push(format!("{name} (type {})", n.AllocationType.0));
                        node = n.pNext;
                    }
                    if !names.is_empty() {
                        text.push_str(&format!("; {what} {names:?}"));
                    }
                    #[cfg(feature = "diagnostics")]
                    for name in names.iter().filter(|_| what == "inside freed") {
                        text.push_str(&crate::resource::life::of(name));
                    }
                }
                text.push_str(&crate::shadow::near(va));
                text.push_str(&crate::descriptor::host::near_heaps(va));
            }
            // The plain output: the extended one (with marker texts) came
            // back without stopped lists on this host.
            let crumbs = match dred.GetAutoBreadcrumbsOutput() {
                Ok(crumbs) => crumbs,
                Err(e) => {
                    text.push_str(&format!("; no breadcrumbs ({e})"));
                    return text;
                }
            };
            // The nodes, oldest list first, and which run each is.
            let mut nodes = Vec::new();
            let mut node = crumbs.pHeadAutoBreadcrumbNode;
            while let Some(p) = node.as_ref() {
                nodes.push(p);
                node = p.pNext;
            }
            let index: Vec<Option<usize>> = nodes
                .iter()
                .map(|p| runs.iter().position(|r| r.is_node(p)))
                .collect();
            if let Some(last) = nodes.last()
                && index.iter().all(Option::is_none)
            {
                let name = |w: windows::core::PCWSTR, a: *const u8| {
                    let w = (!w.is_null()).then(|| w.to_string().unwrap_or_default());
                    let a = (!a.is_null()).then(|| {
                        std::ffi::CStr::from_ptr(a.cast())
                            .to_string_lossy()
                            .into_owned()
                    });
                    format!("{w:?}/{a:?}")
                };
                tracing::warn!(
                    nodes = nodes.len(),
                    last_pointer = ?last.pCommandList.as_ref().map(|l| l.as_raw()),
                    last_names = %name(last.pCommandListDebugNameW, last.pCommandListDebugNameA),
                    "no breadcrumb node named one of our lists"
                );
            }
            // Every list's progress (done/count), in the runtime's order.
            let progress: Vec<String> = nodes
                .iter()
                .zip(&index)
                .map(|(p, index)| {
                    let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                    format!("{done}/{} {index:?}", p.BreadcrumbCount)
                })
                .collect();
            text.push_str(&format!("; breadcrumbs (done/count, list) {progress:?}"));
            for (p, index) in nodes.iter().zip(&index) {
                let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
                if done < p.BreadcrumbCount && !p.pCommandHistory.is_null() {
                    let history =
                        std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize);
                    text.push_str(&format!(
                        "; a list stopped at command {done} of {} (op {})",
                        p.BreadcrumbCount, history[done as usize].0
                    ));
                    let run = index.map(|i| runs[i]);
                    // The k-th marker command is the k-th note.
                    #[cfg(feature = "diagnostics")]
                    let markers: Vec<(u32, String)> = run.map_or_else(Vec::new, |r| {
                        let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
                        history
                            .iter()
                            .enumerate()
                            .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
                            .zip(notes.iter())
                            .map(|((at, _), (note, range))| {
                                let place = fault_va.zip(*range).map(|(va, r)| fault_in(va, r));
                                (at as u32, format!("{note}{}", place.unwrap_or_default()))
                            })
                            .collect()
                    });
                    #[cfg(not(feature = "diagnostics"))]
                    let markers: Vec<(u32, String)> = Vec::new();
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = fault_va;
                    if done > 0 {
                        text.push_str(&in_flight(history, &markers, done as usize));
                    } else if let Some(r) = run {
                        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
                        text.push_str(&format!(", its pipelines {pipelines:?}"));
                    }
                }
            }
        }
        text
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    // These three informational diagnostics recur for ordinary
                    // pipeline-cache misses, empty input layouts, and clear
                    // value hints. Keep representative messages and powers of
                    // two, while never suppressing an error or a new ID.
                    let repeated = if m.Severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0 {
                        static CACHE_MISS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        static EMPTY_INPUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        static CLEAR_HINT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let counter = match m.ID.0 {
                            971 => Some(&CACHE_MISS),
                            253 => Some(&EMPTY_INPUT),
                            820 => Some(&CLEAR_HINT),
                            _ => None,
                        };
                        counter.map(|counter| counter.fetch_add(1, Ordering::Relaxed) + 1)
                    } else {
                        None
                    };
                    if repeated.is_some_and(|count| count > 8 && !count.is_power_of_two()) {
                        continue;
                    }
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, occurrence = repeated, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// A host render target view of `resource`, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A slot of CPU-only views, valid until it is recorded.
    pub(crate) fn cpu_view(&self) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_cpu_view.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a live heap.
        let mut handle = unsafe { self.cpu_views.GetCPUDescriptorHandleForHeapStart() };
        handle.ptr += (slot * self.cpu_view_size) as usize;
        handle
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }
}

impl HostList {
    #[cfg(feature = "diagnostics")]
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() && profile_stride().is_none() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC {
                                Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                                Count: STAMPS,
                                NodeMask: 0,
                            },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: (count + 1) * 8,
        };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (
                    ticks as f64 * 1e3 / frequency.max(1) as f64,
                    notes[i].0.chars().take(300).collect(),
                )
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    /// Completed sparse command intervals. The GPU fence has already
    /// signalled, so the read-back buffer is ready without per-list waits.
    #[cfg(feature = "diagnostics")]
    fn profile_samples(&self, frequency: u64) -> Vec<(f64, String)> {
        let profile = self.profile.lock().unwrap_or_else(|e| e.into_inner());
        if profile.samples.is_empty() {
            return Vec::new();
        }
        let Some((_, back)) = self.stamps() else { return Vec::new() };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: profile.next as usize * 8 };
        // SAFETY: the list resolved every sampled timestamp before the fence.
        let stamps = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let values = std::slice::from_raw_parts(data.cast::<u64>(), profile.next as usize).to_vec();
            back.Unmap(0, None);
            values
        };
        profile.samples.iter().map(|(start, end, note)| {
            let ticks = stamps[*end as usize].saturating_sub(stamps[*start as usize]);
            (ticks as f64 * 1e3 / frequency.max(1) as f64, note.chars().take(300).collect())
        }).collect()
    }

    /// Whether `node` of the device-removed data (its breadcrumbs) is this
    /// list.
    fn is_node(&self, node: &D3D12_AUTO_BREADCRUMB_NODE) -> bool {
        // SAFETY: only the pointer and the name text are read: the pointer is
        // compared, never used, and the name is the runtime's copy.
        let (pointer, wide, ansi) = unsafe {
            (
                node.pCommandList.as_ref().map(|l| l.as_raw() as usize),
                (!node.pCommandListDebugNameW.is_null())
                    .then(|| node.pCommandListDebugNameW.to_string().unwrap_or_default()),
                (!node.pCommandListDebugNameA.is_null()).then(|| {
                    std::ffi::CStr::from_ptr(node.pCommandListDebugNameA.cast())
                        .to_string_lossy()
                        .into_owned()
                }),
            )
        };
        let (pointers, name) = (identities(&self.list), list_name(self.id));
        node_is_list(pointer, wide.as_deref(), &pointers, &name)
            || node_is_list(None, ansi.as_deref(), &pointers, &name)
    }

    pub(crate) fn close(&self) {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if !self.open.swap(false, Ordering::AcqRel) {
            return;
        }
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            if profile_stride().is_some() {
                let mut profile = self.profile.lock().unwrap_or_else(|e| e.into_inner());
                profile.finish(|at| {
                    // SAFETY: an open list and a timestamp slot below STAMPS.
                    unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
                });
                if profile.next > 0 {
                    // SAFETY: the query heap and read-back buffer cover these slots.
                    unsafe {
                        self.list.ResolveQueryData(
                            heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, profile.next, back, 0,
                        )
                    };
                }
            } else {
                let count =
                    (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
                // SAFETY: an open list, our heap and read-back buffer.
                unsafe {
                    self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                    self.list
                        .ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
                }
            }
        }
        // SAFETY: our list, and it is open.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    /// The list is in a submission until [`HostList::end_flight`]. Reset
    /// waits, under `gate`, so it cannot slip between this and the execute.
    fn begin_flight(&self) {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.flying.fetch_add(1, Ordering::AcqRel);
    }

    fn end_flight(&self) {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        self.flying.fetch_sub(1, Ordering::AcqRel);
    }

    /// After the list's last submission has finished. An open list is closed
    /// first: Reset onto an allocator whose list is still recording returns
    /// `E_INVALIDARG` and the debug layer removes the device (id 540).
    pub(crate) fn reset(&self) {
        loop {
            let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            if self.flying.load(Ordering::Acquire) != 0 {
                drop(_gate);
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            *self.deferred() = Deferred::default();
            if let Some(gpu) = self.mesh.lock().unwrap_or_else(|e| e.into_inner()).take() {
                mesh_checkin(gpu);
            }
            self.states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.twins.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.pipelines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            #[cfg(feature = "diagnostics")]
            self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
            #[cfg(feature = "diagnostics")]
            {
                *self.profile.lock().unwrap_or_else(|e| e.into_inner()) = ProfileNotes::default();
            }
            #[cfg(feature = "diagnostics")]
            self.targets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            #[cfg(feature = "diagnostics")]
            self.lit_writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            {
                let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
                (scratch.current, scratch.used) = (0, 0);
            }
            if self.open.swap(false, Ordering::AcqRel) {
                // SAFETY: the list is open and not executing (`flying` is 0).
                if let Err(e) = unsafe { self.list.Close() } {
                    fail(format_args!(
                        "a host command list did not close before reset ({e})"
                    ));
                }
            }
            // SAFETY: the list is closed and not executing.
            let reset = unsafe {
                self.allocator
                    .Reset()
                    .and_then(|()| self.list.Reset(&self.allocator, None))
            };
            if let Err(e) = reset {
                // The device is often already removed by the submission still
                // inside its fence wait. Report that submission, not only the
                // Reset HRESULT that observed the removal.
                let extra = reset_context();
                fail(format_args!(
                    "a host command list did not reset ({e}){extra}"
                ));
            }
            self.open.store(true, Ordering::Release);
            return;
        }
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    /// A depth texture with a writable twin (`crate::twin`) has the side
    /// the use needs brought up to date first; unordered access moves the
    /// twin, not the depth texture (nothing returned).
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(ID3D12Resource, u32)> {
        // One host texture for the whole move (`crate::resource::evict` may
        // give it up meanwhile; this list then holds it).
        let copy = crate::resource::host_copy(resource)?;
        let texture = copy.texture.clone();
        if let Some(Some(twin)) = copy.twin.get()
            && crate::twin::route(self, &copy, twin, wanted)
        {
            return None;
        }
        let wanted = legal_state(&texture, wanted);
        self.track(&copy, wanted).map(|before| (texture, before))
    }

    /// The primary texture state at this point in this recorded list.
    /// A bounded diagnostic copy restores this after its readback.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn recorded_state(&self, copy: &std::sync::Arc<crate::resource::HostTexture>) -> u32 {
        self.states.lock().unwrap_or_else(|e| e.into_inner())
            .get(&(std::sync::Arc::as_ptr(copy) as usize))
            .map_or_else(|| copy.state.load(Ordering::Relaxed), |s| s.primary_at)
    }

    /// Samples `copy`'s R9G9B9E5 alias. The list's first sample is the
    /// prelude's; a later one records the aliasing barrier now, after the
    /// primary's writes. False when `copy` has no alias.
    pub(crate) fn use_shared_exp(
        &self,
        copy: &std::sync::Arc<crate::resource::HostTexture>,
    ) -> bool {
        const SRV: u32 = 0x40 | 0x80;
        self.use_shared_exp_state(copy, SRV)
    }

    /// Brings the paired R9 texture to a requested state. A packed-format
    /// repack first uses COPY_DEST, then returns it to shader-resource state.
    pub(crate) fn use_shared_exp_state(
        &self,
        copy: &std::sync::Arc<crate::resource::HostTexture>,
        state: u32,
    ) -> bool {
        let Some(alias) = copy.shared_exp_resource() else {
            return false;
        };
        let wanted = legal_state(&alias, state);
        let mut extra = Vec::new();
        {
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            match states.entry(std::sync::Arc::as_ptr(copy) as usize) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    copy.note_used();
                    if !self.ours.load(Ordering::Relaxed) {
                        copy.stamp();
                    }
                    let primary_initial = copy.state.load(Ordering::Relaxed);
                    v.insert(Tracked {
                        first: wanted,
                        last: wanted,
                        first_alias: true,
                        last_alias: true,
                        primary_after_alias: false,
                        primary_initial,
                        primary_at: primary_initial,
                        alias_at: wanted,
                        copy: copy.clone(),
                    });
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    let t = o.get_mut();
                    if !t.last_alias {
                        if !copy.shared_exp_separate() {
                            if t.primary_at & 0x8 != 0 {
                                extra.push(uav_barrier(&copy.texture));
                            }
                            extra.push(aliasing_barrier(&copy.texture, &alias));
                        }
                        if t.alias_at != wanted {
                            extra.push(transition(
                                &alias,
                                D3D12_RESOURCE_STATES(t.alias_at as i32),
                                D3D12_RESOURCE_STATES(wanted as i32),
                            ));
                        }
                        t.alias_at = wanted;
                        t.last_alias = true;
                        t.last = wanted;
                    } else if t.alias_at != wanted {
                        extra.push(transition(
                            &alias,
                            D3D12_RESOURCE_STATES(t.alias_at as i32),
                            D3D12_RESOURCE_STATES(wanted as i32),
                        ));
                        t.alias_at = wanted;
                        t.last = wanted;
                    }
                }
            }
        }
        if !extra.is_empty() {
            // SAFETY: an open list; the barriers name live textures.
            unsafe { self.list.ResourceBarrier(&extra) };
        }
        true
    }

    /// The list's state for host texture `copy` becomes `wanted` (a legal
    /// one): the state it leaves, if that is a change. Nothing for the
    /// list's first use (its prelude moves the texture there). A switch
    /// off the R9G9B9E5 alias records that barrier here and returns nothing.
    fn track(
        &self,
        copy: &std::sync::Arc<crate::resource::HostTexture>,
        wanted: u32,
    ) -> Option<u32> {
        let mut extra = Vec::new();
        let result = {
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            match states.entry(std::sync::Arc::as_ptr(copy) as usize) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    copy.note_used();
                    if !self.ours.load(Ordering::Relaxed) {
                        copy.stamp();
                    }
                    v.insert(Tracked {
                        first: wanted,
                        last: wanted,
                        first_alias: false,
                        last_alias: false,
                        primary_after_alias: false,
                        primary_initial: wanted,
                        primary_at: wanted,
                        alias_at: copy.alias_state(),
                        copy: copy.clone(),
                    });
                    None
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    let t = o.get_mut();
                    if t.last_alias
                        && let Some(alias) = copy.shared_exp_resource()
                    {
                        t.primary_after_alias |= t.first_alias;
                        if !copy.shared_exp_separate() {
                            extra.push(aliasing_barrier(&alias, &copy.texture));
                        }
                        if t.primary_at != wanted {
                            extra.push(transition(
                                &copy.texture,
                                D3D12_RESOURCE_STATES(t.primary_at as i32),
                                D3D12_RESOURCE_STATES(wanted as i32),
                            ));
                        }
                        t.primary_at = wanted;
                        t.last = wanted;
                        t.last_alias = false;
                        None
                    } else {
                        let before = std::mem::replace(&mut t.last, wanted);
                        t.primary_at = wanted;
                        t.last_alias = false;
                        (before != wanted).then_some(before)
                    }
                }
            }
        };
        if !extra.is_empty() {
            // SAFETY: an open list; the barriers name live textures.
            unsafe { self.list.ResourceBarrier(&extra) };
        }
        result
    }

    /// [`HostList::transition`] for a host texture of ours (a twin's
    /// plane), without twins: the barrier to record, if one is needed.
    pub(crate) fn move_texture(
        &self,
        copy: &std::sync::Arc<crate::resource::HostTexture>,
        wanted: u32,
    ) -> Option<D3D12_RESOURCE_BARRIER> {
        let wanted = legal_state(&copy.texture, wanted);
        let before = self.track(copy, wanted)?;
        Some(transition(
            &copy.texture,
            D3D12_RESOURCE_STATES(before as i32),
            D3D12_RESOURCE_STATES(wanted as i32),
        ))
    }

    /// A use of the depth texture `depth` (with a twin) needing `side`
    /// (`crate::twin::DEPTH` or `TWIN`): whether a copy must be recorded
    /// now (true: into the depth texture). The list's first use is the
    /// prelude's to prepare.
    pub(crate) fn twin_use(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        side: u8,
        writes: bool,
    ) -> Option<bool> {
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        match twins.entry(std::sync::Arc::as_ptr(depth) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(crate::twin::Use::new(depth, side, writes));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => o.get_mut().again(side, writes),
        }
    }

    /// The list is one of ours (uploads): what it uses is no use of the
    /// title's.
    pub(crate) fn set_ours(&self) {
        self.ours.store(true, Ordering::Relaxed);
    }

    /// What the list's shaders write from now until the guard goes is
    /// written by a program using title memory (`uses`) or not.
    pub(crate) fn writer_uses_memory(&self, uses: bool) -> impl Drop + '_ {
        struct Reset<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Relaxed);
            }
        }
        self.memory_writer.store(uses, Ordering::Relaxed);
        Reset(&self.memory_writer)
    }

    /// A copy of ours replaced the compute root signature and pipeline.
    pub(crate) fn need_rebind(&self) {
        self.rebind.store(true, Ordering::Relaxed);
    }

    /// Whether the list's compute root signature and pipeline must be set
    /// again (once).
    pub(crate) fn take_rebind(&self) -> bool {
        self.rebind.swap(false, Ordering::Relaxed)
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with; the textures' real
    /// states become those the list ends with.
    fn prelude(&self) -> Option<ID3D12CommandList> {
        // Depth textures and their twins first, from their real states and
        // back: the side each first use needs, made newer.
        let twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let copies = crate::twin::at_submission(twins.values());
        drop(twins);
        // `gate` before `states`, the same order as [`HostList::reset`].
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        // The copies start from the real states, then the list's moves from
        // them; only after both do they become the states the list leaves.
        let need_open = states.values().any(needs_open);
        if copies.is_empty() && !need_open {
            for u in states.values() {
                publish(u);
            }
            return None;
        }
        let (allocator, list) = &self.prelude;
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                for (depth, to_depth) in &copies {
                    let Some(Some(twin)) = depth.twin.get() else {
                        continue;
                    };
                    // From each texture's real state to the copy's and back.
                    let mut back = Vec::new();
                    twin.copy(list, depth, *to_depth, &mut |t, wanted| {
                        let real = t.state.load(Ordering::Relaxed);
                        let wanted = legal_state(&t.texture, wanted);
                        (real != wanted).then(|| {
                            let there = transition(
                                &t.texture,
                                D3D12_RESOURCE_STATES(real as i32),
                                D3D12_RESOURCE_STATES(wanted as i32),
                            );
                            back.push((t.clone(), wanted, real));
                            there
                        })
                    });
                    if !back.is_empty() {
                        let restore: Vec<_> = back.into_iter().map(|(texture, wanted, real)|
                            transition(&texture.texture,
                                D3D12_RESOURCE_STATES(wanted as i32),
                                D3D12_RESOURCE_STATES(real as i32)))
                            .collect();
                        list.ResourceBarrier(&restore);
                    }
                }
                // Build these after the twin copies so diagnostics record
                // barriers in the same order the GPU will execute them.
                let opening: Vec<_> = states.values().flat_map(open_move).collect();
                if !opening.is_empty() {
                    list.ResourceBarrier(&opening);
                }
                list.Close()
            })
        };
        for u in states.values() {
            publish(u);
        }
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    /// As [`HostList::note_op`], for a copy that reads the host GPU range
    /// `(address, bytes)`: the report places the faulting address in it.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        if let Some(stride) = profile_stride() {
            if let Some((heap, _)) = self.stamps() {
                let mut profile = self.profile.lock().unwrap_or_else(|e| e.into_inner());
                profile.before(stride, what, |at| {
                    // SAFETY: an open list and a timestamp slot below STAMPS.
                    unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
                });
            }
            return;
        }
        if dred_on() {
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps()
                && at < STAMPS - 1
            {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            traced(&what);
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list
                    .SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
        }
    }

    /// Notes a texture the list renders to or writes, in a listed frame.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_target(&self, resource: usize, by: &str) {
        let mut targets = self.targets.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = targets.iter_mut().find(|t| t.0 == resource) {
            if by.starts_with("ffx-") {
                existing.1 = by.to_owned();
            }
        } else {
            targets.push((resource, by.to_owned()));
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn binds_target(&self, resource: usize) -> bool {
        self.targets.lock().unwrap_or_else(|e| e.into_inner())
            .iter().any(|(target, _)| *target == resource)
    }

    /// A command actually wrote the tracked lighting target. Keep its place
    /// within this list until the submission order is known.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_lit_write(&self, resource: usize, pipeline: String) {
        let mut writes = self.lit_writes.lock().unwrap_or_else(|e| e.into_inner());
        if writes.len() < 4096 {
            writes.push((resource, pipeline));
        } else if writes.len() == 4096 {
            tracing::warn!(list = self.id, "lighting writer trace reached 4096 commands on one list");
            writes.push((0, String::new()));
        }
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn has_lit_write(&self) -> bool {
        self.lit_writes.lock().unwrap_or_else(|e| e.into_inner())
            .iter().any(|(resource, _)| *resource != 0)
    }

    /// Notes a pipeline the list binds.
    pub(crate) fn used_pipeline(&self, name: &str) {
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copies `len` bytes from `src` to `dst` when the list is submitted.
    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        let mut deferred = self.deferred();
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.refreshed.clear();
    }

    /// Copies `len` bytes of title memory at `src` into the list's upload
    /// memory at `dst` when the list is submitted.
    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len));
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run
    /// (`marked`: the pages they mark).
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
        marked: bool,
    ) {
        let memory = self.memory_writer.load(Ordering::Relaxed);
        self.deferred()
            .copies
            .push(Step::GpuWrite(shadow.clone(), address, len, marked, memory));
    }

    /// Shaders of the list take views from the heap itself
    /// (`crate::bindless`): what the heap's views reach is brought up to
    /// date at submission (once until a title copy intervenes).
    pub(crate) fn defer_heap(&self, viewed: &std::sync::Arc<crate::bindless::Viewed>) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(viewed) as usize, usize::MAX, 0);
        if deferred.refreshed.insert(key) {
            deferred.copies.push(Step::Heap(viewed.clone()));
        }
    }

    /// Shaders of the list read title memory through the page table of
    /// view heap `heap` (`crate::memory::prepare` at submission).
    pub(crate) fn defer_memory(&self, heap: usize) {
        self.deferred().copies.push(Step::Memory(heap));
    }

    /// After the list has run, before its writes go back: `check` says
    /// whether its shaders read title memory with no copy.
    pub(crate) fn defer_missed(&self, check: impl Fn() -> bool + Send + 'static) {
        self.deferred().missed.push(Box::new(check));
    }

    /// Runs `work` once the list has run (each submission), before its
    /// bottom-of-pipe writes.
    pub(crate) fn defer_after<F: Fn() + Send + 'static>(&self, work: F) {
        self.deferred().after.push(AfterWork {
            run: Box::new(work),
            #[cfg(feature = "diagnostics")]
            kind: std::any::type_name::<F>(),
        });
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    /// A memory value the list waits for before it starts.
    pub(crate) fn defer_wait(&self, wait: crate::hold::ValueWait) {
        self.deferred().waits.push(wait);
    }

    /// The memory values the list waits for before it starts.
    pub(crate) fn waits(&self) -> Vec<crate::hold::ValueWait> {
        self.deferred().waits.clone()
    }

    /// Whether nothing has been recorded since the list was made or reset
    /// that a wait recorded now would have to follow: no work, texture
    /// use, bottom-of-pipe write or pipeline (state settings alone carry
    /// over).
    pub(crate) fn is_empty(&self) -> bool {
        let d = self.deferred();
        let deferred_empty = d.copies.is_empty()
            && d.uploads.is_empty()
            && d.writes.is_empty()
            && d.after.is_empty()
            && d.missed.is_empty();
        drop(d);
        deferred_empty
            && self
                .states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            && self
                .twins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            && self
                .pipelines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
    }

    /// A mapped upload buffer of `size` bytes, alive until the list is
    /// reset.
    ///
    /// shortcut: one buffer per copy; pool them once uploads are frequent.
    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.upload_buffer(size)?;
        name(&buffer, || format!("list upload {size:#x}"));
        self.deferred().uploads.push(buffer.clone());
        Some((buffer, data))
    }

    /// A mapped readback buffer of `size` bytes, alive until the list is
    /// reset.
    pub(crate) fn readback(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let (buffer, data) = host.mapped_buffer(size, true)?;
        name(&buffer, || format!("list readback {size:#x}"));
        self.deferred().uploads.push(buffer.clone());
        Some((buffer, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).checked_next_multiple_of(256)?;
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            self.deferred().uploads.push(buffer);
            return Some((gpu, data));
        }
        while let Some(chunk) = s.chunks.get(s.current) {
            if len <= chunk.capacity - s.used {
                break;
            }
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let next = s
                .chunks
                .last()
                .map_or(FIRST_SCRATCH_CHUNK, |c| 2 * c.capacity);
            let capacity = len
                .next_multiple_of(FIRST_SCRATCH_CHUNK)
                .max(next)
                .min(SCRATCH_CHUNK);
            let (buffer, data) = host.upload_buffer(capacity)?;
            name(&buffer, || format!("list scratch {capacity:#x}"));
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            #[cfg(feature = "diagnostics")]
            tracing::debug!(target: "victory_gpu::scratch", list = ?std::ptr::from_ref(self),
                chunk = s.current, requested = len, capacity, "allocated list scratch");
            s.chunks.push(ScratchChunk {
                _buffer: buffer,
                mapped: data as usize,
                gpu,
                capacity,
            });
            s.used = 0;
        }
        let chunk = &s.chunks[s.current];
        let at = s.used;
        let result = (chunk.gpu + at, (chunk.mapped + at as usize) as *mut u8);
        s.used += len;
        Some(result)
    }

    /// The host address of the list's indirect-work budget, zeroed at each
    /// submission of the list from the first use after a reset on.
    pub(crate) fn budget(&self) -> Option<u64> {
        let (buffer, data) = self
            .budget
            .get_or_init(|| {
                let (b, d) = host()?.shared_buffer(256)?;
                Some((b, d as usize))
            })
            .as_ref()?;
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            deferred.budget_zeroed = true;
            let at = *data;
            // SAFETY: the list's own mapped word, written before it runs.
            deferred.copies.push(Step::Host(Box::new(move || unsafe {
                (at as *mut u32).write_volatile(0)
            })));
        }
        // SAFETY: a live buffer.
        Some(unsafe { buffer.GetGPUVirtualAddress() })
    }

    /// The list's argument buffer, at least `bytes` long, common between
    /// its uses (`crate::indirect`).
    pub(crate) fn arguments(&self, bytes: u64) -> Option<ID3D12Resource> {
        let mut arguments = self.arguments.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = arguments
            .as_ref()
            .filter(|b| {
                // SAFETY: `b` is a live resource held by `arguments`.
                unsafe { b.GetDesc() }.Width >= bytes
            })
        {
            return Some(b.clone());
        }
        let (made, _) = host()?.shared_buffer(bytes.next_power_of_two().max(1 << 16))?;
        name(&made, || "indirect arguments".into());
        // Commands recorded earlier may still name the smaller one.
        if let Some(old) = arguments.replace(made.clone()) {
            self.keep(&old);
        }
        Some(made)
    }

    /// Index, vertex and draw-argument buffers for one mesh fallback, at
    /// least `index_bytes` and `vertex_bytes` long. A larger draw replaces
    /// a buffer and keeps the old one until reset, because commands already
    /// recorded still name it.
    pub(crate) fn mesh_slot(&self, index_bytes: u64, vertex_bytes: u64) -> Option<MeshSlot> {
        let index_bytes = index_bytes.max(4);
        let vertex_bytes = vertex_bytes.max(4);
        let mut slot = self.mesh.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            if let Some(gpu) = mesh_checkout(index_bytes, vertex_bytes) {
                *slot = Some(gpu);
            } else {
                let index = default_uav(index_bytes)?;
                let vertex = default_uav(vertex_bytes)?;
                let draws = default_uav(MESH_DRAWS)?;
                name(&index, || "mesh fallback indices".into());
                name(&vertex, || "mesh fallback vertices".into());
                name(&draws, || "mesh fallback draws".into());
                mesh_born(index_bytes, vertex_bytes);
                *slot = Some(MeshGpu {
                    index,
                    vertex,
                    draws,
                    index_bytes,
                    vertex_bytes,
                    index_state: D3D12_RESOURCE_STATE_COMMON,
                    vertex_state: D3D12_RESOURCE_STATE_COMMON,
                    draws_state: D3D12_RESOURCE_STATE_COMMON,
                });
            }
        }
        let gpu = slot.as_mut().unwrap();
        if gpu.index_bytes < index_bytes {
            let index = default_uav(index_bytes)?;
            name(&index, || "mesh fallback indices".into());
            self.keep(&gpu.index);
            MESH_BYTES.fetch_add(index_bytes - gpu.index_bytes, Ordering::Relaxed);
            gpu.index = index;
            gpu.index_bytes = index_bytes;
            gpu.index_state = D3D12_RESOURCE_STATE_COMMON;
        }
        if gpu.vertex_bytes < vertex_bytes {
            let vertex = default_uav(vertex_bytes)?;
            name(&vertex, || "mesh fallback vertices".into());
            self.keep(&gpu.vertex);
            MESH_BYTES.fetch_add(vertex_bytes - gpu.vertex_bytes, Ordering::Relaxed);
            gpu.vertex = vertex;
            gpu.vertex_bytes = vertex_bytes;
            gpu.vertex_state = D3D12_RESOURCE_STATE_COMMON;
        }
        let gpu = slot.as_ref().unwrap();
        Some(MeshSlot {
            index: gpu.index.clone(),
            vertex: gpu.vertex.clone(),
            draws: gpu.draws.clone(),
            index_bytes: gpu.index_bytes,
            index_state: gpu.index_state,
            vertex_state: gpu.vertex_state,
            draws_state: gpu.draws_state,
        })
    }

    /// Records the states [`mesh_slot`] left these buffers in.
    pub(crate) fn mesh_remember(
        &self,
        index: &ID3D12Resource,
        index_state: D3D12_RESOURCE_STATES,
        vertex: &ID3D12Resource,
        vertex_state: D3D12_RESOURCE_STATES,
        draws: &ID3D12Resource,
        draws_state: D3D12_RESOURCE_STATES,
    ) {
        let mut slot = self.mesh.lock().unwrap_or_else(|e| e.into_inner());
        let Some(gpu) = slot.as_mut() else { return };
        if gpu.index.as_raw() == index.as_raw() {
            gpu.index_state = index_state;
        }
        if gpu.vertex.as_raw() == vertex.as_raw() {
            gpu.vertex_state = vertex_state;
        }
        if gpu.draws.as_raw() == draws.as_raw() {
            gpu.draws_state = draws_state;
        }
    }

    /// Keeps `object` alive until the list is reset.
    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(object);
        }
    }

    /// Whether this is the first time since the reset that the list asks
    /// about `key` (a texture to bring up to date).
    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Run(Box::new(work)));
        self.deferred().refreshed.clear();
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    pub(crate) fn defer_host(&self, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work)));
    }

    /// Brings a texture's staging buffer and host texture `copy` up to date
    /// when the list is submitted (`crate::bindless::uploads`), keeping
    /// both alive until the list is reset.
    pub(crate) fn defer_texture(
        &self,
        staging: std::sync::Arc<crate::texture::Staging>,
        copy: std::sync::Arc<crate::resource::HostTexture>,
    ) {
        self.deferred().copies.push(Step::Texture(staging, copy));
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges).
    fn run_deferred(&self, batch: &mut Batch) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        for step in &self.deferred().copies {
            let started = std::time::Instant::now();
            #[cfg(feature = "diagnostics")]
            let _timed = crate::watch::Timed(
                started,
                match step {
                    Step::Refresh(..) => C::StepRefreshUs,
                    Step::Copy(..) | Step::Fill(..) => C::StepCopyUs,
                    Step::Run(_) | Step::Host(_) => C::StepWorkUs,
                    Step::GpuWrite(..) => C::StepGpuWriteUs,
                    Step::Texture(..) => C::StepTextureUs,
                    Step::Heap(_) => C::StepHeapUs,
                    Step::Memory(_) => C::StepMemoryUs,
                },
            );
            let copied = match step {
                &Step::Copy(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    batch.title_written();
                    copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    len
                }
                &Step::Fill(dst, src, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Copies, 1);
                        add(C::CopyBytes, len as u64);
                    }
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    len
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Refreshes, 1);
                        add(C::RefreshBytes, *len as u64);
                    }
                    batch.add(shadow, *address, *len);
                    continue;
                }
                Step::Run(work) | Step::Host(work) => {
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    work();
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::GpuWrite(shadow, address, len, marked, _) => {
                    // The shaders start from the title's data, which a
                    // snapshot must hold (marked ranges keep none).
                    if !marked {
                        batch.flush();
                    }
                    shadow.writing(*address, *len, *marked);
                    continue;
                }
                // Brought up to date before the submission's lists were made.
                Step::Texture(..) => continue,
                Step::Heap(viewed) => {
                    crate::bindless::refresh(viewed, |s, a, l| batch.add(s, a, l));
                    continue;
                }
                &Step::Memory(heap) => {
                    crate::memory::prepare(heap, |s, a, l| batch.add(s, a, l));
                    continue;
                }
            };
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
        }
    }
}

/// A submission's refreshes not done yet, merged per buffer copy into the
/// range that covers them: done before a step that writes title memory or
/// gives a range to shaders, and before the lists run.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
///
/// A copy's range brought up to date is not copied again in the submission
/// until a step writes title memory: every list of a world submission using
/// title memory asks for every copy whole. What the title's threads write
/// meanwhile is for later submissions (on the console it would race the
/// GPU).
#[derive(Default)]
struct Batch {
    refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    /// Where each copy's refresh is in `refreshes`, by copy.
    pending: std::collections::HashMap<usize, usize>,
    /// The range of each copy brought up to date since the last title
    /// write, by copy (held, so that its address names no other).
    current:
        std::collections::HashMap<usize, (std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
}

impl Batch {
    fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        // Nothing to copy: a world submission asks for every copy its heaps'
        // views and page tables reach (~9,000), a few of them written.
        if shadow.clean() {
            return;
        }
        let (key, end) = (std::sync::Arc::as_ptr(shadow) as usize, address + len);
        if self
            .current
            .get(&key)
            .is_some_and(|&(_, a, b)| a <= address && end <= b)
        {
            return;
        }
        match self.pending.get(&key) {
            Some(&i) => {
                let (_, from, to) = &mut self.refreshes[i];
                (*from, *to) = ((*from).min(address), (*to).max(end));
            }
            None => {
                self.pending.insert(key, self.refreshes.len());
                self.refreshes.push((shadow.clone(), address, end));
            }
        }
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    fn title_written(&mut self) {
        self.flush();
        self.current.clear();
    }

    /// Does the refreshes; returns the bytes copied.
    fn flush(&mut self) -> usize {
        #[cfg(feature = "diagnostics")]
        let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::FlushUs);
        self.pending.clear();
        let mut total = 0;
        for (shadow, from, to) in self.refreshes.drain(..) {
            let started = std::time::Instant::now();
            let copied = shadow.update(from, to - from);
            let us = started.elapsed().as_micros() as u64;
            #[cfg(feature = "diagnostics")]
            crate::copystat::range(from, to - from, us, 0);
            PREPARING[0].fetch_add(us, Ordering::Relaxed);
            PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
            total += copied;
            // Kept: the union with what was current if they touch, else
            // the larger.
            let key = std::sync::Arc::as_ptr(&shadow) as usize;
            let (_, a, b) = self.current.entry(key).or_insert((shadow, from, to));
            if from <= *b && *a <= to {
                (*a, *b) = ((*a).min(from), (*b).max(to));
            } else if to - from > *b - *a {
                (*a, *b) = (from, to);
            }
        }
        total
    }
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped takes nothing from its
/// first unmapped page on.
///
/// The copy runs in the kernel (`ReadProcessMemory`), which reports an
/// unmapped page of the source, or of the destination it writes, instead of
/// faulting. Asking `VirtualQuery` first cost about 2 ms per copy in the
/// title's big pools (it walks the page tables to the region's end), which
/// made copies run at 100-200 MB/s; a title destination is asked only once
/// its copy has failed (asked before each, the world's 80 copies per frame
/// cost 60 ms).
pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    // The kernel writes the destination: it must not be guarded meanwhile.
    let _writing = title_dst.then(|| crate::watch::writing(dst as usize, len));
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let whole = read(src, dst, len);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::MoveUs,
        moved.elapsed().as_micros() as u64,
    );
    if whole {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::failed_read(src as usize, len, title_dst);
    // The copy stopped at an unmapped page. One of a title destination ends
    // it: the retries below write the destination themselves (zeros for
    // what cannot be read), and would fault there.
    if title_dst && !crate::texture::committed(dst, len) {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    #[cfg(feature = "diagnostics")]
    let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::FallbackUs);
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    // SAFETY: inside the destination, which is mapped.
                    unsafe { to.write_bytes(0, n) };
                }
            });
        }
    });
}

/// Reads title memory at `src` into `dst`; false if a page of it is not
/// readable.
pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

/// Copies `len` bytes from `src` to `dst` in the kernel: false if a source
/// page is not readable or a destination page not writable (the bytes
/// before that page may have been copied).
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut done = 0;
    // SAFETY: the destination is `len` bytes the caller may write, host
    // memory or title memory; the kernel checks both ranges, and a page it
    // cannot read or write fails the call instead of faulting.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
///
/// Through [`read`]: the kernel refuses an unmapped destination page as it
/// does a source page. `WriteProcessMemory` makes the same copy, but asks
/// for the destination's region first, at `VirtualQuery`'s cost (see
/// [`copy_mapped`]): 0.35 ms for each piece of the world's copy-back, 39 ms
/// per frame.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    let write = |at: usize, n: usize| read(src.wrapping_add(at - dst as usize), at as *mut u8, n);
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                write(at, n);
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

#[cfg(test)]
mod title_copies {
    use super::{copy_mapped, write_mapped};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect,
    };

    const BLOCK: usize = 64 << 10;

    /// Three reserved blocks, the first and the last committed and filled
    /// with `fill`: title memory with an unmapped block in it.
    fn holed(fill: u8) -> *mut u8 {
        // SAFETY: a fresh reservation; the committed blocks are ours to fill.
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 3 * BLOCK, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            for block in [0, 2] {
                let at = base.add(block * BLOCK);
                assert!(!VirtualAlloc(at.cast(), BLOCK, MEM_COMMIT, PAGE_READWRITE).is_null());
                at.write_bytes(fill, BLOCK);
            }
            base
        }
    }

    fn block<'a>(base: *const u8, n: usize) -> &'a [u8] {
        // SAFETY: tests ask for committed blocks only.
        unsafe { std::slice::from_raw_parts(base.add(n * BLOCK), BLOCK) }
    }

    fn release(base: *mut u8) {
        // SAFETY: a reservation of `holed`.
        assert_ne!(unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) }, 0);
    }

    fn pattern() -> Vec<u8> {
        (0..3 * BLOCK).map(|i| (i % 251) as u8 + 1).collect()
    }

    #[test]
    fn title_copies_reach_mapped_destinations_and_stop_at_unmapped_pages() {
        let from = pattern();
        // All mapped: copied, through a destination that was never asked.
        let to = holed(0xee);
        copy_mapped(to, from.as_ptr(), BLOCK, true);
        assert_eq!(block(to, 0), &from[..BLOCK]);
        release(to);
        // An unmapped block in the destination: nothing faults, and nothing
        // arrives from that block on.
        let to = holed(0xee);
        copy_mapped(to, from.as_ptr(), 3 * BLOCK, true);
        assert!(block(to, 2).iter().all(|&b| b == 0xee));
        release(to);
        // An unmapped block in the source reads as zeros.
        let holes = holed(7);
        let mut to = vec![0xeeu8; 3 * BLOCK];
        copy_mapped(to.as_mut_ptr(), holes, 3 * BLOCK, true);
        assert!(to[..BLOCK].iter().all(|&b| b == 7));
        assert!(to[BLOCK..2 * BLOCK].iter().all(|&b| b == 0));
        assert!(to[2 * BLOCK..].iter().all(|&b| b == 7));
        release(holes);
    }

    #[test]
    fn copy_back_writes_mapped_pages_only() {
        let from = pattern();
        let to = holed(0xee);
        write_mapped(to, from.as_ptr(), 3 * BLOCK);
        assert_eq!(block(to, 0), &from[..BLOCK]);
        assert_eq!(block(to, 2), &from[2 * BLOCK..]);
        release(to);
        // Neither aligned nor whole: the bytes around the range stay.
        let to = holed(0xee);
        // SAFETY: inside the first block.
        write_mapped(unsafe { to.add(100) }, from.as_ptr(), BLOCK - 300);
        let first = block(to, 0);
        assert_eq!(first[100..BLOCK - 200], from[..BLOCK - 300]);
        let around = first[..100].iter().chain(&first[BLOCK - 200..]);
        assert!(around.into_iter().all(|&b| b == 0xee));
        // A page that is mapped but not writable is not written either
        // (guarded title blocks are opened before they are copied back).
        let mut old = 0;
        // SAFETY: a committed block of ours.
        let protected =
            unsafe { VirtualProtect(to.add(2 * BLOCK).cast(), BLOCK, PAGE_READONLY, &mut old) };
        assert_ne!(protected, 0);
        // SAFETY: the third block.
        write_mapped(unsafe { to.add(2 * BLOCK) }, from.as_ptr(), BLOCK);
        assert!(block(to, 2).iter().all(|&b| b == 0xee));
        release(to);
    }
}

impl Host {
    /// A buffer in the upload heap, mapped for writing.
    /// A GPU-only buffer of `size` bytes in the copy-destination state.
    /// Small ones share default heaps ([`crate::placement::place_gpu_buffer`]).
    pub(crate) fn default_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        gpu_buffer(&self.device, &desc, D3D12_RESOURCE_STATE_COPY_DEST)
    }

    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        self.mapped_buffer(size, false)
    }

    /// A mapped buffer of `size` bytes: upload memory, or (`readback`)
    /// readback memory in the copy-destination state.
    fn mapped_buffer(&self, size: u64, readback: bool) -> Option<(ID3D12Resource, *mut u8)> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: if readback {
                D3D12_HEAP_TYPE_READBACK
            } else {
                D3D12_HEAP_TYPE_UPLOAD
            },
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let state = if readback {
            D3D12_RESOURCE_STATE_COPY_DEST
        } else {
            D3D12_RESOURCE_STATE_GENERIC_READ
        };
        // SAFETY: a plain query.
        let info = unsafe { self.device.GetResourceAllocationInfo(0, &[desc]) };
        if info.SizeInBytes != u64::MAX
            && let Some((placed, place)) = crate::placement::place_mapped(
                &self.device,
                info.SizeInBytes,
                info.Alignment,
                readback,
            )
        {
            let mut buffer: Option<ID3D12Resource> = None;
            // SAFETY: a range of the heap for this buffer; our out pointer.
            let created = unsafe {
                self.device.CreatePlacedResource(
                    &placed,
                    place.offset(),
                    &desc,
                    state,
                    None,
                    &mut buffer,
                )
            };
            if let (Ok(()), Some(buffer)) = (created, buffer) {
                let mut data = std::ptr::null_mut();
                // SAFETY: mapping a live upload or readback buffer.
                if unsafe { buffer.Map(0, None, Some(&mut data)) }.is_ok() {
                    crate::placement::note_mapped(buffer.clone(), place, readback);
                    return Some((buffer, data.cast()));
                }
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    readback,
                    "a mapped buffer was not placed; that one is committed"
                )
            });
            crate::placement::reject_mapped(place, readback);
        }
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    state,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, readback, "no mapped buffer");
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's.
fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

/// Limit only the host's final multi-plane composite. The title's scene and
/// UI textures keep their original extents and all title coordinates remain
/// valid; the compositor samples both into a smaller swap-chain buffer.
fn present_max_size() -> Option<(u32, u32)> {
    static SIZE: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        let value = std::env::var("VICTORY_PRESENT_MAX").ok()?;
        let parsed = value.split_once(['x', 'X']).and_then(|(w, h)| {
            Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))
        });
        match parsed {
            Some((w, h)) if (640..=7680).contains(&w) && (360..=4320).contains(&h) => {
                tracing::info!(width = w, height = h, "host composite output cap enabled");
                Some((w, h))
            }
            _ => {
                tracing::warn!(value, "invalid VICTORY_PRESENT_MAX; expected WIDTHxHEIGHT");
                None
            }
        }
    })
}

fn fit_present_size(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    if u64::from(width) * u64::from(max_height) > u64::from(height) * u64::from(max_width) {
        (max_width, (u64::from(height) * u64::from(max_width) / u64::from(width)).max(1) as u32)
    } else {
        ((u64::from(width) * u64::from(max_height) / u64::from(height)).max(1) as u32, max_height)
    }
}

const BUFFERS: u32 = 2;
const MAX_PLANES: u32 = 8;

/// Built-in fullscreen-triangle compositor for multi-plane `PresentX` calls
/// (e.g. Series S presenting a 1080p scene plane with a 4K UI overlay plane).
struct Compositor {
    format: DXGI_FORMAT,
    root: ID3D12RootSignature,
    opaque: ID3D12PipelineState,
    overlay: ID3D12PipelineState,
    rtv_heap: ID3D12DescriptorHeap,
    srv_heap: ID3D12DescriptorHeap,
    srv_stride: u32,
}

impl Compositor {
    fn new(device: &ID3D12Device, format: DXGI_FORMAT) -> windows::core::Result<Self> {
        const SOURCE: &str = r#"
            Texture2D tex : register(t0);
            SamplerState smp : register(s0);
            void vs(uint id : SV_VertexID, out float4 pos : SV_Position, out float2 uv : TEXCOORD) {
                uv = float2((id << 1) & 2, id & 2);
                pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
            }
            float4 ps(float4 pos : SV_Position, float2 uv : TEXCOORD) : SV_Target {
                return tex.Sample(smp, uv);
            }
        "#;
        let compile = |entry: PCSTR, target: PCSTR| -> windows::core::Result<ID3DBlob> {
            let mut code: Option<ID3DBlob> = None;
            let mut errors: Option<ID3DBlob> = None;
            // SAFETY: static source and entry/target strings; out pointers ours.
            let res = unsafe {
                D3DCompile(
                    SOURCE.as_ptr().cast(),
                    SOURCE.len(),
                    s!("present_composite"),
                    None,
                    None,
                    entry,
                    target,
                    0,
                    0,
                    &mut code,
                    Some(&mut errors),
                )
            };
            if let Err(e) = res {
                if let Some(errors) = errors {
                    // SAFETY: the blob's own bytes.
                    let text = unsafe {
                        std::slice::from_raw_parts(
                            errors.GetBufferPointer().cast::<u8>(),
                            errors.GetBufferSize(),
                        )
                    };
                    tracing::warn!("present compositor: {}", String::from_utf8_lossy(text));
                }
                return Err(e);
            }
            code.ok_or_else(windows::core::Error::empty)
        };
        let vs = compile(s!("vs"), s!("vs_5_1"))?;
        let ps = compile(s!("ps"), s!("ps_5_1"))?;
        // SAFETY: compiled shader blobs live through PSO creation.
        unsafe {
            let vs_bytes =
                std::slice::from_raw_parts(vs.GetBufferPointer().cast::<u8>(), vs.GetBufferSize());
            let ps_bytes =
                std::slice::from_raw_parts(ps.GetBufferPointer().cast::<u8>(), ps.GetBufferSize());
            // Wine's D3DCompile does not embed a root signature in the shader blobs.
            let range = D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
                ..Default::default()
            };
            let parameter = D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &range,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            };
            let sampler = D3D12_STATIC_SAMPLER_DESC {
                Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                MaxAnisotropy: 16,
                ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
                BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE,
                MaxLOD: f32::MAX,
                ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
                ..Default::default()
            };
            let desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: 1,
                pParameters: &parameter,
                NumStaticSamplers: 1,
                pStaticSamplers: &sampler,
                ..Default::default()
            };
            let mut signature = None;
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut signature, None)?;
            let signature = signature.ok_or_else(windows::core::Error::empty)?;
            let signature = std::slice::from_raw_parts(
                signature.GetBufferPointer().cast::<u8>(),
                signature.GetBufferSize(),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, signature)?;
            let make_pso = |blend: bool| -> windows::core::Result<ID3D12PipelineState> {
                let mut rt_blend = D3D12_RENDER_TARGET_BLEND_DESC {
                    RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
                    ..Default::default()
                };
                if blend {
                    rt_blend.BlendEnable = true.into();
                    rt_blend.SrcBlend = D3D12_BLEND_ONE;
                    rt_blend.DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
                    rt_blend.BlendOp = D3D12_BLEND_OP_ADD;
                    rt_blend.SrcBlendAlpha = D3D12_BLEND_ONE;
                    rt_blend.DestBlendAlpha = D3D12_BLEND_INV_SRC_ALPHA;
                    rt_blend.BlendOpAlpha = D3D12_BLEND_OP_ADD;
                }
                let mut blend_desc = D3D12_BLEND_DESC::default();
                blend_desc.RenderTarget[0] = rt_blend;
                let mut rtv_formats = [DXGI_FORMAT_UNKNOWN; 8];
                rtv_formats[0] = format;
                let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::ManuallyDrop::new(Some(root.clone())),
                    VS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: vs_bytes.as_ptr().cast(),
                        BytecodeLength: vs_bytes.len(),
                    },
                    PS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: ps_bytes.as_ptr().cast(),
                        BytecodeLength: ps_bytes.len(),
                    },
                    BlendState: blend_desc,
                    SampleMask: u32::MAX,
                    RasterizerState: D3D12_RASTERIZER_DESC {
                        FillMode: D3D12_FILL_MODE_SOLID,
                        CullMode: D3D12_CULL_MODE_NONE,
                        DepthClipEnable: false.into(),
                        ..Default::default()
                    },
                    PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                    NumRenderTargets: 1,
                    RTVFormats: rtv_formats,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                };
                let pso = device.CreateGraphicsPipelineState(&desc);
                let _ = std::mem::ManuallyDrop::into_inner(desc.pRootSignature);
                pso
            };
            let opaque = make_pso(false)?;
            let overlay = make_pso(true)?;
            let rtv_heap: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })?;
            let srv_heap: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: MAX_PLANES,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })?;
            let srv_stride =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            Ok(Compositor {
                format,
                root,
                opaque,
                overlay,
                rtv_heap,
                srv_heap,
                srv_stride,
            })
        }
    }
}

#[derive(Default)]
struct Presentation {
    presenter: Option<Presenter>,
    retry: SwapChainRetry,
}

#[derive(Default)]
struct SwapChainRetry {
    next_attempt: Option<Instant>,
}

impl SwapChainRetry {
    fn ready(&self, now: Instant) -> bool {
        self.next_attempt.is_none_or(|next| now >= next)
    }

    /// A desktop can become presentable again. Keep retrying while limiting
    /// the DXGI calls and warnings during the outage.
    fn attempt<T, E>(
        &mut self,
        now: Instant,
        operation: impl FnOnce() -> Result<T, E>,
    ) -> Option<Result<T, E>> {
        if !self.ready(now) {
            return None;
        }
        let result = operation();
        self.next_attempt = result.is_err().then(|| now + Duration::from_secs(1));
        Some(result)
    }
}

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    hwnd: HWND,
    meter: Meter,
    compositor: Option<Compositor>,
}

#[cfg(feature = "diagnostics")]
struct PresentPhase {
    entry: Instant,
    lock_requested: Instant,
    lock_acquired: Instant,
    present_started: Instant,
    present_finished: Instant,
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds.
struct Meter {
    since: std::time::Instant,
    frames: u32,
    report_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
    #[cfg(feature = "diagnostics")]
    last_present_end: Option<Instant>,
    /// Between-present, pre-lock, presenter-lock, presenter-body, Present call,
    /// frame meter and diagnostic logging.
    #[cfg(feature = "diagnostics")]
    present_phase_us: [u64; 6],
    #[cfg(feature = "diagnostics")]
    between_present_frames: u64,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            report_frames: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            caption: None,
            #[cfg(feature = "diagnostics")]
            last_present_end: None,
            #[cfg(feature = "diagnostics")]
            present_phase_us: [0; 6],
            #[cfg(feature = "diagnostics")]
            between_present_frames: 0,
        }
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND, #[cfg(feature = "diagnostics")] phase: PresentPhase) {
        #[cfg(feature = "diagnostics")]
        {
            if let Some(last) = self.last_present_end {
                self.present_phase_us[0] += phase.entry.duration_since(last).as_micros() as u64;
                self.between_present_frames += 1;
            }
            self.present_phase_us[1] +=
                phase.lock_requested.duration_since(phase.entry).as_micros() as u64;
            self.present_phase_us[2] += phase
                .lock_acquired
                .duration_since(phase.lock_requested)
                .as_micros() as u64;
            self.present_phase_us[3] += phase
                .present_started
                .duration_since(phase.lock_acquired)
                .as_micros() as u64;
            self.present_phase_us[4] += phase
                .present_finished
                .duration_since(phase.present_started)
                .as_micros() as u64;
        }
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        self.frames += 1;
        crate::memory::log_frame();
        #[cfg(feature = "diagnostics")]
        crate::watch::frame_done();
        self.report_frames += 1;
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        // Of a frame, ms preparing submissions and ms waiting for the GPU.
        let [cpu, gpu] =
            [0, 1].map(|i| SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] =
            [0, 1].map(|i| COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames);
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            let report_frames = std::mem::take(&mut self.report_frames) as f64;
            #[cfg(feature = "diagnostics")]
            {
                let [
                    between,
                    pre_lock,
                    presenter_lock,
                    presenter_body,
                    dxgi_present,
                    meter,
                ] = std::mem::take(&mut self.present_phase_us).map(|us| us as f64 / 1000.0);
                let between_frames = std::mem::take(&mut self.between_present_frames).max(1) as f64;
                let [
                    submission_lock,
                    execute_body,
                    before_copyback,
                    after_copyback,
                ] = std::array::from_fn(|i| {
                    EXECUTE_PHASE_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0
                });
                tracing::info!(
                    "frame phases over {report_frames:.0} presents, ms/present: between previous present end and next present entry {:.1}; \
                     before presenter lock {:.1}, presenter lock wait {:.1}, presenter body before DXGI Present {:.1}, \
                     DXGI Present call {:.1}, frame meter and logging {:.1}; \
                     host execute submission lock wait {:.1}, execute body after lock {:.1}, \
                     gap before copy-back {:.1}, work after copy-back {:.1}",
                    between / between_frames,
                    pre_lock / report_frames,
                    presenter_lock / report_frames,
                    presenter_body / report_frames,
                    dxgi_present / report_frames,
                    meter / report_frames,
                    submission_lock / report_frames,
                    execute_body / report_frames,
                    before_copyback / report_frames,
                    after_copyback / report_frames,
                );
                let copyback: [f64; 6] = std::array::from_fn(|i| {
                    COPYBACK_PHASE_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / report_frames
                });
                tracing::info!(
                    "copy-back phases ms/frame: missed/range setup {:.1}, marked pages {:.1}, deferred views {:.1}, explicit ranges {:.1}, capture hooks {:.1}, deferred callbacks {:.1}",
                    copyback[0], copyback[1], copyback[2], copyback[3], copyback[4], copyback[5],
                );
                log_after_times(report_frames);
            }
            let dropped = crate::draw::take_dropped() as f64 / report_frames;
            #[cfg(feature = "diagnostics")]
            let direct_drop_accounting = crate::draw::take_direct_drop_accounting();
            #[cfg(feature = "diagnostics")]
            let indirect_drop_accounting = crate::draw::take_indirect_drop_accounting();
            let indirect = crate::list::take_indirect() as f64 / report_frames;
            let root_views = crate::draw::take_root_view_slots() as f64 / report_frames;
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect; {root_views:.0} root-view slots/frame",
                making = crate::pipeline::making()
            );
            let (textures, bytes, used, shared, saved, replaced) =
                crate::resource::texture_counts();
            let (heaps, heap_used, heap_free) = crate::placement::stats();
            let (buf_heaps, buf_used, buf_free) = crate::placement::buffer_stats();
            tracing::info!(
                "host textures: {textures} ({:.0} MB, {:.0} MB of them used by lists), title textures sharing one {shared} (saving {:.0} MB), replacing one {replaced}; \
                 heaps of textures {heaps} ({:.0} MB used, {:.0} MB free); \
                 write-back heaps {buf_heaps} ({:.0} MB used, {:.0} MB free); \
                 copies between textures of one memory {:.1}/frame, depth twin copies {:.1}/frame",
                bytes as f64 / 1e6,
                used as f64 / 1e6,
                saved as f64 / 1e6,
                heap_used as f64 / 1e6,
                heap_free as f64 / 1e6,
                buf_used as f64 / 1e6,
                buf_free as f64 / 1e6,
                crate::texture::ALIAS_COPIES.swap(0, Ordering::Relaxed) as f64 / report_frames,
                crate::twin::COPIES.swap(0, Ordering::Relaxed) as f64 / report_frames,
            );
            #[cfg(feature = "diagnostics")]
            {
                if let Some([(local, budget), (system, system_budget)]) = video_memory() {
                    let (commit, working_set) = process_memory();
                    tracing::info!(
                        "video memory: {local:.0} MB of a {budget:.0} MB budget on the card, \
                         {system:.0} MB of {system_budget:.0} MB in system memory; process commit {:.0} MB, \
                         working set {:.0} MB; Rust heap {:.0} MB; placed textures made {:.0} MB; list upload memory {} blocks {:.0} MB",
                        commit as f64 / 1e6,
                        working_set as f64 / 1e6,
                        crate::copystat::HEAP_BYTES.load(Ordering::Relaxed) as f64 / 1e6,
                        crate::resource::PLACED_BYTES.load(Ordering::Relaxed) as f64 / 1e6,
                        crate::list::UPLOADS[0].load(Ordering::Relaxed),
                        crate::list::UPLOADS[1].load(Ordering::Relaxed) as f64 / 1e6,
                    );
                    let [given, given_b, remade, soon] =
                        [0, 1, 2, 3].map(|i| crate::resource::EVICTIONS[i].load(Ordering::Relaxed));
                    let (mesh_sets, mesh_bytes, mesh_pooled) = mesh_stats();
                    let (gpu_heaps, gpu_used, gpu_free) = crate::placement::gpu_buffer_stats();
                    let [(up_heaps, up_used, up_free), (rb_heaps, rb_used, rb_free)] =
                        crate::placement::mapped_stats();
                    tracing::info!(
                        "given up {given} host textures ({:.0} MB), remade {remade} ({soon} within 10 frames); \
                         mesh buffers {mesh_sets} ({:.0} MB, {mesh_pooled} pooled); \
                         gpu-buffer heaps {gpu_heaps} ({:.0} MB used, {:.0} MB free); \
                         upload heaps {up_heaps} ({:.0} MB used, {:.0} MB free); \
                         readback heaps {rb_heaps} ({:.0} MB used, {:.0} MB free)",
                        given_b as f64 / 1e6,
                        mesh_bytes as f64 / 1e6,
                        gpu_used as f64 / 1e6,
                        gpu_free as f64 / 1e6,
                        up_used as f64 / 1e6,
                        up_free as f64 / 1e6,
                        rb_used as f64 / 1e6,
                        rb_free as f64 / 1e6,
                    );
                }
                let [gpu, title] =
                    crate::resource::texture_census().map(|r| r.map(|b| b as f64 / 1e6));
                tracing::info!(
                    "host texture MB (used by lists, viewed by heap slots only, neither): GPU-written {:.0} {:.0} {:.0}, \
                     from title memory {:.0} {:.0} {:.0}",
                    gpu[0],
                    gpu[1],
                    gpu[2],
                    title[0],
                    title[1],
                    title[2]
                );
                crate::resource::log_heap_texture_shapes();
                crate::resource::log_gpu_written_shapes();
                crate::resource::log_allocation_pressure();
                crate::list::log_skipped_streamed_copies();
                crate::descriptor::log_kept_census();
                // Window caption updates leave the report counters intact.
                let drops = crate::pipeline::take_drops();
                let reason_events: u64 = drops.iter().map(|(_, count)| *count).sum();
                tracing::info!(
                    "drop accounting: regular_failed={} regular_ray_disabled={} \
                     indirect_failed={} indirect_ray_disabled={} reason_events={} \
                     reason_categories={} reason_categories_omitted={}",
                    direct_drop_accounting.0,
                    direct_drop_accounting.1,
                    indirect_drop_accounting.0,
                    indirect_drop_accounting.1,
                    reason_events,
                    drops.len(),
                    drops.len().saturating_sub(40),
                );
                for ((kind, title, requested, host, outcome), count) in crate::descriptor::host::take_view_coverage() {
                    tracing::info!(kind, title, requested, host, outcome, count, "texture view coverage");
                }
                crate::frame::log();
                log_gpu_times(report_frames);
                log_gpu_profile(report_frames);
                crate::copystat::log(report_frames);
                crate::bindless::log(report_frames);
                crate::shadow::log_views(report_frames);
                let [n, bytes] =
                    [0, 1].map(|i| crate::list::READ_BACKS[i].swap(0, Ordering::Relaxed));
                if n > 0 {
                    tracing::info!(
                        "texture read backs: {:.1}/frame, {:.1} KB/frame",
                        n as f64 / report_frames,
                        bytes as f64 / report_frames / 1e3
                    );
                }
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / report_frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        let text = format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame");
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows one or more `planes` (each a live host texture and its current
    /// host state, kept in that state), compositing planes 1.. on top of
    /// plane 0 when multiple planes are presented, as frame `frame` (saved if
    /// captures are on). `None` if nothing was shown; else, in diagnostics
    /// builds, whether the frame was black.
    pub(crate) fn present(&self, planes: &[(&ID3D12Resource, u32)], frame: u64) -> Option<bool> {
        #[cfg(feature = "diagnostics")]
        if !planes.is_empty() {
            static FIRST: std::sync::Once = std::sync::Once::new();
            FIRST.call_once(|| {
                let shapes: Vec<_> = planes.iter().enumerate().map(|(i, (tex, _))| {
                    // SAFETY: PresentX passed a live resource for this call.
                    let d = unsafe { tex.GetDesc() };
                    (i, d.Width, d.Height, d.Format.0)
                }).collect();
                tracing::info!(?shapes, "first PresentX plane sizes and formats");
            });
        }
        self.present_with_compositor(
            planes,
            frame,
            Compositor::new,
            #[cfg(all(test, feature = "diagnostics"))]
            |_, _| {},
        )
    }

    fn present_with_compositor(
        &self,
        planes: &[(&ID3D12Resource, u32)],
        frame: u64,
        create_compositor: impl FnOnce(&ID3D12Device, DXGI_FORMAT) -> windows::core::Result<Compositor>,
        #[cfg(all(test, feature = "diagnostics"))] inspect: impl FnOnce(
            &ID3D12Resource,
            &D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        ),
    ) -> Option<bool> {
        #[cfg(feature = "diagnostics")]
        let present_entry = Instant::now();
        let &(primary, _) = planes.first()?;
        // SAFETY: live resources.
        let primary_desc = unsafe { primary.GetDesc() };
        let Some(format) = swap_format(primary_desc.Format) else {
            tracing::warn!(format = primary_desc.Format.0, "cannot present this format");
            return None;
        };
        #[cfg(feature = "diagnostics")]
        let presenter_lock_requested = Instant::now();
        let mut presentation = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(feature = "diagnostics")]
        let presenter_lock_acquired = Instant::now();
        let Presentation { presenter, retry } = &mut *presentation;
        if presenter.is_none() && !retry.ready(Instant::now()) {
            return None;
        }
        let cached_format = presenter
            .as_ref()
            .and_then(|p| p.compositor.as_ref())
            .map(|c| c.format);
        let mut new_compositor = None;
        let planes = if planes.len() > 1 && cached_format != Some(format) {
            match create_compositor(&self.device, format) {
                Ok(c) => {
                    new_compositor = Some(c);
                    planes
                }
                Err(e) => {
                    tracing::warn!(error = %e, "presenting the primary plane after compositor initialization failed");
                    &planes[..1]
                }
            }
        } else {
            planes
        };
        let (mut width, mut height) = (primary_desc.Width as u32, primary_desc.Height);
        for &(tex, _) in &planes[1..] {
            // SAFETY: live resource.
            let d = unsafe { tex.GetDesc() };
            width = width.max(d.Width as u32);
            height = height.max(d.Height);
        }
        if planes.len() > 1
            && let Some((max_width, max_height)) = present_max_size()
        {
            let source = (width, height);
            (width, height) = fit_present_size(width, height, max_width, max_height);
            #[cfg(feature = "diagnostics")]
            {
                static LOGGED: std::sync::Once = std::sync::Once::new();
                LOGGED.call_once(|| tracing::info!(?source, output = ?(width, height),
                    "scaling title planes into capped host composite"));
            }
            #[cfg(not(feature = "diagnostics"))]
            let _ = source;
        }
        if presenter.is_none() {
            *presenter = match retry
                .attempt(Instant::now(), || self.presenter(width, height, format))?
            {
                Ok(p) => Some(p),
                Err((step, e)) => {
                    tracing::warn!(error = %e, step, width, height, format = format.0, "no swap chain");
                    return None;
                }
            };
        }
        let p = presenter.as_mut()?;
        if let Some(c) = new_compositor {
            p.compositor = Some(c);
        }
        if (p.width, p.height, p.format) != (width, height, format) {
            let resized = retry.attempt(Instant::now(), || {
                // SAFETY: no back buffer is held; every submission was waited for.
                unsafe {
                    p.swap_chain.ResizeBuffers(
                        BUFFERS,
                        width,
                        height,
                        format,
                        DXGI_SWAP_CHAIN_FLAG(0),
                    )
                }
            })?;
            match resized {
                Ok(()) => (p.width, p.height, p.format) = (width, height, format),
                Err(e) => {
                    tracing::warn!(error = %e, width, height, format = format.0, "swap chain not resized");
                    return None;
                }
            }
        }
        // SAFETY: our swap chain, list and live source textures.
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            p.copy.reset();
            let list = &p.copy.list;
            let (capture, probe) = if planes.len() == 1 {
                let (texture, state) = planes[0];
                let state = D3D12_RESOURCE_STATES(state as i32);
                let mut before = Vec::with_capacity(2);
                if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    before.push(transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE));
                }
                before.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ));
                list.ResourceBarrier(&before);
                list.CopyResource(&back, texture);
                #[cfg(all(test, feature = "diagnostics"))]
                let capture = {
                    // Inspect what reached the swap chain, before Present.
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                    )]);
                    let readback = crate::capture::read_back(&self.device, list, &back);
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    )]);
                    readback
                };
                #[cfg(all(not(test), feature = "diagnostics"))]
                let capture = crate::capture::wanted(frame)
                    .then(|| crate::capture::read_back(&self.device, list, texture))
                    .flatten();
                #[cfg(feature = "diagnostics")]
                let probe = crate::capture::probe(&self.device, list, texture);
                #[cfg(not(feature = "diagnostics"))]
                let (capture, probe) = {
                    let _ = frame;
                    (None::<()>, None::<()>)
                };
                let mut after = Vec::with_capacity(2);
                if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    after.push(transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state));
                }
                after.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                ));
                list.ResourceBarrier(&after);
                (capture, probe)
            } else {
                let comp = p
                    .compositor
                    .as_ref()
                    .expect("compositor was prepared for multiple planes");
                let rtv = comp.rtv_heap.GetCPUDescriptorHandleForHeapStart();
                self.device.CreateRenderTargetView(
                    &back,
                    Some(&D3D12_RENDER_TARGET_VIEW_DESC {
                        Format: format,
                        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                            Texture2D: D3D12_TEX2D_RTV {
                                MipSlice: 0,
                                PlaneSlice: 0,
                            },
                        },
                    }),
                    rtv,
                );
                let srv_cpu_base = comp.srv_heap.GetCPUDescriptorHandleForHeapStart();
                let srv_gpu_base = comp.srv_heap.GetGPUDescriptorHandleForHeapStart();
                let count = planes.len().min(MAX_PLANES as usize);
                let mut before = Vec::with_capacity(count + 1);
                let mut after = Vec::with_capacity(count);
                for (i, &(tex, state)) in planes[..count].iter().enumerate() {
                    let d = tex.GetDesc();
                    let srv_fmt = swap_format(d.Format).unwrap_or(d.Format);
                    let handle = D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: srv_cpu_base.ptr + i * comp.srv_stride as usize,
                    };
                    self.device.CreateShaderResourceView(
                        tex,
                        Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                            Format: srv_fmt,
                            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                                Texture2D: D3D12_TEX2D_SRV {
                                    MostDetailedMip: 0,
                                    MipLevels: 1,
                                    PlaneSlice: 0,
                                    ResourceMinLODClamp: 0.0,
                                },
                            },
                        }),
                        handle,
                    );
                    let state = D3D12_RESOURCE_STATES(state as i32);
                    if state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE
                        && planes[..i]
                            .iter()
                            .all(|(earlier, _)| earlier.as_raw() != tex.as_raw())
                    {
                        before.push(transition(
                            tex,
                            state,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                        ));
                        after.push(transition(
                            tex,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                            state,
                        ));
                    }
                }
                before.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                ));
                list.ResourceBarrier(&before);
                list.SetDescriptorHeaps(&[Some(comp.srv_heap.clone())]);
                list.SetGraphicsRootSignature(&comp.root);
                list.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: width as f32,
                    Height: height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }]);
                list.OMSetRenderTargets(1, Some(&rtv), false, None);
                list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                for i in 0..count {
                    list.SetPipelineState(if i == 0 { &comp.opaque } else { &comp.overlay });
                    list.SetGraphicsRootDescriptorTable(
                        0,
                        D3D12_GPU_DESCRIPTOR_HANDLE {
                            ptr: srv_gpu_base.ptr + (i as u64) * u64::from(comp.srv_stride),
                        },
                    );
                    list.DrawInstanced(3, 1, 0, 0);
                }
                #[cfg(feature = "diagnostics")]
                let (capture, probe) = {
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                    )]);
                    let capture = (cfg!(test) || crate::capture::wanted(frame))
                        .then(|| crate::capture::read_back(&self.device, list, &back))
                        .flatten();
                    let probe = crate::capture::probe(&self.device, list, &back);
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_COPY_SOURCE,
                        D3D12_RESOURCE_STATE_PRESENT,
                    )]);
                    (capture, probe)
                };
                #[cfg(not(feature = "diagnostics"))]
                let (capture, probe) = {
                    let _ = frame;
                    list.ResourceBarrier(&[transition(
                        &back,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        D3D12_RESOURCE_STATE_PRESENT,
                    )]);
                    (None::<()>, None::<()>)
                };
                if !after.is_empty() {
                    list.ResourceBarrier(&after);
                }
                (capture, probe)
            };
            p.copy.close();
            self.execute(&[&p.copy]);
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                // Tests inspect the completed copy before flip-discard Present
                // can invalidate the back buffer, with the presenter still locked.
                #[cfg(test)]
                inspect(&buffer, &footprint);
                crate::capture::save_read_back(frame, &buffer, &footprint);
            }
            #[cfg(feature = "diagnostics")]
            let present_started = Instant::now();
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            #[cfg(feature = "diagnostics")]
            let present_finished = Instant::now();
            p.meter.frame(
                p.hwnd,
                #[cfg(feature = "diagnostics")]
                PresentPhase {
                    entry: present_entry,
                    lock_requested: presenter_lock_requested,
                    lock_acquired: presenter_lock_acquired,
                    present_started,
                    present_finished,
                },
            );
            #[cfg(feature = "diagnostics")]
            {
                let present_end = Instant::now();
                p.meter.present_phase_us[5] +=
                    present_end.duration_since(present_finished).as_micros() as u64;
                p.meter.last_present_end = Some(present_end);
            }
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            {
                let _ = (capture, probe);
                Some(false)
            }
        }
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> Result<Presenter, (&'static str, windows::core::Error)> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height).map_err(|e| ("window", e))?,
        };
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
                .map_err(|e| ("DXGI factory", e))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let swap_chain = factory
                .CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)
                .and_then(|chain| chain.cast::<IDXGISwapChain3>())
                .map_err(|e| ("swap chain", e))?;
            let copy = self.new_list().map_err(|e| ("copy list", e))?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                hwnd,
                meter: Meter::new(),
                compositor: None,
            })
        }
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

impl Host {
    /// Diagnostics, `VICTORY_COPIES_IN_VIDEO_MEMORY`: [`Host::shared_buffer`]s
    /// (host copies of title memory among them) in video memory the CPU maps
    /// (a GPU upload heap, where the host has one), to measure how much of
    /// the GPU's time is shaders reading the copies over the bus. CPU reads
    /// of them (snapshots, copy-backs) become slow: for that measurement only.
    #[cfg(feature = "diagnostics")]
    fn copies_in_video_memory(&self) -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            if !crate::switch::on("VICTORY_COPIES_IN_VIDEO_MEMORY") {
                return false;
            }
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
            // SAFETY: the query writes its own structure of the given size.
            let ok = unsafe {
                self.device.CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS16,
                    (&raw mut options).cast(),
                    size_of_val(&options) as u32,
                )
            }
            .is_ok()
                && options.GPUUploadHeapSupported.as_bool();
            tracing::warn!(
                on = ok,
                "host copies of title memory in video memory (diagnostics)"
            );
            ok
        })
    }

    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    ///
    /// Small ones share write-back heaps ([`crate::placement::place_buffer`]).
    /// One committed resource per title copy was a heap each, and world load
    /// removed the device once about 9,500 heaps were live.
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let write_back = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        #[cfg(feature = "diagnostics")]
        if self.copies_in_video_memory() {
            let upload = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_GPU_UPLOAD,
                ..Default::default()
            };
            return self.commit_shared(&upload, size);
        }
        let desc = shared_buffer_desc(size);
        // SAFETY: a plain query.
        let info = unsafe { self.device.GetResourceAllocationInfo(0, &[desc]) };
        if info.SizeInBytes != u64::MAX
            && let Some((heap, place)) =
                crate::placement::place_buffer(&self.device, info.SizeInBytes, info.Alignment)
        {
            let mut buffer: Option<ID3D12Resource> = None;
            // SAFETY: a range of the heap for this buffer; our out pointer.
            let created = unsafe {
                self.device.CreatePlacedResource(
                    &heap,
                    place.offset(),
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
            };
            if let (Ok(()), Some(buffer)) = (created, buffer) {
                let mut data = std::ptr::null_mut();
                // SAFETY: mapping a live write-back buffer.
                if unsafe { buffer.Map(0, None, Some(&mut data)) }.is_ok() {
                    // A reused range still holds the previous buffer.
                    // A range handed out for the first time is still the
                    // heap's demand-zero pages: touching every byte would
                    // commit them before the title writes.
                    if !place.fresh() {
                        // SAFETY: the mapping covers the buffer.
                        unsafe { std::ptr::write_bytes(data, 0, desc.Width as usize) };
                    }
                    crate::placement::note_buffer(buffer.clone(), place);
                    return Some((buffer, data.cast()));
                }
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!("a write-back buffer was not placed; that one is committed")
            });
            crate::placement::reject_buffer(place);
        }
        self.commit_shared(&write_back, size)
    }

    /// A root-view copy above the nonlocal admission limit may reuse bytes
    /// already handed out in a write-back heap. It must not create a heap or
    /// fall back to a committed resource.
    pub(crate) fn shared_buffer_reused(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let desc = shared_buffer_desc(size);
        // SAFETY: a plain resource allocation query.
        let info = unsafe { self.device.GetResourceAllocationInfo(0, &[desc]) };
        if info.SizeInBytes == u64::MAX {
            return None;
        }
        let (heap, place) = crate::placement::place_buffer_reused(info.SizeInBytes, info.Alignment)?;
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: the placement owns a range of the existing heap.
        let created = unsafe {
            self.device.CreatePlacedResource(
                &heap,
                place.offset(),
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        if let (Ok(()), Some(buffer)) = (created, buffer) {
            let mut data = std::ptr::null_mut();
            // SAFETY: mapping a live write-back buffer.
            if unsafe { buffer.Map(0, None, Some(&mut data)) }.is_ok() {
                // The range held an older buffer: remove its bytes before a
                // new root view can see any of them.
                // SAFETY: a successful Map returned a writable range of at
                // least `desc.Width` bytes for this buffer.
                unsafe { std::ptr::write_bytes(data, 0, desc.Width as usize) };
                crate::placement::note_buffer(buffer.clone(), place);
                return Some((buffer, data.cast()));
            }
        }
        crate::placement::reject_buffer(place);
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("a root view could not reuse its write-back range"));
        None
    }

    /// [`Host::shared_buffer`] as one committed resource.
    fn commit_shared(
        &self,
        heap: &D3D12_HEAP_PROPERTIES,
        size: u64,
    ) -> Option<(ID3D12Resource, *mut u8)> {
        let desc = shared_buffer_desc(size);
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => Some((buffer?, data.cast())),
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }
}

/// A default-heap unordered-access buffer, created in the common state.
/// Small ones share heaps ([`gpu_buffer`]).
fn default_uav(size: u64) -> Option<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    gpu_buffer(&host()?.device, &desc, D3D12_RESOURCE_STATE_COMMON)
}

/// A default-heap buffer, placed in a shared heap when it fits and
/// committed otherwise.
fn gpu_buffer(
    device: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    // SAFETY: a plain query.
    let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
    if info.SizeInBytes != u64::MAX
        && let Some((heap, place)) =
            crate::placement::place_gpu_buffer(device, info.SizeInBytes, info.Alignment)
    {
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: a range of the heap for this buffer; our out pointer.
        let created = unsafe {
            device.CreatePlacedResource(&heap, place.offset(), desc, state, None, &mut buffer)
        };
        if let (Ok(()), Some(buffer)) = (created, buffer) {
            crate::placement::note_gpu_buffer(buffer.clone(), place);
            return Some(buffer);
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!("a default-heap buffer was not placed; that one is committed")
        });
        crate::placement::reject_gpu_buffer(place);
    }
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: a buffer description and our out pointer.
    unsafe {
        device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, desc, state, None, &mut buffer)
            .ok()?;
    }
    buffer
}

/// Description of a [`Host::shared_buffer`].
fn shared_buffer_desc(size: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    }
}

/// Host resources that view heap slots stopped viewing while a submission
/// may have been running ([`retire`]).
static RETIRED: Mutex<Vec<ID3D12Resource>> = Mutex::new(Vec::new());

/// Keeps `resource` alive until the GPU work running now (or the next
/// submission's) has run: title threads rewrite view heap slots while the
/// GPU runs, and a shader that took the old descriptor still reads what it
/// views.
pub(crate) fn retire(resource: ID3D12Resource) {
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(resource);
}

/// After a submission has run, with none running: the retired resources go.
pub(crate) fn release_retired() {
    let retired = std::mem::take(&mut *RETIRED.lock().unwrap_or_else(|e| e.into_inner()));
    if !retired.is_empty() {
        release_later(Box::new(retired));
    }
}

/// Drops `what` on a thread of its own: releasing a D3D12 resource is a
/// kernel call (~5 % of a world frame on the title's submitting thread,
/// run perf1). Only for objects no GPU work in flight uses.
fn release_later(what: Box<dyn std::any::Any + Send>) {
    type Release = std::sync::mpsc::Sender<Box<dyn std::any::Any + Send>>;
    static RELEASE: OnceLock<Release> = OnceLock::new();
    let release = RELEASE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn std::any::Any + Send>>();
        // Without the thread the receiver is gone: sends fail and drop here.
        let _ = std::thread::Builder::new()
            .name("victory-release".into())
            .spawn(move || rx.into_iter().for_each(drop));
        tx
    });
    let _ = release.send(what);
}

/// Diagnostics: a noted command running a shader `VICTORY_TRACE_SHADERS`
/// names (hashes, comma-separated) is logged with what it binds: all of
/// them in listed capture frames, the first 500 elsewhere.
#[cfg(feature = "diagnostics")]
fn traced(what: &str) {
    static LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if is_traced(what)
        && (crate::capture::listed(crate::queue::presented() + 1)
            || LOGGED.fetch_add(1, Ordering::Relaxed) < 500)
    {
        tracing::info!("traced command: {what}");
    }
}

/// The shader hashes `VICTORY_TRACE_SHADERS` lists (comma-separated;
/// diagnostics).
#[cfg(feature = "diagnostics")]
pub(crate) fn traced_shaders() -> &'static [String] {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    HASHES.get_or_init(|| {
        let v = std::env::var("VICTORY_TRACE_SHADERS").unwrap_or_default();
        v.split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

/// Whether `what` names a shader [`traced_shaders`] lists.
#[cfg(feature = "diagnostics")]
pub(crate) fn is_traced(what: &str) -> bool {
    traced_shaders().iter().any(|h| what.contains(h.as_str()))
}

/// Diagnostics, `VICTORY_GPU_TIMING`: every command of the list during
/// whose run (each list alone) the device was found removed.
fn removed_during(runs: &[&HostList], list: Option<usize>) -> String {
    #[cfg(feature = "diagnostics")]
    if let Some(i) = list {
        let notes = runs[i].notes.lock().unwrap_or_else(|e| e.into_inner());
        let mut text = format!(
            "; removed while list {i} ran alone ({} commands):",
            notes.len()
        );
        for (k, (n, _)) in notes.iter().enumerate() {
            text.push_str(&format!(" [{k}] {n}"));
        }
        return text;
    }
    let _ = (runs, list);
    String::new()
}

/// The submission currently inside `ExecuteCommandLists` or its fence wait:
/// its last noted commands and the pipelines it bound. A later list reset
/// can be the first call to see the device removed.
static IN_FLIGHT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// What [`HostList::reset`] appends when the device is already removed:
/// device-removed data, then the in-flight submission's notes.
fn reset_context() -> String {
    let Some(host) = host() else {
        return String::new();
    };
    // SAFETY: a plain query.
    let removed = unsafe { host.device.GetDeviceRemovedReason() }.is_err();
    let data = if removed {
        host.removal_data(&[])
    } else {
        String::new()
    };
    let flight = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    format!("{data}{flight}")
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!(
                    "; list {i} of {}, of its {}: {n}",
                    runs.len(),
                    notes.len()
                ));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Last noted commands plus a few pipelines of the submission about to run.
fn flight_report(runs: &[&HostList]) -> String {
    let mut text = last_notes(runs);
    for (i, r) in runs.iter().enumerate() {
        if text.len() > 2000 {
            text.push_str("; …");
            break;
        }
        let pipelines = r.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if pipelines.is_empty() {
            continue;
        }
        let n = pipelines.len();
        let names: Vec<&str> = pipelines.iter().take(4).map(String::as_str).collect();
        text.push_str(&format!(
            "; list {i} of {} pipelines {n}: {names:?}",
            runs.len()
        ));
    }
    text
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
fn in_flight(
    history: &[D3D12_AUTO_BREADCRUMB_OP],
    markers: &[(u32, String)],
    done: usize,
) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done]
        .iter()
        .rposition(barrier)
        .map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    for i in from..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

/// Barriers that put `u`'s first use where the list starts, from the
/// states the previous submission left.
#[derive(Debug, PartialEq, Eq)]
enum AliasOpenStep {
    ActivatePrimary,
    PrimaryTransition(u32, u32),
    ActivateAlias,
    AliasTransition(u32, u32),
}

/// An alias-first list can switch back to its primary after the prelude.
/// Its first in-list transition was recorded against `primary_initial`,
/// which may be stale by submission time. Establish that state before
/// activating the alias, so the later transition has the before-state it
/// names. The list's final primary state is a separate value.
fn first_alias_open_steps(
    prior_alias_owner: bool,
    primary_real: u32,
    primary_initial: u32,
    alias_real: u32,
    alias_first: u32,
    primary_after_alias: bool,
) -> Vec<AliasOpenStep> {
    use AliasOpenStep::*;
    let mut steps = Vec::new();
    if primary_after_alias && primary_real != primary_initial {
        if prior_alias_owner {
            steps.push(ActivatePrimary);
        }
        steps.push(PrimaryTransition(primary_real, primary_initial));
        steps.push(ActivateAlias);
    } else if !prior_alias_owner {
        steps.push(ActivateAlias);
    }
    if alias_real != alias_first {
        steps.push(AliasTransition(alias_real, alias_first));
    }
    steps
}

#[cfg(test)]
mod alias_open_tests {
    use super::{AliasOpenStep::*, first_alias_open_steps};

    #[test]
    fn later_rtv_transition_gets_its_recorded_common_before_state() {
        // The list recorded alias SRV first, then primary COMMON -> RTV.
        // Its final primary state is RTV, but the transition inside the list
        // still expects COMMON. An earlier list left the real primary in UAV.
        let primary_initial = 0;
        let primary_final = 0x4;
        assert_ne!(primary_initial, primary_final);
        assert_eq!(
            first_alias_open_steps(false, 0x8, primary_initial, 0, 0xc0, true),
            [
                PrimaryTransition(0x8, 0),
                ActivateAlias,
                AliasTransition(0, 0xc0)
            ]
        );
    }

    #[test]
    fn prior_alias_owner_is_switched_twice_for_primary_normalization() {
        assert_eq!(
            first_alias_open_steps(true, 0x8, 0, 0xc0, 0xc0, true),
            [ActivatePrimary, PrimaryTransition(0x8, 0), ActivateAlias]
        );
    }

    #[test]
    fn matching_final_primary_still_normalizes_to_initial_before_alias() {
        // Previously the prelude compared the real state with the final RTV
        // state, did nothing, then the in-list UAV -> RTV barrier mismatched.
        let primary_initial = 0x8;
        let primary_final = 0x4;
        assert_eq!(
            first_alias_open_steps(false, primary_final, primary_initial, 0xc0, 0xc0, true),
            [PrimaryTransition(primary_final, primary_initial), ActivateAlias]
        );
    }

    #[test]
    fn alias_only_list_does_not_disturb_the_primary_state() {
        assert_eq!(first_alias_open_steps(true, 0x8, 0, 0xc0, 0xc0, false), []);
    }
}

fn needs_open(u: &Tracked) -> bool {
    let copy = &u.copy;
    if u.first_alias && copy.shared_exp_resource().is_some() {
        let primary_real = copy.state.load(Ordering::Relaxed);
        if copy.shared_exp_separate() {
            return (u.primary_after_alias && primary_real != u.primary_initial)
                || copy.alias_state() != u.first;
        }
        return !first_alias_open_steps(copy.alias_owner(), primary_real,
            u.primary_initial, copy.alias_state(), u.first,
            u.primary_after_alias).is_empty();
    }
    (!copy.shared_exp_separate() && copy.alias_owner()
        && copy.shared_exp_resource().is_some())
        || copy.state.load(Ordering::Relaxed) != u.first
}

fn open_move(u: &Tracked) -> Vec<D3D12_RESOURCE_BARRIER> {
    let copy = &u.copy;
    let mut barriers = Vec::new();
    if u.first_alias {
        let Some(alias) = copy.shared_exp_resource() else {
            return primary_open(u);
        };
        let primary_real = copy.state.load(Ordering::Relaxed);
        if copy.shared_exp_separate() {
            // Both resources remain active. The alias-first list may still
            // contain a later primary transition recorded against the old
            // primary state, so normalize that state in the prelude.
            if u.primary_after_alias && primary_real != u.primary_initial {
                barriers.push(transition(
                    &copy.texture,
                    D3D12_RESOURCE_STATES(primary_real as i32),
                    D3D12_RESOURCE_STATES(u.primary_initial as i32),
                ));
            }
            let alias_real = copy.alias_state();
            if alias_real != u.first {
                barriers.push(transition(
                    &alias,
                    D3D12_RESOURCE_STATES(alias_real as i32),
                    D3D12_RESOURCE_STATES(u.first as i32),
                ));
            }
            return barriers;
        }
        #[cfg(feature = "diagnostics")]
        if u.primary_after_alias && primary_real != u.primary_initial {
            static NORMALIZED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let count = NORMALIZED.fetch_add(1, Ordering::Relaxed) + 1;
            if count <= 8 || count.is_power_of_two() {
                tracing::info!(
                    count,
                    resource = ?copy.texture.as_raw(),
                    primary_real,
                    recorded_primary = u.primary_initial,
                    final_primary = u.primary_at,
                    prior_alias_owner = copy.alias_owner(),
                    "normalizing primary before alias-first list uses it"
                );
            }
        }
        for step in first_alias_open_steps(
            copy.alias_owner(),
            primary_real,
            u.primary_initial,
            copy.alias_state(),
            u.first,
            u.primary_after_alias,
        ) {
            match step {
                AliasOpenStep::ActivatePrimary => {
                    barriers.push(aliasing_barrier(&alias, &copy.texture));
                }
                AliasOpenStep::PrimaryTransition(before, after) => {
                    barriers.push(transition(
                        &copy.texture,
                        D3D12_RESOURCE_STATES(before as i32),
                        D3D12_RESOURCE_STATES(after as i32),
                    ));
                }
                AliasOpenStep::ActivateAlias => {
                    let before_alias = if u.primary_after_alias {
                        u.primary_initial
                    } else {
                        primary_real
                    };
                    if before_alias & 0x8 != 0 {
                        barriers.push(uav_barrier(&copy.texture));
                    }
                    barriers.push(aliasing_barrier(&copy.texture, &alias));
                }
                AliasOpenStep::AliasTransition(before, after) => {
                    barriers.push(transition(
                        &alias,
                        D3D12_RESOURCE_STATES(before as i32),
                        D3D12_RESOURCE_STATES(after as i32),
                    ));
                }
            }
        }
        return barriers;
    }
    primary_open(u)
}

fn primary_open(u: &Tracked) -> Vec<D3D12_RESOURCE_BARRIER> {
    let copy = &u.copy;
    let mut barriers = Vec::new();
    if !copy.shared_exp_separate()
        && copy.alias_owner()
        && let Some(alias) = copy.shared_exp_resource()
    {
        barriers.push(aliasing_barrier(&alias, &copy.texture));
    }
    let real = copy.state.load(Ordering::Relaxed);
    if real != u.first {
        barriers.push(transition(
            &copy.texture,
            D3D12_RESOURCE_STATES(real as i32),
            D3D12_RESOURCE_STATES(u.first as i32),
        ));
    }
    barriers
}

/// Remembers the states `u`'s list leaves, for the next submission.
fn publish(u: &Tracked) {
    // A list that only samples the alias did not change the primary.
    if !u.first_alias || !u.last_alias || u.primary_after_alias {
        u.copy.state.store(u.primary_at, Ordering::Relaxed);
    }
    if u.first_alias || u.last_alias {
        u.copy.set_alias_state(u.alias_at);
    }
    if u.copy.shared_exp_resource().is_some() {
        u.copy.set_alias_owner(u.last_alias);
    }
}

/// An unordered-access barrier on `resource`: its shader writes finish
/// before an aliased resource reads the same bytes.
fn uav_barrier(resource: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
            }),
        },
    }
}

/// Switches aliased resources. `before` was used last; `after` is next.
fn aliasing_barrier(before: &ID3D12Resource, after: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                // SAFETY: borrowed pointers; the barrier never releases them.
                pResourceBefore: unsafe { std::mem::transmute_copy(before) },
                // SAFETY: borrowed pointers; the barrier never releases them.
                pResourceAfter: unsafe { std::mem::transmute_copy(after) },
            }),
        },
    }
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    #[cfg(test)]
    tests::ISSUED.with(|l| {
        l.borrow_mut()
            .push((resource.as_raw() as usize, before.0 as u32, after.0 as u32))
    });
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod batch {
    // `mod tests` is already taken at the top of this file
    use super::Batch;

    /// A host copy of tracked title memory: a submission whose lists all ask
    /// for it whole copies the one page the title wrote, once, and again
    /// only after a step that writes title memory.
    #[test]
    fn a_copy_is_refreshed_once_per_submission_by_the_pages_written() {
        use crate::watch::PAGE;
        let p = crate::watch::tests::pool(4);
        let len = 4 * PAGE;
        crate::shadow::register(p, p as u64, len as u64);
        let Some(copy) = crate::shadow::find(p as u64, len as u64) else {
            crate::shadow::unregister(p, p as u64);
            return; // no host GPU here
        };
        let mut batch = Batch::default();
        batch.add(&copy, p, len);
        assert_eq!(batch.flush(), len, "all of it at first");
        // SAFETY: our pool, guarded since the copy took it.
        unsafe { ((p + PAGE + 8) as *mut u8).write_volatile(7) };
        // The next submission: two lists ask for the copy.
        let mut batch = Batch::default();
        batch.add(&copy, p, len);
        batch.add(&copy, p + 100, 200);
        assert_eq!(batch.flush(), PAGE, "the page written");
        assert_eq!(copy.read_host((p + PAGE + 8) as u64, 1)[0], 7);
        // SAFETY: as above.
        unsafe { (p as *mut u8).write_volatile(1) };
        batch.add(&copy, p, len);
        assert_eq!(batch.flush(), 0, "nothing again in the submission");
        batch.title_written();
        batch.add(&copy, p, len);
        assert_eq!(batch.flush(), PAGE, "after a title write, what changed");
        crate::shadow::unregister(p, p as u64);
        drop(copy);
        crate::watch::tests::free(p);
    }
}

#[cfg(test)]
mod withholding {
    /// A list in which a program using title memory missed memory: what
    /// that program wrote is withheld from title memory and leaves the
    /// copies at once; what an ordinary program wrote in the same list goes
    /// back and stays in its copy (run light13: the imposter transforms in
    /// the BVH builder's list); an ordinary range overlapping a withheld one
    /// is withheld with it, since its copy-back would take the other's bytes
    /// along.
    #[test]
    fn only_programs_using_title_memory_are_withheld() {
        use crate::watch::PAGE;
        let Some(host) = super::host() else {
            return; // no host GPU here
        };
        let pools = [crate::watch::tests::pool(1), crate::watch::tests::pool(1)];
        for &p in &pools {
            // SAFETY: our pool, not guarded yet.
            unsafe { ((p + 16) as *mut u8).write_volatile(0x11) };
            crate::shadow::register(p, p as u64, PAGE as u64);
        }
        let copies: Vec<_> = pools
            .iter()
            .map(|&p| crate::shadow::find(p as u64, PAGE as u64).unwrap())
            .collect();
        let list = host.new_list().unwrap();
        // (copy, start, bytes, the writer uses title memory, byte written at +0 and +16)
        let writes = [
            (0, 0, 32, false, 0x22),
            (1, 16, 16, true, 0x33),
            (1, 0, 32, false, 0x44),
        ];
        for &(k, at, len, memory, value) in &writes {
            let _writer = list.writer_uses_memory(memory);
            let (copy, address) = (copies[k].clone(), (pools[k] + at) as u64);
            copy.refresh(&list, address, len);
            copy.gpu_writes(&list, address, len, false);
            // The shader's write, after the step that keeps the range's bytes.
            list.defer_host(move || copy.write_host(address, &[value]));
        }
        list.defer_missed(|| true);
        list.close();
        host.execute(&[&list]);
        // SAFETY: bytes of our pools.
        let title = |k: usize, at: usize| unsafe { ((pools[k] + at) as *const u8).read_volatile() };
        let copy = |k: usize, at: usize| {
            copies[k].update(pools[k], PAGE);
            copies[k].read_host((pools[k] + at) as u64, 1)[0] & 0xff
        };
        assert_eq!(
            (title(0, 0), copy(0, 0)),
            (0x22, 0x22),
            "an ordinary program's write goes back and stays"
        );
        assert_eq!(
            (title(1, 16), copy(1, 16)),
            (0x11, 0x11),
            "withheld, then dropped from the copy"
        );
        assert_eq!(
            (title(1, 0), copy(1, 0)),
            (0, 0),
            "overlapping a withheld range: withheld with it"
        );
        for (&p, copy) in pools.iter().zip(copies) {
            crate::shadow::unregister(p, p as u64);
            drop(copy);
            crate::watch::tests::free(p);
        }
    }
}

#[cfg(all(test, feature = "diagnostics"))]
mod present_plane_tests {
    use super::*;

    #[test]
    fn compositor_failure_presents_primary_in_the_same_frame() {
        let Some(host) = host() else {
            eprintln!("skipping PresentX recovery: no D3D12 device");
            return;
        };
        let list = host.new_list().unwrap();
        let make_plane = |width, height, color: &[f32; 4]| {
            let mut texture = None;
            // SAFETY: live device, valid render target description, and an open list.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                            Width: width,
                            Height: height,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                            ..Default::default()
                        },
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut texture,
                    )
                    .unwrap();
                let texture = texture.unwrap();
                list.list
                    .ClearRenderTargetView(host.rtv(&texture, None), color, None);
                texture
            }
        };
        let scene = make_plane(32, 32, &[1.0, 0.0, 0.0, 1.0]);
        let overlay = make_plane(64, 64, &[0.0, 0.0, 128.0 / 255.0, 128.0 / 255.0]);
        list.close();
        host.execute(&[&list]);
        let state = D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32;
        assert!(
            host.present(&[(&scene, state)], 1).is_some(),
            "single-plane control"
        );
        let first_pixel =
            |buffer: &ID3D12Resource, footprint: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| {
                let mut mapped = std::ptr::null_mut();
                // SAFETY: the present copy has completed. The footprint names
                // a live RGBA pixel inside the readback buffer.
                let rgba = unsafe {
                    buffer.Map(0, None, Some(&mut mapped)).unwrap();
                    let rgba = mapped
                        .cast::<u8>()
                        .add(footprint.Offset as usize)
                        .cast::<[u8; 4]>()
                        .read();
                    buffer.Unmap(0, None);
                    rgba
                };
                (footprint.Footprint.Width, footprint.Footprint.Height, rgba)
            };

        let mut attempted = false;
        let mut fallback = None;
        let shown = host.present_with_compositor(
            &[(&scene, state), (&overlay, state)],
            2,
            |_, _| {
                attempted = true;
                Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ))
            },
            |buffer, footprint| fallback = Some(first_pixel(buffer, footprint)),
        );
        assert!(attempted);
        assert!(
            shown.is_some(),
            "the primary plane must still be shown in this frame"
        );
        assert_eq!(fallback, Some((32, 32, [255, 0, 0, 255])));
        let presentation = host.presenter.lock().unwrap_or_else(|e| e.into_inner());
        let swap_chain = presentation.presenter.as_ref().unwrap();
        assert_eq!((swap_chain.width, swap_chain.height), (32, 32));
        drop(presentation);

        let mut composite = None;
        assert!(
            host.present_with_compositor(
                &[(&scene, state), (&overlay, state)],
                3,
                Compositor::new,
                |buffer, footprint| composite = Some(first_pixel(buffer, footprint)),
            )
            .is_some(),
            "compositor initialization can be retried after failure"
        );
        // Premultiplied half-blue over opaque red.
        assert_eq!(composite, Some((64, 64, [127, 0, 128, 255])));
    }
}
