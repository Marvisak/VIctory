//! DirectStorage on the console (`XCRDAPI.dll`): memory-sourced requests,
//! zlib, completion order, request reuse and automatic submission.
//!
//! Public reference: reference/system/dstorage (DSTORAGE_REQUEST,
//! DSTORAGE_REQUEST_SOURCE_TYPE, DSTORAGE_PRIORITY) and the DirectStorage
//! overview (_content/gc/system/overviews/directstorage). Quoted there:
//! - "all requests are completed in order"; "Notifications aren't signaled
//!   until all previous enqueued requests on the queue have completed."
//! - "If a read causes an error, that read is marked as completed. Future
//!   notifications in the queue aren't blocked from signaling."
//! - "The structure can be reused as soon as `EnqueueRequest` returns."
//! - an Enqueue that "filled more than half of the queue's capacity since
//!   last submission ... triggers an automatic submission."
//! - "Memory-sourced queues must be created with real-time priority."
//!
//! The factory's interface id, the method slots and the request option bits
//! are VIctory's (`crates/victory-hle/src/modules/dstorage.rs`).

use std::ffi::c_void;

use crate::rt::{Guid, Hresult, S_OK};
use crate::tap::Tap;

const FACTORY: Guid = Guid(
    0x6295_9d11,
    0xd45f,
    0x4884,
    [0xa0, 0xa3, 0xf7, 0x86, 0x2e, 0xa2, 0x43, 0xae],
);
const E_PENDING: Hresult = 0x8000_000A_u32 as i32;

const SOURCE_FILE: u64 = 0;
const SOURCE_MEMORY: u64 = 1;
const PRIORITY_NORMAL: i8 = 0;
const PRIORITY_REALTIME: i8 = 2;
/// Request options: inflate zlib; the source is an address.
const ZLIB: u64 = 1;
const MEMORY_SOURCE: u64 = 1 << 22;

#[link(name = "xcrdapi", kind = "raw-dylib")]
unsafe extern "system" {
    fn DStorageGetFactory(iid: *const Guid, out: *mut *mut c_void) -> Hresult;
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn Sleep(ms: u32);
}

#[repr(C)]
struct QueueDesc {
    source_type: u64,
    capacity: u16,
    priority: i8,
    name: *const u8,
    unknown: u64,
}

/// DSTORAGE_REQUEST as VIctory lays it out (0x48 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct Request {
    options: u64,
    destination: *mut c_void,
    page_offset: u64,
    destination_size: u32,
    source: *const c_void,
    offset: u64,
    source_size: u32,
    intermediate_size: u32,
    tag: u64,
    name: *const u8,
}

impl Request {
    fn memory(options: u64, source: &[u8], destination: &mut [u8]) -> Request {
        Request {
            options: options | MEMORY_SOURCE,
            destination: destination.as_mut_ptr().cast(),
            page_offset: 0,
            destination_size: destination.len() as u32,
            source: source.as_ptr().cast(),
            offset: 0,
            source_size: source.len() as u32,
            intermediate_size: 0,
            tag: 0,
            name: std::ptr::null(),
        }
    }
}

/// A COM-style object: method table first.
#[derive(Clone, Copy)]
struct Com(*mut c_void);

impl Com {
    /// # Safety
    /// `F` is the slot's signature.
    unsafe fn method<F: Copy>(self, offset: usize) -> F {
        // SAFETY: (caller) the object's first word is its method table.
        unsafe {
            std::mem::transmute_copy(&*(*self.0.cast::<*const *const c_void>()).add(offset / 8))
        }
    }
    fn release(self) {
        // SAFETY: Release.
        let f: extern "system" fn(*mut c_void) -> u32 = unsafe { self.method(0x10) };
        f(self.0);
    }
}

struct Storage {
    factory: Com,
}

impl Storage {
    fn status_array(&self, capacity: u32) -> Com {
        let mut out = std::ptr::null_mut();
        // SAFETY: (all methods) the slot's signature in VIctory's table.
        let f: extern "system" fn(
            *mut c_void,
            u32,
            *const u8,
            *const Guid,
            *mut *mut c_void,
        ) -> Hresult = unsafe { self.factory.method(0x28) };
        f(
            self.factory.0,
            capacity,
            c"conformance".as_ptr().cast(),
            std::ptr::null(),
            &mut out,
        );
        Com(out)
    }
    fn queue(&self, capacity: u16) -> Com {
        self.queue_of(SOURCE_MEMORY, PRIORITY_REALTIME, capacity)
    }
    fn queue_of(&self, source_type: u64, priority: i8, capacity: u16) -> Com {
        let desc = QueueDesc {
            source_type,
            capacity,
            priority,
            name: c"conformance".as_ptr().cast(),
            unknown: 0,
        };
        let mut out = std::ptr::null_mut();
        let f: extern "system" fn(
            *mut c_void,
            *const QueueDesc,
            *const Guid,
            *mut *mut c_void,
        ) -> Hresult = unsafe { self.factory.method(0x50) };
        f(self.factory.0, &desc, std::ptr::null(), &mut out);
        Com(out)
    }
}

impl Storage {
    /// IDStorageFactoryX::OpenFile.
    fn open(&self, path: &std::path::Path) -> Result<Com, Hresult> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut out = std::ptr::null_mut();
        // SAFETY: the slot's signature in VIctory's table.
        let f: extern "system" fn(
            *mut c_void,
            *const u16,
            *const Guid,
            *mut *mut c_void,
        ) -> Hresult = unsafe { self.factory.method(0x20) };
        match f(self.factory.0, wide.as_ptr(), std::ptr::null(), &mut out) {
            S_OK if !out.is_null() => Ok(Com(out)),
            S_OK => Err(-1),
            hr => Err(hr),
        }
    }
}

/// A request reading `size` bytes of `file` at `offset`.
fn from_file(options: u64, file: Com, offset: u64, size: usize, destination: &mut [u8]) -> Request {
    Request {
        options,
        destination: destination.as_mut_ptr().cast(),
        page_offset: 0,
        destination_size: destination.len() as u32,
        source: file.0,
        offset,
        source_size: size as u32,
        intermediate_size: 0,
        tag: 0,
        name: std::ptr::null(),
    }
}

/// File sources: raw reads at an offset, zlib at a 16-byte aligned offset
/// (the overview: FileOffset must be 16-byte aligned for compressed data),
/// and a missing file refused.
fn files(t: &mut Tap, s: &Storage, array: Com) {
    let plain = pattern(300_000, 7);
    let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
    let mut contents = plain.clone();
    contents.resize(contents.len().next_multiple_of(16), 0);
    let zlib_at = contents.len() as u64;
    contents.extend_from_slice(&packed);
    let path = std::env::temp_dir().join(format!("victory-conformance-{}.bin", std::process::id()));
    if let Err(e) = std::fs::write(&path, &contents) {
        t.check(false, "dstorage: a test file is written", || e.to_string());
        return;
    }
    let file = match s.open(&path) {
        Ok(f) => f,
        Err(hr) => {
            t.check(
                false,
                "dstorage: IDStorageFactoryX::OpenFile opens a file",
                || format!("{hr:#010x}"),
            );
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    t.check(
        true,
        "dstorage: IDStorageFactoryX::OpenFile opens a file",
        String::new,
    );
    let q = s.queue_of(SOURCE_FILE, PRIORITY_NORMAL, 32);
    let mut raw = vec![0u8; 1000];
    let mut inflated = vec![0u8; plain.len()];
    enqueue(q, &from_file(0, file, 5000, 1000, &mut raw));
    enqueue(
        q,
        &from_file(ZLIB, file, zlib_at, packed.len(), &mut inflated),
    );
    enqueue_status(q, array, 13);
    submit(q);
    t.eq(
        wait(array, 13, 5000),
        S_OK,
        "dstorage: file-sourced requests complete",
    );
    t.check(
        raw == plain[5000..6000],
        "dstorage: a raw file read returns the bytes at its offset",
        || "bytes differ".into(),
    );
    t.check(
        inflated == plain,
        "dstorage: a zlib file read inflates to the original bytes",
        || "bytes differ".into(),
    );
    close(q);
    q.release();
    file.release();
    let _ = std::fs::remove_file(&path);
    let missing = std::env::temp_dir().join("victory-conformance-no-such-file.bin");
    t.check(
        s.open(&missing).is_err(),
        "dstorage: opening a missing file fails",
        || "opened".into(),
    );
}

fn enqueue(q: Com, r: &Request) {
    let f: extern "system" fn(*mut c_void, *const Request) = unsafe { q.method(0x18) };
    f(q.0, r)
}
fn enqueue_status(q: Com, array: Com, index: u32) {
    let f: extern "system" fn(*mut c_void, *mut c_void, u32) = unsafe { q.method(0x20) };
    f(q.0, array.0, index)
}
fn submit(q: Com) {
    let f: extern "system" fn(*mut c_void) = unsafe { q.method(0x30) };
    f(q.0)
}
fn close(q: Com) {
    let f: extern "system" fn(*mut c_void) = unsafe { q.method(0x40) };
    f(q.0)
}
fn status(array: Com, index: u32) -> Hresult {
    let f: extern "system" fn(*mut c_void, u32) -> Hresult = unsafe { array.method(0x20) };
    f(array.0, index)
}

/// Waits up to `ms` for status entry `index`; its result.
fn wait(array: Com, index: u32, ms: u32) -> Hresult {
    for _ in 0..ms {
        let hr = status(array, index);
        if hr != E_PENDING {
            return hr;
        }
        // SAFETY: a plain call.
        unsafe { Sleep(1) };
    }
    status(array, index)
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..n)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            // Runs of repeated bytes, so zlib has something to find.
            if i % 64 < 48 { (i / 64) as u8 } else { x as u8 }
        })
        .collect()
}

pub fn run(t: &mut Tap) {
    let mut out = std::ptr::null_mut();
    // SAFETY: a plain call.
    let hr = unsafe { DStorageGetFactory(&FACTORY, &mut out) };
    t.check(
        hr == S_OK && !out.is_null(),
        "dstorage: DStorageGetFactory gives the factory",
        || format!("{hr:#010x}"),
    );
    if out.is_null() {
        return;
    }
    let s = Storage { factory: Com(out) };
    let array = s.status_array(16);
    let q = s.queue(64);

    // A memory-to-memory copy and a zlib inflate.
    let raw = pattern(1 << 20, 1);
    let mut copied = vec![0u8; raw.len()];
    enqueue(q, &Request::memory(0, &raw, &mut copied));
    let plain = pattern(256 << 10, 2);
    let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
    let mut inflated = vec![0u8; plain.len()];
    enqueue(q, &Request::memory(ZLIB, &packed, &mut inflated));
    enqueue_status(q, array, 0);
    submit(q);
    t.eq(
        wait(array, 0, 5000),
        S_OK,
        "dstorage: memory-sourced requests complete",
    );
    t.check(
        copied == raw,
        "dstorage: a raw request copies its bytes",
        || "bytes differ".into(),
    );
    t.check(
        inflated == plain,
        "dstorage: a zlib request inflates to the original bytes",
        || "bytes differ".into(),
    );

    // Completion in order: a status is not signaled before every request
    // enqueued before it has completed, whatever their sizes.
    let sources: Vec<Vec<u8>> = (0..8)
        .map(|i| pattern(if i % 2 == 0 { 4 << 20 } else { 4 << 10 }, 10 + i))
        .collect();
    let mut destinations: Vec<Vec<u8>> = sources.iter().map(|s| vec![0u8; s.len()]).collect();
    for (i, (src, dst)) in sources.iter().zip(destinations.iter_mut()).enumerate() {
        enqueue(q, &Request::memory(0, src, dst));
        enqueue_status(q, array, 1 + i as u32);
    }
    submit(q);
    let mut in_order = true;
    for _ in 0..5000 {
        let done: Vec<bool> = (1..9).map(|i| status(array, i) != E_PENDING).collect();
        // Done entries form a prefix.
        in_order &= done.windows(2).all(|w| w[0] || !w[1]);
        if done.iter().all(|&d| d) {
            break;
        }
        // SAFETY: a plain call.
        unsafe { Sleep(1) };
    }
    t.check(
        in_order,
        "dstorage: status entries are signaled in queue order",
        || "a later entry was signaled first".into(),
    );
    t.check(
        (1..9).all(|i| status(array, i) == S_OK),
        "dstorage: ... and all succeed",
        || "not all S_OK".into(),
    );
    t.check(
        sources == destinations,
        "dstorage: every ordered request wrote its bytes",
        || "bytes differ".into(),
    );

    // A failed read does not block later notifications.
    let broken = vec![0x78, 0x9c, 0xff, 0xff, 0xff, 0xff];
    let mut sink = vec![0u8; 4096];
    enqueue(q, &Request::memory(ZLIB, &broken, &mut sink));
    enqueue_status(q, array, 9);
    let after = pattern(64 << 10, 3);
    let mut after_copy = vec![0u8; after.len()];
    enqueue(q, &Request::memory(0, &after, &mut after_copy));
    enqueue_status(q, array, 10);
    submit(q);
    t.check(
        wait(array, 9, 5000) != E_PENDING,
        "dstorage: a failed read is marked completed",
        || "still pending".into(),
    );
    t.eq(
        wait(array, 10, 5000),
        S_OK,
        "dstorage: a notification after a failed read still signals",
    );
    t.check(
        after_copy == after,
        "dstorage: ... and the read after it wrote its bytes",
        || "bytes differ".into(),
    );

    // The request structure can be reused as soon as EnqueueRequest returns.
    let first = pattern(32 << 10, 4);
    let second = pattern(32 << 10, 5);
    let (mut to_first, mut to_second) = (vec![0u8; first.len()], vec![0u8; second.len()]);
    let mut r = Request::memory(0, &first, &mut to_first);
    enqueue(q, &r);
    r = Request::memory(0, &second, &mut to_second);
    enqueue(q, &r);
    enqueue_status(q, array, 11);
    submit(q);
    t.eq(
        wait(array, 11, 5000),
        S_OK,
        "dstorage: requests from one reused structure complete",
    );
    t.check(
        to_first == first && to_second == second,
        "dstorage: a reused request structure does not change the enqueued request",
        || "bytes differ".into(),
    );
    close(q);

    // Automatic submission once more than half the capacity is enqueued.
    let small = s.queue(8);
    let bytes = pattern(4096, 6);
    let mut sinks = vec![vec![0u8; 4096]; 5];
    for sink in sinks.iter_mut().take(3) {
        enqueue(small, &Request::memory(0, &bytes, sink));
    }
    // Four of eight entries: half, not more than half.
    enqueue_status(small, array, 12);
    t.eq(
        wait(array, 12, 300),
        E_PENDING,
        "dstorage: a queue filled to half its capacity is not submitted on its own",
    );
    enqueue(small, &Request::memory(0, &bytes, &mut sinks[3]));
    t.eq(
        wait(array, 12, 5000),
        S_OK,
        "dstorage: filling more than half the capacity submits on its own",
    );
    submit(small);
    close(small);
    small.release();
    q.release();
    files(t, &s, array);
    array.release();
}

/// The streaming workload: memory-sourced requests through one real-time
/// queue, raw and zlib, in the chunk sizes the DirectStorage overview
/// recommends (64 KiB to 512 KiB). A report of throughput per kind; the console's own figures are 2.0 GB/s raw (its
/// minimum) and 4.8 GB/s decompressed at 2:1.
pub fn bench() -> String {
    let mut out = std::ptr::null_mut();
    // SAFETY: a plain call.
    if unsafe { DStorageGetFactory(&FACTORY, &mut out) } != S_OK || out.is_null() {
        return "no factory\n".into();
    }
    let s = Storage { factory: Com(out) };
    let array = s.status_array(4);
    let q = s.queue(1024);
    let mut report = String::new();
    for (kind, chunk, total) in [
        ("raw", 64usize << 10, 512usize << 20),
        ("raw", 512 << 10, 512 << 20),
        ("zlib", 64 << 10, 256 << 20),
        ("zlib", 512 << 10, 256 << 20),
    ] {
        let plain = pattern(chunk, chunk as u32);
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
        let (source, options) = if kind == "zlib" {
            (&packed, ZLIB)
        } else {
            (&plain, 0)
        };
        // A ring of destinations, as a streaming title reuses its buffers.
        let mut ring: Vec<Vec<u8>> = (0..64).map(|_| vec![0u8; chunk]).collect();
        let n = total / chunk;
        let started = std::time::Instant::now();
        for i in 0..n {
            enqueue(q, &Request::memory(options, source, &mut ring[i % 64]));
        }
        enqueue_status(q, array, 0);
        submit(q);
        let hr = wait(array, 0, 120_000);
        let seconds = started.elapsed().as_secs_f64();
        let ok = hr == S_OK && ring[0] == plain;
        report.push_str(&format!(
            "{kind:>4} {:>4} KiB chunks: {:>6.2} GB/s out, {:>6.2} GB/s in, {:>8.0} requests/s{}\n",
            chunk >> 10,
            total as f64 / seconds / 1e9,
            (n * source.len()) as f64 / seconds / 1e9,
            n as f64 / seconds,
            if ok { "" } else { "  (FAILED)" },
        ));
    }
    close(q);
    q.release();
    array.release();
    report
}
