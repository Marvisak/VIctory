//! What marking written pages costs translated programs, on this GPU.
//!
//! A program that writes title memory at addresses it computes looks each
//! address up in the page table and sets the bits of the pages it wrote in
//! a bitmap with atomics (`victory_shader::emit`, "Title memory"; module
//! `crate::memory`). The table is in an upload heap and the bitmap in
//! system memory (`Host::shared_buffer`), so each atomic crosses the bus.
//! The shaders here do what translated code does, in HLSL, one variant per
//! way of marking:
//!
//! - `main`: two atomics per store, the pages of its first and last byte,
//!   whether set or not; the table's null entry loaded per access;
//! - `test-first`: the bit is loaded and the atomic issued only where it is
//!   clear, the second skipped for a store in one page; the entry in one
//!   load and the null entry once per program;
//! - `wave`: where a wave's active lanes mark the same bitmap word, one lane
//!   ORs their bits in one atomic (others each their own), after a load
//!   finds a bit missing; the second mark only for a store across pages;
//! - `wave+test-first`: both;
//! - `none`: no marks, the floor.
//!
//! Each runs with the bitmap in system memory (as now) and in video memory,
//! over four ways of writing: all in one page, linear, a page per lane (32
//! pages to a bitmap word) and scattered. Times are GPU timestamps around
//! one dispatch, on a queue of its own, with the bitmap clear (each page's
//! first marks) and already set (later dispatches of the same submission).
//!
//! The shaders are compiled in this process by the Windows SDK's
//! `dxcompiler.dll` (shader model 6: wave operations); without it,
//! or without a GPU, the tests do nothing. The benchmark is `#[ignore]`d:
//! `cargo test -p victory-gpu --release --lib marks_bench -- --ignored
//! --nocapture`. The other test checks that every variant sets the same
//! bits as `main` (its marks are what copy-back copies).

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{HRESULT, s};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

const SOURCE: &str = r#"
#define RS "SRV(t0), UAV(u0), UAV(u1), RootConstants(num32BitConstants=4, b0)"
ByteAddressBuffer table : register(t0);
RWByteAddressBuffer dirty : register(u0);
RWByteAddressBuffer data : register(u1);
cbuffer c : register(b0) { uint pattern; uint stores; uint base_lo; uint base_hi; }

// VARIANT: 0 main, 1 test-first, 2 wave, 3 wave and test-first, 4 none.
#define TEST_FIRST (VARIANT == 1 || VARIANT == 3)
#define WAVE (VARIANT == 2 || VARIANT == 3)

static uint null_view;

void mark(uint n) {
    uint word = n >> 5 << 2;
    uint bit = 1u << (n & 31);
#if WAVE
    bool same = WaveActiveAllEqual(word);
    uint bits = WaveActiveBitOr(bit);
    bool first = WaveIsFirstLane();
    uint mask = same ? bits : bit;
    if (!same || first) {
        if ((dirty.Load(word) & mask) != mask) {
            dirty.InterlockedOr(word, mask);
        }
    }
#elif TEST_FIRST
    if ((dirty.Load(word) & bit) == 0) {
        dirty.InterlockedOr(word, bit);
    }
#else
    dirty.InterlockedOr(word, bit);
#endif
}

// A dword store at title address (hi:lo), through the page table.
void store(uint lo, uint hi, uint value) {
    bool inside = hi < 256;
    uint l1 = inside ? (hi << 4 | lo >> 28) : 0;
    uint block = inside ? table.Load(l1 * 4) : 0;
    uint entry = block << 12 | (lo >> 16 & 0xFFF);
#if TEST_FIRST
    uint2 e = table.Load2((4096 + 2 * entry) * 4);
    uint nul = null_view;
#else
    uint2 e = uint2(table.Load((4096 + 2 * entry) * 4), table.Load((4097 + 2 * entry) * 4));
    uint nul = table.Load(4096 * 4);
#endif
    bool miss = e.x == nul;
    uint first = miss && block == 0 ? l1 : entry;
#if VARIANT != 4
    uint last = miss ? first : entry + ((lo + 3) >> 16) - (lo >> 16);
#if VARIANT == 0
    mark(first);
    mark(last);
#else
    mark(first);
    if (last != first) {
        mark(last);
    }
#endif
#endif
    if (!miss) {
        data.Store(lo - e.y, value);
    }
}

uint hash(uint x) {
    x ^= x >> 16; x *= 0x7feb352d; x ^= x >> 15; x *= 0x846ca68b; x ^= x >> 16;
    return x;
}

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID, uint3 groups : SV_GroupID) {
#if TEST_FIRST
    null_view = table.Load(4096 * 4);
#endif
    uint g = id.x;
    uint threads = 64 * 2048;
    for (uint i = 0; i < stores; i++) {
        uint off;
        if (pattern == 0) {
            off = (g * 4) & 0xFFFF;                          // one page
        } else if (pattern == 1) {
            off = ((i * threads + g) * 4) & 0x3FFFFFF;       // linear
        } else if (pattern == 2) {
            off = ((g + i * 7) & 1023) << 16 | (i * 4);      // a page per lane
        } else {
            uint h = hash(g * 131 + i);
            off = (h & 1023) << 16 | (h >> 16 & 0xFFFC);     // scattered
        }
        store(base_lo + off, base_hi, g ^ i);
    }
}
"#;

/// Names of the variants, by `VARIANT`.
const VARIANTS: [&str; 5] = ["main", "test-first", "wave", "wave+test-first", "none"];
const PATTERNS: [&str; 4] = ["one page", "linear", "page per lane", "scattered"];
/// Groups of 64 threads a dispatch (the shader's `threads` is 64 times
/// this).
const GROUPS: u32 = 2048;
/// Title memory the table maps: 1,024 pages of 64 KiB from [`BASE`].
const PAGES: u64 = 1024;
/// Where it is: 256 MiB region 0x41 (block 1 of the table).
const BASE: u64 = 0x41 << 28;
/// Bitmap bytes: a bit for each of the table's entries (two blocks).
const BITMAP: u64 = 2 * 4096 / 8;

/// The newest Windows SDK's DXC folder (`bin\<version>\x64`, holding
/// `dxcompiler.dll`).
fn dxc() -> Option<PathBuf> {
    let bin = PathBuf::from(std::env::var_os("ProgramFiles(x86)")?).join(r"Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(bin)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path().join("x64")))
        .filter(|p| p.join("dxcompiler.dll").is_file())
        .collect();
    versions.sort();
    versions.pop()
}

/// The shader of `variant`, compiled.
fn compile(dxc: &Path, variant: usize) -> Option<Vec<u8>> {
    let define = format!("-DVARIANT={variant}");
    let code = compile_hlsl(dxc, SOURCE, &[&define]);
    assert!(code.is_some(), "dxc failed for {}", VARIANTS[variant]);
    code
}

/// `source` compiled as a cs_6_6 program with `args`, in this process by
/// the SDK's `dxcompiler.dll` in `dir` (D3DCompile has no shader model 6:
/// no wave operations, no 16-bit types), signed by its `dxil.dll` where
/// the SDK has one (without, DXC marks the container for the runtime to
/// take unsigned). None if the compiler cannot be loaded; compile errors
/// panic.
pub(crate) fn compile_hlsl(dir: &Path, source: &str, args: &[&str]) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Dxc::*;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::{GUID, HSTRING, Interface, PCWSTR};
    type Create = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
    // SAFETY: loading the SDK's DLLs and calling its documented entry point
    // and interfaces; `dxil.dll` first, so the compiler finds it to sign.
    unsafe {
        let _ = LoadLibraryW(&HSTRING::from(dir.join("dxil.dll").as_os_str()));
        let compiler = LoadLibraryW(&HSTRING::from(dir.join("dxcompiler.dll").as_os_str())).ok()?;
        let create: Create =
            std::mem::transmute(GetProcAddress(compiler, s!("DxcCreateInstance"))?);
        let new = |clsid: &GUID, iid: &GUID| {
            let mut out = std::ptr::null_mut();
            create(clsid, iid, &mut out).ok().map(|()| out)
        };
        let c: IDxcCompiler3 =
            IDxcCompiler3::from_raw(new(&CLSID_DxcCompiler, &IDxcCompiler3::IID).ok()?);
        let mut words: Vec<HSTRING> = ["-T", "cs_6_6", "-E", "main", "-O3"]
            .iter()
            .map(|a| HSTRING::from(*a))
            .collect();
        words.extend(args.iter().map(|a| HSTRING::from(*a)));
        let argv: Vec<PCWSTR> = words.iter().map(|w| PCWSTR(w.as_ptr())).collect();
        let buffer = DxcBuffer {
            Ptr: source.as_ptr().cast(),
            Size: source.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        let result: IDxcResult = c.Compile(&buffer, Some(&argv), None).ok()?;
        let status = result.GetStatus().ok()?;
        if status.is_err() {
            let errors = result.GetErrorBuffer().ok().map(|b| {
                let bytes = std::slice::from_raw_parts(
                    b.GetBufferPointer().cast::<u8>(),
                    b.GetBufferSize(),
                );
                String::from_utf8_lossy(bytes).into_owned()
            });
            panic!("dxc: {status:?}: {}", errors.unwrap_or_default());
        }
        let blob = result.GetResult().ok()?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        Some(bytes.to_vec())
    }
}

fn buffer(
    device: &ID3D12Device,
    heap: D3D12_HEAP_PROPERTIES,
    size: u64,
    uav: bool,
) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if uav {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAG_NONE
        },
        ..Default::default()
    };
    let state = if heap.Type == D3D12_HEAP_TYPE_UPLOAD {
        D3D12_RESOURCE_STATE_GENERIC_READ
    } else if heap.Type == D3D12_HEAP_TYPE_READBACK {
        D3D12_RESOURCE_STATE_COPY_DEST
    } else {
        D3D12_RESOURCE_STATE_COMMON
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out)
            .unwrap();
    }
    out.unwrap()
}

fn heap(kind: D3D12_HEAP_TYPE) -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: kind,
        ..Default::default()
    }
}

/// System memory the CPU reads back, as `Host::shared_buffer`'s.
fn system() -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_CUSTOM,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
        MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
        ..Default::default()
    }
}

/// A queue, a list and a fence of the bench's own.
struct Gpu {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: HANDLE,
    done: u64,
    stamps: ID3D12QueryHeap,
    readback: ID3D12Resource,
    frequency: u64,
}

impl Gpu {
    fn new(device: ID3D12Device) -> Gpu {
        // SAFETY: plain creation.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .unwrap();
            list.Close().unwrap();
            let fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event = CreateEventW(None, false, false, None).unwrap();
            let mut stamps: Option<ID3D12QueryHeap> = None;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        NodeMask: 0,
                    },
                    &mut stamps,
                )
                .unwrap();
            let readback = buffer(&device, heap(D3D12_HEAP_TYPE_READBACK), 16, false);
            let frequency = queue.GetTimestampFrequency().unwrap();
            Gpu {
                queue,
                allocator,
                list,
                fence,
                event,
                done: 0,
                stamps: stamps.unwrap(),
                readback,
                frequency,
            }
        }
    }

    /// Records with `f` into the reset list, runs it and waits.
    fn run(&mut self, f: impl FnOnce(&ID3D12GraphicsCommandList)) {
        // SAFETY: the list is idle (every run is waited for).
        unsafe {
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
            f(&self.list);
            self.list.Close().unwrap();
            self.queue
                .ExecuteCommandLists(&[Some(self.list.cast::<ID3D12CommandList>().unwrap())]);
            self.done += 1;
            self.queue.Signal(&self.fence, self.done).unwrap();
            self.fence
                .SetEventOnCompletion(self.done, self.event)
                .unwrap();
            WaitForSingleObject(self.event, INFINITE);
        }
    }

    /// Runs `f` between two timestamps; its GPU time in ms.
    fn timed(&mut self, f: impl FnOnce(&ID3D12GraphicsCommandList)) -> f64 {
        let (stamps, readback) = (self.stamps.clone(), self.readback.clone());
        self.run(|list| {
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                list.EndQuery(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                f(list);
                list.EndQuery(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                list.ResolveQueryData(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &readback, 0);
            }
        });
        let mut data = std::ptr::null_mut();
        // SAFETY: the read-back buffer of two stamps, resolved and done.
        let t = unsafe {
            self.readback.Map(0, None, Some(&mut data)).unwrap();
            let t = std::slice::from_raw_parts(data.cast::<u64>(), 2).to_vec();
            self.readback.Unmap(0, None);
            t
        };
        t[1].saturating_sub(t[0]) as f64 * 1e3 / self.frequency as f64
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: our event.
        let _ = unsafe { CloseHandle(self.event) };
    }
}

/// Everything the variants run with.
struct Bench {
    gpu: Gpu,
    root: ID3D12RootSignature,
    pipelines: Vec<ID3D12PipelineState>,
    table: ID3D12Resource,
    data: ID3D12Resource,
    /// The bitmap in system memory (mapped) and in video memory.
    system: (ID3D12Resource, *mut u32),
    video: ID3D12Resource,
    /// Zeros to clear the video one, and where to read it back.
    zeros: ID3D12Resource,
    back: ID3D12Resource,
}

impl Bench {
    fn new() -> Option<Bench> {
        let host = crate::host::host()?;
        let dxc = dxc()?;
        let device = host.device.clone();
        let code: Vec<Vec<u8>> = (0..VARIANTS.len())
            .map(|v| compile(&dxc, v))
            .collect::<Option<_>>()?;
        // SAFETY: plain creation from compiled shaders with their signature.
        let (root, pipelines) = unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &code[0]).unwrap();
            let pipelines = code
                .iter()
                .map(|bytes| {
                    let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    };
                    device.CreateComputePipelineState(&desc).unwrap()
                })
                .collect();
            (root, pipelines)
        };
        // The page table: level 1 maps region 0x41 to block 1, whose
        // entries are view 1 from BASE; entry 0 (block 0's first) is the
        // null view, 0.
        let entries = 4096 + 2 * 2 * 4096;
        let table = buffer(
            &device,
            heap(D3D12_HEAP_TYPE_UPLOAD),
            entries as u64 * 4,
            false,
        );
        let mut words = vec![0u32; entries];
        words[(BASE >> 28) as usize] = 1;
        for page in 0..PAGES as usize {
            let entry = 4096 + page;
            words[4096 + 2 * entry] = 1;
            words[4096 + 2 * entry + 1] = BASE as u32;
        }
        let mut data_ptr = std::ptr::null_mut();
        // SAFETY: an upload buffer of `entries` dwords.
        unsafe {
            table.Map(0, None, Some(&mut data_ptr)).unwrap();
            std::ptr::copy_nonoverlapping(words.as_ptr(), data_ptr.cast(), entries);
            table.Unmap(0, None);
        }
        let data = buffer(&device, system(), PAGES << 16, true);
        let system_bitmap = buffer(&device, system(), BITMAP, true);
        let mut bits = std::ptr::null_mut();
        // SAFETY: a CPU-visible buffer, mapped for its life.
        unsafe { system_bitmap.Map(0, None, Some(&mut bits)).unwrap() };
        let video = buffer(&device, heap(D3D12_HEAP_TYPE_DEFAULT), BITMAP, true);
        let zeros = buffer(&device, heap(D3D12_HEAP_TYPE_UPLOAD), BITMAP, false);
        // SAFETY: an upload buffer of BITMAP bytes.
        unsafe {
            let mut z = std::ptr::null_mut();
            zeros.Map(0, None, Some(&mut z)).unwrap();
            std::ptr::write_bytes(z.cast::<u8>(), 0, BITMAP as usize);
            zeros.Unmap(0, None);
        }
        let back = buffer(&device, heap(D3D12_HEAP_TYPE_READBACK), BITMAP, false);
        Some(Bench {
            gpu: Gpu::new(device),
            root,
            pipelines,
            table,
            data,
            system: (system_bitmap, bits.cast()),
            video,
            zeros,
            back,
        })
    }

    /// Clears the bitmap (in video memory or not).
    fn clear(&mut self, video: bool) {
        if !video {
            // SAFETY: the mapped bitmap, the GPU done with it.
            unsafe { std::ptr::write_bytes(self.system.1, 0, BITMAP as usize / 4) };
            return;
        }
        let (dst, src) = (self.video.clone(), self.zeros.clone());
        // SAFETY: an open list; buffers of BITMAP bytes, in their states
        // (the video one decays to common after each run).
        self.gpu
            .run(|list| unsafe { list.CopyBufferRegion(&dst, 0, &src, 0, BITMAP) });
    }

    /// The bitmap's words.
    fn bitmap(&mut self, video: bool) -> Vec<u32> {
        let n = BITMAP as usize / 4;
        if !video {
            // SAFETY: the mapped bitmap, the GPU done with it.
            return unsafe { std::slice::from_raw_parts(self.system.1, n).to_vec() };
        }
        let (dst, src) = (self.back.clone(), self.video.clone());
        // SAFETY: an open list; buffers of BITMAP bytes.
        self.gpu
            .run(|list| unsafe { list.CopyBufferRegion(&dst, 0, &src, 0, BITMAP) });
        let mut p = std::ptr::null_mut();
        // SAFETY: a read-back buffer of BITMAP bytes, written and done.
        unsafe {
            self.back.Map(0, None, Some(&mut p)).unwrap();
            let words = std::slice::from_raw_parts(p.cast::<u32>(), n).to_vec();
            self.back.Unmap(0, None);
            words
        }
    }

    /// [`GROUPS`] groups of `variant` over `pattern`, 16 stores a thread,
    /// as four submissions of a quarter each (the slowest variant takes
    /// over a second for all of them, near the host's 2 s device reset);
    /// their GPU times summed, in ms.
    fn timed_quarters(&mut self, variant: usize, video: bool, pattern: u32) -> f64 {
        (0..4)
            .map(|_| self.dispatch(variant, video, pattern, 16, GROUPS / 4))
            .sum()
    }

    /// One dispatch of `variant` over `pattern`, `stores` a thread, `groups`
    /// groups; its GPU time in ms.
    fn dispatch(
        &mut self,
        variant: usize,
        video: bool,
        pattern: u32,
        stores: u32,
        groups: u32,
    ) -> f64 {
        let pso = self.pipelines[variant].clone();
        let root = self.root.clone();
        // SAFETY: live buffers.
        let (table, bitmap, data) = unsafe {
            (
                self.table.GetGPUVirtualAddress(),
                if video { &self.video } else { &self.system.0 }.GetGPUVirtualAddress(),
                self.data.GetGPUVirtualAddress(),
            )
        };
        let constants = [pattern, stores, BASE as u32, (BASE >> 32) as u32];
        self.gpu.timed(|list| {
            // SAFETY: an open list; the signature's parameters, set.
            unsafe {
                list.SetComputeRootSignature(&root);
                list.SetPipelineState(&pso);
                list.SetComputeRootShaderResourceView(0, table);
                list.SetComputeRootUnorderedAccessView(1, bitmap);
                list.SetComputeRootUnorderedAccessView(2, data);
                list.SetComputeRoot32BitConstants(3, 4, constants.as_ptr().cast(), 0);
                list.Dispatch(groups, 1, 1);
            }
        })
    }
}

/// Every variant marks the same pages as `main`, for every pattern and
/// bitmap place: a bit for each page written, and only those.
#[test]
fn every_variant_marks_the_pages_main_marks() {
    let Some(mut b) = Bench::new() else {
        return; // no host GPU or no SDK compiler here
    };
    for pattern in 0..PATTERNS.len() as u32 {
        b.clear(false);
        b.dispatch(0, false, pattern, 4, 64);
        let want = b.bitmap(false);
        assert!(
            want.iter().any(|&w| w != 0),
            "{}: main marks nothing",
            PATTERNS[pattern as usize]
        );
        for (variant, name) in VARIANTS.iter().enumerate().take(VARIANTS.len() - 1).skip(1) {
            for video in [false, true] {
                b.clear(video);
                b.dispatch(variant, video, pattern, 4, 64);
                assert_eq!(
                    b.bitmap(video),
                    want,
                    "{name} over {} (video memory: {video})",
                    PATTERNS[pattern as usize]
                );
            }
        }
    }
}

/// The benchmark: ms a dispatch of 131,072 threads storing 16 dwords each,
/// per variant, bitmap place and pattern; the median of five runs, with
/// the bitmap clear before each and already set.
#[test]
#[ignore]
fn marks_bench() {
    let Some(mut b) = Bench::new() else {
        eprintln!("no host GPU or no SDK compiler: nothing measured");
        return;
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    println!("ms per dispatch ({GROUPS} groups of 64 threads, 16 stores each), median of 5");
    println!(
        "{:<18}{:<8}{:<15}{:>10}{:>10}",
        "variant", "bitmap", "pattern", "clear", "set"
    );
    for (variant, name) in VARIANTS.iter().enumerate() {
        for video in [false, true] {
            if variant == VARIANTS.len() - 1 && video {
                continue; // no marks: no bitmap
            }
            for pattern in 0..PATTERNS.len() as u32 {
                b.clear(video);
                b.timed_quarters(variant, video, pattern); // warm-up
                let clear = median(
                    (0..5)
                        .map(|_| {
                            b.clear(video);
                            b.timed_quarters(variant, video, pattern)
                        })
                        .collect(),
                );
                let set = median(
                    (0..5)
                        .map(|_| b.timed_quarters(variant, video, pattern))
                        .collect(),
                );
                println!(
                    "{name:<18}{:<8}{:<15}{clear:>10.3}{set:>10.3}",
                    if video { "video" } else { "system" },
                    PATTERNS[pattern as usize]
                );
            }
        }
    }
}

/// What a page bitmap in video memory costs a list's end
/// (as #2175 and #2465 record them): a copy of the bitmap into read-back memory and its
/// clear from zeros, at the size `crate::memory` uses (513 blocks of 4,096
/// pages, 256.5 KiB); the median of 20, in µs.
#[test]
#[ignore]
fn bitmap_copy_out_bench() {
    let Some(host) = crate::host::host() else {
        return;
    };
    let bytes = (513 * 4096 / 8_u64).next_multiple_of(256);
    let device = host.device.clone();
    let bitmap = buffer(&device, heap(D3D12_HEAP_TYPE_DEFAULT), bytes, true);
    let back = buffer(&device, heap(D3D12_HEAP_TYPE_READBACK), bytes, false);
    let zeros = buffer(&device, heap(D3D12_HEAP_TYPE_UPLOAD), bytes, false);
    let mut gpu = Gpu::new(device);
    let mut times: Vec<f64> = (0..21)
        .map(|_| {
            gpu.timed(|list| {
                // SAFETY: an open list; buffers of `bytes` bytes, the bitmap
                // in the common state at the list's start.
                unsafe {
                    use D3D12_RESOURCE_STATE_COPY_DEST as DEST;
                    use D3D12_RESOURCE_STATE_COPY_SOURCE as SOURCE;
                    let t = crate::host::transition;
                    list.ResourceBarrier(&[t(&bitmap, D3D12_RESOURCE_STATE_COMMON, SOURCE)]);
                    list.CopyBufferRegion(&back, 0, &bitmap, 0, bytes);
                    list.ResourceBarrier(&[t(&bitmap, SOURCE, DEST)]);
                    list.CopyBufferRegion(&bitmap, 0, &zeros, 0, bytes);
                }
            }) * 1e3
        })
        .skip(1)
        .collect();
    times.sort_by(f64::total_cmp);
    println!(
        "bitmap copy-out and clear, {bytes} bytes: {:.1} µs",
        times[times.len() / 2]
    );
}
