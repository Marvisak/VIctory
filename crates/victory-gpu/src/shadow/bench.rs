//! A compute-heavy submission through the copies' real paths ([`find`],
//! [`Shadow::refresh`], [`Shadow::gpu_writes`], `HostList`,
//! `Host::execute`), with the copies in system memory, the read-only ones in
//! video memory, all of them in video memory, and no copies (title memory
//! in place, `crate::in_place`): what each costs the GPU and the CPU.
//!
//! `cargo test --release -p victory-gpu --lib compute_heavy -- --ignored --nocapture`
//!
//! `VICTORY_TEST_ADAPTER=<part of a GPU's name>` runs it on another GPU of
//! the machine (an integrated one). `VICTORY_TEST_BENCH_LOOSE=1` makes the
//! title memory no buffer's: every access goes through the copies of its
//! 4 MiB windows ([`loose`]), placed as written buffers' copies are.

use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

const MIB: usize = 1 << 20;
/// Title buffers made without unordered access, which shaders read.
const READ_ONLY: [usize; 4] = [64 * MIB; 4];
/// Title buffers shaders write: the first [`WRITTEN_PART`] bytes, in 1 MiB
/// slices; the rest is data the title writes and shaders read.
const WRITTEN: [usize; 2] = [32 * MIB; 2];
const WRITTEN_PART: usize = 24 * MIB;
const DISPATCHES: usize = 200;
const LISTS: usize = 8;
/// Per dispatch: bytes read from a read-only buffer, read and written in a
/// written buffer, and read from a written buffer's title part.
const READ: usize = 4 * MIB;
const READ_WRITTEN: usize = MIB;
const TAIL: usize = 256 << 10;
/// Threads per dispatch: four dwords each read and written.
const THREADS: usize = READ_WRITTEN / 16;
/// Title writes between submissions, in whole pages: into the read-only
/// buffers, and into each written buffer's title part.
const TITLE_READ_ONLY: usize = 12 * MIB;
const TITLE_WRITTEN: usize = 2 * MIB;

const SRC: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), SRV(t1), UAV(u0), UAV(u1)"
cbuffer C : register(b0) { uint n; uint mark_base; uint mark_phase; uint marked; };
ByteAddressBuffer src : register(t0);
ByteAddressBuffer tail : register(t1);
RWByteAddressBuffer rw : register(u0);
RWByteAddressBuffer marks : register(u1);
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint s = 0;
    for (uint k = 0; k < 16; k++) s += src.Load((k * n + id.x) * 4);
    s += tail.Load((id.x % 65536) * 4);
    for (uint j = 0; j < 4; j++) {
        uint at = (j * n + id.x) * 4;
        rw.Store(at, rw.Load(at) + s + 1);
        if (marked != 0) {
            // As translated shaders mark a store's page (`victory_shader`
            // "Write marks"): set where still clear.
            uint bit = mark_base + ((mark_phase + at) >> 16);
            uint word = (bit / 32) * 4, mask = 1u << (bit % 32);
            if ((marks.Load(word) & mask) == 0) marks.InterlockedOr(word, mask);
        }
    }
}"#;

fn pipeline(device: &ID3D12Device) -> (ID3D12RootSignature, ID3D12PipelineState) {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source outlives the call.
    let compiled = unsafe {
        D3DCompile(
            SRC.as_ptr().cast(),
            SRC.len(),
            s!("bench"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if compiled.is_err() {
        // SAFETY: the error blob's bytes.
        let text = errors.map(|e| unsafe {
            String::from_utf8_lossy(std::slice::from_raw_parts(
                e.GetBufferPointer().cast::<u8>(),
                e.GetBufferSize(),
            ))
            .into_owned()
        });
        panic!("{text:?}");
    }
    let code = code.unwrap();
    // SAFETY: the blob's bytes; plain creation.
    unsafe {
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).unwrap();
        let pso: ID3D12PipelineState = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })
            .unwrap();
        (root, pso)
    }
}

fn heap_type(copy: &Shadow) -> D3D12_HEAP_TYPE {
    let mut props = D3D12_HEAP_PROPERTIES::default();
    // SAFETY: a live committed buffer.
    unsafe {
        copy.buffer
            .GetHeapProperties(Some(&mut props), None)
            .unwrap()
    };
    props.Type
}

/// A title buffer: its memory (write-watched, as the runtime's pools) and
/// copy.
struct Buffer {
    title: usize,
    len: usize,
    copy: Arc<Shadow>,
}

impl Buffer {
    fn new(len: usize, read_only: bool) -> Buffer {
        let title = crate::watch::tests::pool(len / PAGE);
        if loose_only() {
            let copy = find(title as u64, len as u64).expect("a window's copy");
            return Buffer { title, len, copy };
        }
        if read_only {
            register_read_only(title, title as u64, len as u64);
        } else {
            register(title, title as u64, len as u64);
        }
        let copy = find(title as u64, len as u64).expect("a copy");
        Buffer { title, len, copy }
    }

    /// The title writes a dword into the page at `page` of the buffer.
    fn title_write(&self, page: usize, value: u32) {
        assert!(page < self.len / PAGE);
        // SAFETY: a dword of our pool.
        unsafe { ((self.title + page * PAGE + 4 * (page % 64)) as *mut u32).write_volatile(value) };
    }
}

/// `VICTORY_TEST_BENCH_LOOSE`: the title memory is no buffer's.
fn loose_only() -> bool {
    crate::switch::on("VICTORY_TEST_BENCH_LOOSE")
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if loose_only() {
            // Windows' copies are never freed: memory made again at the same
            // address would find the last mode's. Kept: each mode its own.
            return;
        }
        unregister(self.title, self.title as u64);
        crate::watch::tests::free(self.title);
    }
}

/// What one submission cost, in ms: from `Host::execute`'s start to its
/// end, and of that preparing (refreshes, snapshots), waiting for the GPU,
/// and copying back.
#[derive(Clone, Copy, Default)]
struct Cost {
    total: f64,
    prepare: f64,
    gpu: f64,
    back: f64,
}

/// One submission of [`DISPATCHES`] dispatches in [`LISTS`] lists; with
/// `round`, the title writes first.
fn submit(
    host: &crate::host::Host,
    (root, pso): &(ID3D12RootSignature, ID3D12PipelineState),
    read_only: &[Buffer],
    written: &[Buffer],
    marked: bool,
    round: Option<u32>,
) -> Cost {
    let bitmap = bitmap(false).expect("the written-pages bitmap");
    let lists: Vec<_> = (0..LISTS).map(|_| host.new_list().unwrap()).collect();
    for d in 0..DISPATCHES {
        let list = &lists[d * LISTS / DISPATCHES];
        let src = &read_only[d % read_only.len()];
        let src_at = src.title + (d / read_only.len()) % (src.len / READ) * READ;
        let rw = &written[d % written.len()];
        let rw_at =
            rw.title + (d / written.len()) * 7 % (WRITTEN_PART / READ_WRITTEN) * READ_WRITTEN;
        // A buffer in place has no marks: nothing to mark, nothing to copy back.
        let marked = marked && rw.copy.marked();
        let tail_at =
            rw.title + WRITTEN_PART + (d / written.len()) % ((rw.len - WRITTEN_PART) / TAIL) * TAIL;
        // As a translated dispatch binds its views: what it reads brought up
        // to date, what it writes declared.
        src.copy.refresh(list, src_at as u64, READ as u64);
        rw.copy.refresh(list, tail_at as u64, TAIL as u64);
        rw.copy.refresh(list, rw_at as u64, READ_WRITTEN as u64);
        rw.copy
            .gpu_writes(list, rw_at as u64, READ_WRITTEN as u64, marked);
        let (base, phase) = rw.copy.mark_entry(rw_at as u64).unwrap_or((0, 0));
        // SAFETY: an open list, live objects.
        unsafe {
            let l = &list.list;
            l.SetComputeRootSignature(root);
            l.SetPipelineState(pso);
            for (i, v) in [THREADS as u32, base, phase, u32::from(marked)]
                .into_iter()
                .enumerate()
            {
                l.SetComputeRoot32BitConstant(0, v, i as u32);
            }
            l.SetComputeRootShaderResourceView(1, src.copy.gpu(src_at as u64));
            l.SetComputeRootShaderResourceView(2, rw.copy.gpu(tail_at as u64));
            l.SetComputeRootUnorderedAccessView(3, rw.copy.gpu(rw_at as u64));
            l.SetComputeRootUnorderedAccessView(4, bitmap.gpu);
            l.Dispatch((THREADS / 256) as u32, 1, 1);
            l.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                ..Default::default()
            }]);
        }
    }
    for list in &lists {
        list.close();
    }
    if let Some(round) = round {
        // Constants and streamed data the title writes between frames.
        let per = TITLE_READ_ONLY / PAGE / read_only.len();
        for (b, buffer) in read_only.iter().enumerate() {
            for j in 0..per {
                let page =
                    (round as usize * 7 + j * (buffer.len / PAGE / per)) % (buffer.len / PAGE);
                buffer.title_write(page, round * 1000 + (b * per + j) as u32);
            }
        }
        let tail_pages = (WRITTEN[0] - WRITTEN_PART) / PAGE;
        for buffer in written {
            for j in 0..TITLE_WRITTEN / PAGE {
                let page = WRITTEN_PART / PAGE + (round as usize * 5 + j * 4) % tail_pages;
                buffer.title_write(page, round + j as u32);
            }
        }
    }
    let refs: Vec<&HostList> = lists.iter().collect();
    crate::host::take_submission_us();
    let t = std::time::Instant::now();
    host.execute(&refs);
    let total = t.elapsed().as_secs_f64() * 1e3;
    let [prepare, gpu, back] = crate::host::take_submission_us().map(|us| us as f64 / 1e3);
    Cost {
        total,
        prepare,
        gpu,
        back,
    }
}

/// The median of each part over `costs`.
fn median(costs: &mut [Cost]) -> Cost {
    let part = |f: fn(&Cost) -> f64| {
        let mut v: Vec<f64> = costs.iter().map(f).collect();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    Cost {
        total: part(|c| c.total),
        prepare: part(|c| c.prepare),
        gpu: part(|c| c.gpu),
        back: part(|c| c.back),
    }
}

/// The configurations, as (label, `VICTORY_GPU_UPLOAD`,
/// `VICTORY_GPU_UPLOAD_WRITES`, `VICTORY_IN_PLACE`).
const CONFIGS: [(&str, &str, &str, &str); 4] = [
    ("system memory", "0", "0", "0"),
    ("read-only copies in video memory", "1", "0", "0"),
    ("all copies in video memory", "1", "1", "0"),
    ("no copies: title memory in place", "0", "0", "1"),
];

const CHILD: &str = "VICTORY_TEST_BENCH_CHILD";

/// Many dispatches reading and writing ranges of several title buffers'
/// copies (320 MiB in all), one submission at a time, in each placement
/// (a child process each: the switches are read once per process). Write
/// marks or snapshots; the title's data unchanged between submissions, or
/// the title writing 16 MiB of it. Prints the median of 5 submissions
/// after 2 warm-ups, and checks every placement leaves title memory the
/// same.
#[test]
#[ignore = "benchmark: run in release with --ignored --nocapture"]
fn compute_heavy_submissions_by_copy_placement() {
    const NAME: &str = "shadow::bench::compute_heavy_submissions_by_copy_placement";
    let Some(config) = std::env::var_os(CHILD) else {
        let mut rows = Vec::new();
        for (i, (label, upload, writes, in_place)) in CONFIGS.iter().enumerate() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    NAME,
                    "--exact",
                    "--test-threads=1",
                    "--nocapture",
                    "--include-ignored",
                ])
                .env(CHILD, i.to_string())
                .env("VICTORY_GPU_UPLOAD", upload)
                .env("VICTORY_GPU_UPLOAD_WRITES", writes)
                .env("VICTORY_IN_PLACE", in_place)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{label}:\n{text}");
            if let Some(why) = text.lines().find(|l| l.contains("SKIP")) {
                eprintln!("{label}: {why}");
                continue;
            }
            rows.extend(
                text.lines()
                    .filter_map(|l| l.split_once("BENCH|").map(|(_, r)| r))
                    .map(|l| (*label, l.to_string())),
            );
        }
        eprintln!(
            "{DISPATCHES} dispatches in {LISTS} lists; per dispatch {} MiB read from a read-only \
             copy, {} MiB read and written, {} KiB more read; median of 5 submissions, ms",
            READ / MIB,
            READ_WRITTEN / MIB,
            TAIL >> 10
        );
        eprintln!(
            "| copies | writes found by | title writes | total | prepare | GPU | copy-back | \
             making buffers | first submission |"
        );
        eprintln!("|---|---|---|---|---|---|---|---|---|");
        let mut sums: BTreeMap<String, String> = BTreeMap::new();
        for (label, row) in &rows {
            let f: Vec<&str> = row.split('|').collect();
            eprintln!(
                "| {label} | {} | {} | {} | {} | {} | {} | {} | {} |",
                f[0], f[1], f[2], f[3], f[4], f[5], f[7], f[8]
            );
            let key = format!("{} {}", f[0], f[1]);
            let sum = sums.entry(key.clone()).or_insert_with(|| f[6].to_string());
            assert_eq!(sum, f[6], "{key}: title memory differs in {label}");
        }
        return;
    };
    let config: usize = config.to_str().unwrap().parse().unwrap();
    let Some(host) = host() else {
        println!("SKIP: no host GPU");
        return;
    };
    let in_place = config == 3;
    let upload = config == 1 || config == 2;
    if (upload && !host.gpu_upload()) || (config == 2 && !host.gpu_upload_writes()) {
        println!("SKIP: no GPU upload heaps here");
        return;
    }
    if bitmap(false).is_none() {
        println!("SKIP: no written-pages bitmap");
        return;
    }
    let pipeline = pipeline(&host.device);
    for (marked, mode) in [(true, "write marks"), (false, "snapshots")] {
        for changing in [false, true] {
            let made = std::time::Instant::now();
            let read_only: Vec<Buffer> = READ_ONLY.iter().map(|&n| Buffer::new(n, true)).collect();
            let written: Vec<Buffer> = WRITTEN.iter().map(|&n| Buffer::new(n, false)).collect();
            let making = made.elapsed().as_secs_f64() * 1e3;
            if in_place {
                if !read_only.iter().chain(&written).all(|b| b.copy.is_in_place()) {
                    println!("SKIP: title memory does not open in place here");
                    return;
                }
            } else {
                let video = |b: &Buffer| heap_type(&b.copy) == D3D12_HEAP_TYPE_GPU_UPLOAD;
                // Windows are placed as written buffers' copies.
                let upload = upload && !loose_only() || config == 2;
                assert!(read_only.iter().all(|b| video(b) == upload), "read-only placement");
                assert!(written.iter().all(|b| video(b) == (config == 2)), "written placement");
            }
            let mut first = 0.0;
            let mut costs = Vec::new();
            for round in 0..7u32 {
                let cost = submit(
                    host,
                    &pipeline,
                    &read_only,
                    &written,
                    marked,
                    changing.then_some(round),
                );
                if round == 0 {
                    first = cost.total;
                }
                if round >= 2 {
                    costs.push(cost);
                }
            }
            let c = median(&mut costs);
            // What the GPU wrote, as title memory has it now.
            let mut sum = 0u64;
            for b in &written {
                // SAFETY: our pool's bytes.
                let title = unsafe { std::slice::from_raw_parts(b.title as *const u32, b.len / 4) };
                sum = title
                    .iter()
                    .fold(sum, |s, &w| s.wrapping_mul(31).wrapping_add(u64::from(w)));
            }
            assert!(sum != 0, "the GPU's writes reached title memory");
            println!(
                "BENCH|{mode}|{}|{:.1}|{:.1}|{:.1}|{:.1}|{sum:#x}|{making:.1}|{first:.1}",
                if changing {
                    "16 MiB per submission"
                } else {
                    "none"
                },
                c.total,
                c.prepare,
                c.gpu,
                c.back
            );
        }
    }
}
