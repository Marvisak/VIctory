//! The console's audio DSP (`chad.dll`): streams that convolve blocks of
//! float samples with an impulse response (reverb), or run other block
//! transforms, on a coprocessor.
//!
//! A title connects once with a memory block for the DSP, activates
//! streams, and submits one command per block (gains, an input and an
//! output buffer). Activation hands back a status block whose second word is
//! the ticket of the last completed command and whose fourth is its result;
//! the title checks it one block later and treats a late command as an
//! overrun. Commands complete when submitted here.
//!
//! The transforms themselves are not computed: the hardware takes impulse
//! responses already in its own partitioned frequency-domain layout, which
//! is not known. Outputs are silence, so reverb returns are dry.
// shortcut: no convolution or block transform; outputs are zeros. Needs the
// coprocessor's impulse response and spectrum layouts to do better.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

const E_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;

const CONVOLUTION: u32 = 1;
/// Option bits that need a stereo stream, and one convolution refuses.
const STEREO_OPTIONS: u32 = 2 | 8;
const NOT_FOR_CONVOLUTION: u32 = 4;

pub struct Dsp {
    max_streams: u32,
    active: Mutex<u32>,
}

/// What activation describes.
#[repr(C, packed)]
pub struct StreamDesc {
    kind: u32,
    options: u32,
    frames: u32,
    channels: u32,
    ir_floats: u32,
    ir: *const f32,
}

/// The status block a stream's owner polls.
#[repr(C)]
pub struct Status {
    submitted: AtomicU32,
    completed: AtomicU32,
    reserved: AtomicU32,
    result: AtomicI32,
}

const _: () = {
    assert!(size_of::<Status>() == 16);
    assert!(align_of::<Status>() == 4);
    assert!(std::mem::offset_of!(Status, submitted) == 0);
    assert!(std::mem::offset_of!(Status, completed) == 4);
    assert!(std::mem::offset_of!(Status, reserved) == 8);
    assert!(std::mem::offset_of!(Status, result) == 12);
};

pub struct Stream {
    dsp: *mut Dsp,
    frames: u32,
    channels: u32,
    /// Serializes submissions so tickets advance monotonically and each
    /// output buffer finishes before `status.completed` publishes it.
    submit: Mutex<u32>,
    status: Box<Status>,
}

/// One block of work.
#[repr(C)]
pub struct Command {
    gains: [f32; 2],
    input: *const f32,
    output: *mut f32,
    reserved: u64,
}

pub extern "system" fn XDspConnectWithMaximumStreamLimit(
    base: *mut c_void,
    base_length: u32,
    ir_seconds: u32,
    max_streams: u32,
    max_channels: u32,
    out: *mut *mut Dsp,
) -> HRESULT {
    if base.is_null()
        || !(0x1_0000..=0x1F40_0000).contains(&base_length)
        || ir_seconds > 0x80
        || max_streams > 0x100
        || max_channels > 2
    {
        return E_INVALIDARG;
    }
    if out.is_null() {
        return E_POINTER;
    }
    tracing::info!(
        base_length,
        ir_seconds,
        max_streams,
        max_channels,
        "audio DSP connected"
    );
    let dsp = Box::new(Dsp {
        max_streams,
        active: Mutex::new(0),
    });
    // SAFETY: checked non-null.
    unsafe { out.write(Box::into_raw(dsp)) };
    S_OK
}

pub extern "system" fn XDspDisconnect(dsp: *mut Dsp) -> HRESULT {
    if dsp.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XDspConnectWithMaximumStreamLimit`; disconnected
    // once, after its streams.
    drop(unsafe { Box::from_raw(dsp) });
    S_OK
}

fn valid(desc: &StreamDesc) -> bool {
    let (kind, options, frames, channels) = (desc.kind, desc.options, desc.frames, desc.channels);
    let shape = if kind == CONVOLUTION {
        (1..=2).contains(&channels)
            && matches!(frames, 0x200 | 0x400)
            && options & NOT_FOR_CONVOLUTION == 0
    } else {
        channels == 1 && matches!(frames, 0x200 | 0x400 | 0x800)
    };
    shape && (options & STEREO_OPTIONS == 0 || channels == 2)
}

pub extern "system" fn XDspActivate(
    dsp: *mut Dsp,
    desc: *const StreamDesc,
    status: *mut *const Status,
    stream: *mut *mut Stream,
) -> HRESULT {
    if status.is_null() || stream.is_null() {
        return E_POINTER;
    }
    if dsp.is_null() || desc.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes its description.
    let desc = unsafe { &*desc };
    if !valid(desc) {
        return E_INVALIDARG;
    }
    let (kind, options, frames, channels) = (desc.kind, desc.options, desc.frames, desc.channels);
    tracing::debug!(
        kind,
        options,
        frames,
        channels,
        "audio DSP stream activated"
    );
    {
        // SAFETY: a connection from `XDspConnectWithMaximumStreamLimit`.
        let dsp = unsafe { &*dsp };
        let mut active = dsp.active.lock().unwrap_or_else(|e| e.into_inner());
        if *active >= dsp.max_streams {
            return E_OUTOFMEMORY;
        }
        *active += 1;
    }
    let new = Box::new(Stream {
        dsp,
        frames: desc.frames,
        channels: desc.channels,
        submit: Mutex::new(0),
        status: Box::new(Status {
            submitted: AtomicU32::new(0),
            completed: AtomicU32::new(0),
            reserved: AtomicU32::new(0),
            result: AtomicI32::new(S_OK),
        }),
    });
    let raw = Box::into_raw(new);
    // SAFETY: checked non-null; `raw` stays heap-allocated until
    // `XDspDeactivate`, and `status` is derived from `raw` after releasing
    // `Box` ownership so concurrent reads never alias a unique borrow.
    unsafe {
        status.write(std::ptr::from_ref(&*(*raw).status));
        stream.write(raw);
    }
    S_OK
}

pub extern "system" fn XDspDeactivate(stream: *mut Stream) -> HRESULT {
    if stream.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XDspActivate`; deactivated once.
    let stream = unsafe { Box::from_raw(stream) };
    // SAFETY: the connection outlives its streams.
    let dsp = unsafe { &*stream.dsp };
    *dsp.active.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    S_OK
}

/// Completes the command at once: its output block is silence.
pub extern "system" fn XDspSubmitCommand(
    stream: *mut Stream,
    command: *const Command,
    ticket: *mut u32,
) -> HRESULT {
    if stream.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a stream from `XDspActivate`, the title's command; a shared
    // borrow avoids aliasing the `Status` block polled on other threads.
    let (stream, command) = unsafe { (&*stream, &*command) };
    let mut last = stream.submit.lock().unwrap_or_else(|e| e.into_inner());
    let next = last.wrapping_add(1);
    *last = next;
    stream.status.submitted.store(next, Ordering::Release);
    if !command.output.is_null() {
        let samples = (stream.frames * stream.channels) as usize;
        // SAFETY: the title's output buffer holds one block of the stream.
        unsafe { std::ptr::write_bytes(command.output, 0, samples) };
    }
    stream.status.result.store(S_OK, Ordering::Relaxed);
    // Release-pairs with title threads polling `completed` so the zeroed
    // output block and `result` are visible before `next` is observed.
    stream.status.completed.store(next, Ordering::Release);
    drop(last);
    if !ticket.is_null() {
        // SAFETY: optional out parameter.
        unsafe { ticket.write(next) };
    }
    S_OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "dsp",
        dlls: &["chad.dll"],
        exports: vec![
            hle_export!(XDspConnectWithMaximumStreamLimit),
            hle_export!(XDspDisconnect),
            hle_export!(XDspActivate),
            hle_export!(XDspDeactivate),
            hle_export!(XDspSubmitCommand),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolution_stream_as_the_title_drives_it() {
        let mut base = vec![0u8; 0x10_0000];
        let mut dsp = std::ptr::null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                0x10_0000,
                24,
                0x80,
                1,
                &mut dsp
            ),
            S_OK
        );
        let ir = vec![0f32; 0x800];
        // Stereo convolution with option 2, as the title asks for.
        let desc = StreamDesc {
            kind: CONVOLUTION,
            options: 2,
            frames: 0x400,
            channels: 2,
            ir_floats: 0x800,
            ir: ir.as_ptr(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        let mut output = vec![1f32; 0x800];
        let input = vec![0.5f32; 0x800];
        let command = Command {
            gains: [1.0, 1.0],
            input: input.as_ptr(),
            output: output.as_mut_ptr(),
            reserved: 0,
        };
        let mut ticket = 0;
        assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
        // SAFETY: the stream's status block.
        let (completed, result) = unsafe {
            (
                (*status).completed.load(Ordering::Acquire),
                (*status).result.load(Ordering::Acquire),
            )
        };
        assert_eq!((completed, result), (ticket, S_OK));
        assert!(output.iter().all(|&s| s == 0.0));
        assert_eq!(XDspDeactivate(stream), S_OK);

        // Mono streams refuse stereo options; transforms are mono only.
        let bad = StreamDesc {
            channels: 1,
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &bad, &mut status, &mut stream),
            E_INVALIDARG
        );
        let transform = StreamDesc {
            kind: 2,
            options: 0,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
            ..desc
        };
        assert_eq!(
            XDspActivate(dsp, &transform, &mut status, &mut stream),
            S_OK
        );
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
    }

    #[test]
    fn concurrent_submit_commands_issue_unique_monotonic_tickets() {
        use std::sync::Barrier;

        const THREADS: usize = 8;
        const ITERS: usize = 4_000;
        const TOTAL: u32 = (THREADS * ITERS) as u32;

        let mut base = vec![0u8; 0x10_0000];
        let mut dsp = std::ptr::null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                0x10_0000,
                24,
                4,
                2,
                &mut dsp
            ),
            S_OK
        );
        let desc = StreamDesc {
            kind: 2,
            options: 0,
            frames: 0x200,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);

        let stream_addr = stream as usize;
        let start = Barrier::new(THREADS);
        let mut all_tickets = Vec::with_capacity(TOTAL as usize);
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(THREADS);
            for _ in 0..THREADS {
                let start = &start;
                handles.push(scope.spawn(move || {
                    let command = Command {
                        gains: [1.0, 1.0],
                        input: std::ptr::null(),
                        output: std::ptr::null_mut(),
                        reserved: 0,
                    };
                    let mut tickets = Vec::with_capacity(ITERS);
                    start.wait();
                    for _ in 0..ITERS {
                        let mut ticket = 0;
                        let hr =
                            XDspSubmitCommand(stream_addr as *mut Stream, &command, &mut ticket);
                        assert_eq!(hr, S_OK);
                        tickets.push(ticket);
                    }
                    tickets
                }));
            }
            for h in handles {
                all_tickets.extend(h.join().unwrap());
            }
        });

        // SAFETY: `status` stays alive until `XDspDeactivate` below.
        let status = unsafe { &*status };
        assert_eq!(status.submitted.load(Ordering::Acquire), TOTAL);
        assert_eq!(status.completed.load(Ordering::Acquire), TOTAL);
        assert_eq!(status.result.load(Ordering::Acquire), S_OK);

        all_tickets.sort_unstable();
        let expected: Vec<u32> = (1..=TOTAL).collect();
        assert_eq!(all_tickets, expected);

        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
    }

    #[test]
    fn status_poller_observes_monotonic_completion_and_zeroed_output() {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;

        const SUBMITTERS: usize = 4;
        const ITERS: usize = 2_000;
        const TOTAL: u32 = (SUBMITTERS * ITERS) as u32;
        const FRAMES: usize = 0x200;

        let mut base = vec![0u8; 0x10_0000];
        let mut dsp = std::ptr::null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                0x10_0000,
                24,
                4,
                2,
                &mut dsp
            ),
            S_OK
        );
        let desc = StreamDesc {
            kind: 2,
            options: 0,
            frames: FRAMES as u32,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
        };

        // Part 1: multiple submitters + concurrent status poller must observe
        // monotonic `completed`, `completed <= submitted`, and `result == S_OK`.
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        let stream_addr = stream as usize;
        let status_addr = status as usize;
        let start = Barrier::new(SUBMITTERS + 1);
        let finished = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let start_ref = &start;
            let finished_ref = &finished;
            let poller = scope.spawn(move || {
                // SAFETY: `status` stays alive until `XDspDeactivate` after this scope joins.
                let status = unsafe { &*(status_addr as *const Status) };
                let mut prev = 0u32;
                start_ref.wait();
                loop {
                    let c = status.completed.load(Ordering::Acquire);
                    let r = status.result.load(Ordering::Relaxed);
                    let s = status.submitted.load(Ordering::Acquire);
                    assert!(c >= prev, "completed regressed from {prev} to {c}");
                    assert!(s >= c, "completed {c} exceeded submitted {s}");
                    assert_eq!(r, S_OK);
                    prev = c;
                    if finished_ref.load(Ordering::Acquire) == SUBMITTERS {
                        let c = status.completed.load(Ordering::Acquire);
                        let s = status.submitted.load(Ordering::Acquire);
                        assert!(c >= prev, "completed regressed from {prev} to {c}");
                        assert_eq!(c, TOTAL, "lost completed tickets");
                        assert_eq!(s, TOTAL, "lost submitted tickets");
                        break;
                    }
                    std::hint::spin_loop();
                }
            });
            for _ in 0..SUBMITTERS {
                let start_ref = &start;
                let finished_ref = &finished;
                scope.spawn(move || {
                    let mut output = vec![1.0f32; FRAMES];
                    start_ref.wait();
                    for _ in 0..ITERS {
                        output.fill(1.0);
                        let command = Command {
                            gains: [1.0, 1.0],
                            input: std::ptr::null(),
                            output: output.as_mut_ptr(),
                            reserved: 0,
                        };
                        let mut ticket = 0;
                        assert_eq!(
                            XDspSubmitCommand(stream_addr as *mut Stream, &command, &mut ticket),
                            S_OK
                        );
                        assert!(output.iter().all(|&s| s == 0.0));
                    }
                    finished_ref.fetch_add(1, Ordering::Release);
                });
            }
            poller.join().unwrap();
        });
        assert_eq!(XDspDeactivate(stream), S_OK);

        // Part 2: single submitter + cross-thread poller synchronized solely via
        // `status.completed` (Acquire/Release) observes every output buffer zeroed.
        const BLOCKS: usize = 256;
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        let mut buffers: Vec<Vec<f32>> = (0..BLOCKS).map(|_| vec![1.0f32; FRAMES]).collect();
        let ptrs: Vec<usize> = buffers
            .iter_mut()
            .map(|b| b.as_mut_ptr() as usize)
            .collect();
        let stream_addr = stream as usize;
        let status_addr = status as usize;
        let pair_start = Barrier::new(2);
        std::thread::scope(|scope| {
            let pair_start = &pair_start;
            let ptrs = &ptrs;
            let submitter = scope.spawn(move || {
                pair_start.wait();
                for (i, &buf) in ptrs.iter().enumerate() {
                    let command = Command {
                        gains: [1.0, 1.0],
                        input: std::ptr::null(),
                        output: buf as *mut f32,
                        reserved: 0,
                    };
                    let mut ticket = 0;
                    assert_eq!(
                        XDspSubmitCommand(stream_addr as *mut Stream, &command, &mut ticket),
                        S_OK
                    );
                    assert_eq!(ticket, (i + 1) as u32);
                }
            });
            // SAFETY: `status` stays alive until `XDspDeactivate` after this scope joins.
            let status = unsafe { &*(status_addr as *const Status) };
            pair_start.wait();
            let mut checked = 0u32;
            while checked < BLOCKS as u32 {
                let c = status.completed.load(Ordering::Acquire);
                assert!(c >= checked, "completed regressed from {checked} to {c}");
                assert_eq!(status.result.load(Ordering::Relaxed), S_OK);
                while checked < c {
                    let idx = checked as usize;
                    // SAFETY: submitter stopped writing `buffers[idx]` before publishing `c`.
                    let slice =
                        unsafe { std::slice::from_raw_parts(ptrs[idx] as *const f32, FRAMES) };
                    assert!(
                        slice.iter().all(|&s| s == 0.0),
                        "ticket {} output not zeroed when completed reached {c}",
                        idx + 1
                    );
                    checked += 1;
                }
                std::hint::spin_loop();
            }
            submitter.join().unwrap();
        });
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
    }

    #[test]
    fn status_abi_layout_and_ticket_wraparound() {
        assert_eq!(size_of::<Status>(), 16);
        assert_eq!(align_of::<Status>(), 4);
        assert_eq!(std::mem::offset_of!(Status, submitted), 0);
        assert_eq!(std::mem::offset_of!(Status, completed), 4);
        assert_eq!(std::mem::offset_of!(Status, reserved), 8);
        assert_eq!(std::mem::offset_of!(Status, result), 12);

        let mut base = vec![0u8; 0x10_0000];
        let mut dsp = std::ptr::null_mut();
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                0x10_0000,
                24,
                4,
                2,
                &mut dsp
            ),
            S_OK
        );
        let desc = StreamDesc {
            kind: 2,
            options: 0,
            frames: 0x200,
            channels: 1,
            ir_floats: 0,
            ir: std::ptr::null(),
        };
        let (mut status, mut stream) = (std::ptr::null(), std::ptr::null_mut());
        assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
        // SAFETY: our active stream, no other thread accesses it here.
        *unsafe { &*stream }.submit.lock().unwrap() = u32::MAX - 1;
        let command = Command {
            gains: [1.0, 1.0],
            input: std::ptr::null(),
            output: std::ptr::null_mut(),
            reserved: 0,
        };
        // SAFETY: `status` stays alive until `XDspDeactivate` below.
        let status = unsafe { &*status };
        for expected in [u32::MAX, 0, 1] {
            let mut ticket = 123;
            assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
            assert_eq!(ticket, expected);
            assert_eq!(status.submitted.load(Ordering::Acquire), expected);
            assert_eq!(status.completed.load(Ordering::Acquire), expected);
            assert_eq!(status.result.load(Ordering::Acquire), S_OK);
        }
        assert_eq!(XDspDeactivate(stream), S_OK);
        assert_eq!(XDspDisconnect(dsp), S_OK);
    }
}
