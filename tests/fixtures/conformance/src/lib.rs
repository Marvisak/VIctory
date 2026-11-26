//! The conformance title: calls the console runtime as a title does and
//! checks what the public platform documentation says each call does.
//! Results are TAP lines, read by the harness
//! (`crates/victory-hle/tests/conformance.rs`) through `conformance_run`.
//!
//! How a title reaches the runtime is VIctory's own description
//! (`crates/victory-hle/src/modules/xgameruntime.rs`): code linked into the
//! title loads `xgameruntime.dll`, asks `QueryApiImpl` for an area's object
//! by provider and interface id, and calls one of its methods. The ids and
//! method slots below are the ones VIctory implements, so these tests check
//! behaviour, not that binary interface (only the title itself can check
//! that). Everything else (the behaviour, the constants, the structure
//! layouts) is from the public reference, cited per test.

#![allow(clippy::missing_safety_doc)]
// The harness calls the report exports as title code would, with pointers
// the real API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod dstorage;
mod files;
mod gfx10;
mod gpu;
mod pls;
mod rt;
pub mod scene;
mod system;
mod tap;
mod xasync;
mod xgamesave;
mod xmem;
mod xsystem;
mod xuser;

/// Runs every test; returns the TAP report (valid until the next call).
#[unsafe(no_mangle)]
pub extern "system" fn conformance_run(len: *mut usize) -> *const u8 {
    let mut t = tap::Tap::default();
    rt::init(&mut t);
    xasync::run(&mut t);
    xsystem::run(&mut t);
    xmem::run(&mut t);
    dstorage::run(&mut t);
    xgamesave::run(&mut t);
    xuser::run(&mut t);
    system::run(&mut t);
    files::run(&mut t);
    pls::run(&mut t);
    let report = t.finish();
    // SAFETY: the harness passes room for the length.
    unsafe { len.write(report.len()) };
    report.as_ptr()
}

/// Runs the workloads; returns their report (valid until the next call).
#[unsafe(no_mangle)]
pub extern "system" fn conformance_bench(len: *mut usize) -> *const u8 {
    static REPORT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    let mut report = REPORT.lock().unwrap_or_else(|e| e.into_inner());
    *report = format!(
        "streaming (DirectStorage, memory sources)
{}",
        dstorage::bench()
    );
    // SAFETY: the harness passes room for the length.
    unsafe { len.write(report.len()) };
    report.as_ptr()
}

/// What the harness passes [`conformance_gpu`]: the WARP reference image
/// (RGBA floats per pixel, null for none) and how long to keep presenting
/// frames in a window afterwards (0: no window), at
/// `window_width` x `window_height`, with `window_scale` times the work. The frame read back is
/// handed out in `image` (valid until the next call).
#[repr(C)]
pub struct GpuRun {
    pub reference: *const f32,
    pub reference_len: usize,
    pub window_seconds: u32,
    pub window_width: u32,
    pub window_height: u32,
    pub window_scale: u32,
    pub image: *const f32,
    pub image_len: usize,
    pub window_image: *const f32,
    pub window_image_len: usize,
}

/// Runs the GPU frame (`gpu`); returns its TAP report (valid until the
/// next call).
#[unsafe(no_mangle)]
pub extern "system" fn conformance_gpu(run: *mut GpuRun, len: *mut usize) -> *const u8 {
    static IMAGE: std::sync::Mutex<Vec<f32>> = std::sync::Mutex::new(Vec::new());
    static WINDOW_IMAGE: std::sync::Mutex<Vec<f32>> = std::sync::Mutex::new(Vec::new());
    // SAFETY: the harness passes its parameters.
    let run = unsafe { &mut *run };
    let reference = (!run.reference.is_null())
        // SAFETY: the harness passes `reference_len` floats.
        .then(|| unsafe { std::slice::from_raw_parts(run.reference, run.reference_len) });
    let mut t = tap::Tap::default();
    let done = gpu::run(
        &mut t,
        reference,
        run.window_seconds,
        (run.window_width, run.window_height),
        run.window_scale,
    );
    let mut image = IMAGE.lock().unwrap_or_else(|e| e.into_inner());
    *image = done.image;
    run.image = image.as_ptr();
    run.image_len = image.len();
    let mut window_image = WINDOW_IMAGE.lock().unwrap_or_else(|e| e.into_inner());
    *window_image = done.window_image;
    run.window_image = window_image.as_ptr();
    run.window_image_len = window_image.len();
    let report = t.finish();
    // SAFETY: the harness passes room for the length.
    unsafe { len.write(report.len()) };
    report.as_ptr()
}
