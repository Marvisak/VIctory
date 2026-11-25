//! What translated 16-bit float math costs, against the host's own.
//!
//! The translator does a console f16 operation in f32 and rounds each
//! result to f16 in integer code (`victory_shader::emit::lower`'s
//! `narrow16`, about 40 operations: the host's `f32tof16` rounds toward zero
//! on some GPUs, the console to nearest even), widening each source with
//! `f16tof32`. A packed instruction such as `v_pk_fma_f16` does that for
//! both halves. The shaders here run a chain of packed fused multiply-adds
//! three ways:
//!
//! - `narrow16`: as translated, sources widened, the FMA in f32, each half
//!   rounded by the integer sequence;
//! - `f32tof16`: the same with the host's conversion instead of the integer
//!   sequence (rounding as the host does, which may not be the console's);
//! - `half2`: the host's native 16-bit math (`-enable-16bit-types`), what an
//!   SM 6.2 path on hosts with native 16-bit operations would emit.
//!
//! The benchmark is `#[ignore]`d (`cargo test -p victory-gpu --release --lib
//! f16_bench -- --ignored --nocapture`); the other test checks the
//! `narrow16` shader rounds to nearest even as the translator does, against
//! a CPU reference. Both need the Windows SDK's `dxcompiler.dll` (loaded in this process) and a GPU, or do
//! nothing.

use windows::Win32::Graphics::Direct3D12::*;

use crate::marks_bench::{Gpu, buffer, compile_hlsl, dxc, heap};

const SOURCE: &str = r#"
#define RS "UAV(u0), RootConstants(num32BitConstants=1, b0)"
RWByteAddressBuffer io : register(u0);
cbuffer c : register(b0) { uint steps; }

// The translator's f16 rounding, to nearest even (lower.rs `narrow16`).
uint narrow16(float f) {
    uint x = asuint(f);
    uint sign = (x >> 16) & 0x8000;
    uint a = x & 0x7FFFFFFF;
    uint rebased = a - 0x38000000;
    uint normal = (rebased + 0xFFF + ((rebased >> 13) & 1)) >> 13;
    normal = normal >= 0x7C00 ? 0x7C00 : normal;
    normal = a == 0x7F800000 ? 0x7C00 : normal;
    uint e = a >> 23;
    uint s = 126 - e;
    s = s > 31 ? 31 : s;
    uint mant = e == 0 ? 0 : ((a & 0x7FFFFF) | 0x800000);
    uint q = mant >> s;
    uint s1 = s - 1;
    uint half_bit = (mant >> s1) & 1;
    uint rest = (mant & ((1u << s1) - 1)) != 0 ? 1 : 0;
    uint den = q + (half_bit & (rest | (q & 1)));
    uint r = a < 0x38800000 ? den : normal;
    uint nan = ((a >> 13) & 0x3FF) | 0x7E00;
    r = a > 0x7F800000 ? nan : r;
    return r | sign;
}

#if VARIANT == 2
typedef half2 V;
V load(uint at) { return io.Load<half2>(at); }
void store(uint at, V v) { io.Store<half2>(at, v); }
V fma2(V a, V b, V c) { return mad(a, b, c); }
#else
typedef uint V;
V load(uint at) { return io.Load(at); }
void store(uint at, V v) { io.Store(at, v); }
uint round16(float f) {
#if VARIANT == 0
    return narrow16(f);
#else
    return f32tof16(f);
#endif
}
// A packed FMA as translated: each half widened, fused in f32, rounded.
V fma2(V a, V b, V c) {
    uint lo = round16(mad(f16tof32(a), f16tof32(b), f16tof32(c)));
    uint hi = round16(mad(f16tof32(a >> 16), f16tof32(b >> 16), f16tof32(c >> 16)));
    return lo | hi << 16;
}
#endif

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint at = id.x * 12;
    V x = load(at), b = load(at + 4), c = load(at + 8);
    for (uint i = 0; i < steps; i++) {
        x = fma2(x, b, c);
    }
    store(at, x);
}
"#;

const VARIANTS: [&str; 3] = ["narrow16", "f32tof16", "half2"];
const GROUPS: u32 = 4096;

struct Bench {
    gpu: Gpu,
    root: ID3D12RootSignature,
    pipelines: Vec<ID3D12PipelineState>,
    io: ID3D12Resource,
    /// Upload memory holding the inputs, copied over `io` before each run.
    inputs: ID3D12Resource,
    back: ID3D12Resource,
    bytes: u64,
}

impl Bench {
    fn new(threads: u32, inputs: &[u32]) -> Option<Bench> {
        let host = crate::host::host()?;
        let dxc = dxc()?;
        let device = host.device.clone();
        let code: Vec<Vec<u8>> = (0..VARIANTS.len())
            .map(|v| {
                let define = format!("-DVARIANT={v}");
                compile_hlsl(&dxc, SOURCE, &[&define, "-enable-16bit-types"])
            })
            .collect::<Option<_>>()?;
        // SAFETY: plain creation from compiled shaders with their signature.
        let (root, pipelines) = unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &code[0]).ok()?;
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
                    device.CreateComputePipelineState(&desc).ok()
                })
                .collect::<Option<_>>()?;
            (root, pipelines)
        };
        let bytes = u64::from(threads) * 12;
        let io = buffer(&device, heap(D3D12_HEAP_TYPE_DEFAULT), bytes, true);
        let upload = buffer(&device, heap(D3D12_HEAP_TYPE_UPLOAD), bytes, false);
        let back = buffer(&device, heap(D3D12_HEAP_TYPE_READBACK), bytes, false);
        let mut data = std::ptr::null_mut();
        // SAFETY: an upload buffer of `bytes` bytes.
        unsafe {
            upload.Map(0, None, Some(&mut data)).unwrap();
            let words = std::slice::from_raw_parts_mut(data.cast::<u32>(), bytes as usize / 4);
            for (k, w) in words.iter_mut().enumerate() {
                *w = inputs[k % inputs.len()];
            }
            upload.Unmap(0, None);
        }
        Some(Bench {
            gpu: Gpu::new(device),
            root,
            pipelines,
            io,
            inputs: upload,
            back,
            bytes,
        })
    }

    /// Resets the inputs, runs `variant` for `steps` FMAs a thread over
    /// `groups` groups and reads the outputs back; its GPU time in ms.
    fn run(&mut self, variant: usize, steps: u32, groups: u32) -> (f64, Vec<u32>) {
        let (io, inputs, back, bytes) = (&self.io, &self.inputs, &self.back, self.bytes);
        let (root, pso) = (&self.root, &self.pipelines[variant]);
        // SAFETY: a live buffer.
        let at = unsafe { io.GetGPUVirtualAddress() };
        let mut ms = 0.0;
        // Inputs in place first (a run of its own: the buffer decays to
        // common after it), then the timed dispatch, then the read-back.
        // SAFETY: an open list; buffers of `bytes` bytes.
        let copy = |list: &ID3D12GraphicsCommandList, to, from| unsafe {
            list.CopyBufferRegion(to, 0, from, 0, bytes)
        };
        self.gpu.timed(|list| copy(list, io, inputs));
        ms += self.gpu.timed(|list| {
            // SAFETY: an open list; the signature's parameters, set.
            unsafe {
                list.SetComputeRootSignature(root);
                list.SetPipelineState(pso);
                list.SetComputeRootUnorderedAccessView(0, at);
                list.SetComputeRoot32BitConstant(1, steps, 0);
                list.Dispatch(groups, 1, 1);
            }
        });
        self.gpu.timed(|list| copy(list, back, io));
        let mut data = std::ptr::null_mut();
        // SAFETY: a read-back buffer of `bytes` bytes, written and done.
        let out = unsafe {
            back.Map(0, None, Some(&mut data)).unwrap();
            let out = std::slice::from_raw_parts(data.cast::<u32>(), bytes as usize / 4).to_vec();
            back.Unmap(0, None);
            out
        };
        (ms, out)
    }
}

/// f16 bits as f32.
fn widen(h: u16) -> f32 {
    let (sign, e, m) = (
        u32::from(h >> 15),
        u32::from(h >> 10 & 31),
        u32::from(h & 0x3FF),
    );
    let bits = match (e, m) {
        (0, 0) => sign << 31,
        (0, _) => {
            // A denormal: m * 2^-24.
            let v = m as f32 * 2f32.powi(-24);
            return if sign == 1 { -v } else { v };
        }
        (31, 0) => sign << 31 | 0x7F80_0000,
        (31, _) => sign << 31 | 0x7FC0_0000 | m << 13,
        _ => sign << 31 | (e + 112) << 23 | m << 13,
    };
    f32::from_bits(bits)
}

/// f32 rounded to the nearest f16, ties to even (NaNs quieted, their
/// payload's top kept), as the console converts.
fn nearest(f: f32) -> u16 {
    let x = f.to_bits();
    let sign = (x >> 16 & 0x8000) as u16;
    let a = x & 0x7FFF_FFFF;
    if a > 0x7F80_0000 {
        return sign | 0x7E00 | (a >> 13 & 0x3FF) as u16;
    }
    let v = f64::from(f32::from_bits(a));
    // Exact: f64 holds every f32, and the halves' grid below.
    let scaled = if v < 2f64.powi(-14) {
        v * 2f64.powi(24) // denormal: units of 2^-24
    } else {
        let e = v.log2().floor() as i32;
        let e = if 2f64.powi(e) > v { e - 1 } else { e };
        let m = v / 2f64.powi(e) * 1024.0; // 1024..2048
        let r = m.round_ties_even();
        let (r, e) = if r >= 2048.0 { (1024.0, e + 1) } else { (r, e) };
        if e > 15 {
            return sign | 0x7C00;
        }
        return sign | ((e + 15) as u16) << 10 | (r as u16 - 1024);
    };
    sign | scaled.round_ties_even() as u16
}

/// The `narrow16` shader rounds each fused result to the nearest half,
/// ties to even, for normal, denormal, overflowing and NaN results, as
/// the translator's rounding does (`f32tof16` may not: it is the host's).
#[test]
fn narrow16_rounds_fused_results_to_nearest_even() {
    // (a, b, c) triples in the low and high halves of three dwords.
    let cases: [[u16; 3]; 10] = [
        [0x3C01, 0x3E00, 0x8001],
        [0x3C03, 0x3E00, 0x0001],
        [0x3C01, 0x3E00, 0xBE00],
        [0x0200, 0x3800, 0x0001], // a denormal result
        [0x7BFF, 0x4000, 0x0000], // overflows to infinity
        [0x7E01, 0x3C00, 0x0000], // NaN in, NaN out
        [0xC500, 0x3555, 0x3C00],
        [0x1400, 0x1400, 0x0000], // underflows toward zero
        [0x0001, 0x3800, 0x0000], // a denormal tie: to even, 0
        [0x0003, 0x3800, 0x0000], // a denormal tie: to even, 2
    ];
    let mut inputs = Vec::new();
    for pair in cases.chunks(2) {
        let (lo, hi) = (pair[0], pair[1]);
        inputs.extend(
            lo.iter()
                .zip(hi)
                .map(|(&l, h)| u32::from(l) | u32::from(h) << 16),
        );
    }
    let threads = (cases.len() / 2) as u32;
    // One group of 64 threads: inputs repeat; check the first five.
    let Some(mut b) = Bench::new(64.max(threads), &inputs) else {
        return; // no host GPU or no SDK compiler here
    };
    let (_, out) = b.run(0, 1, 1);
    for (t, pair) in cases.chunks(2).enumerate() {
        for (half, [a, bb, c]) in pair.iter().enumerate() {
            // The host may fuse `mad` or not: either f32 result, rounded.
            let (a, bb, c) = (widen(*a), widen(*bb), widen(*c));
            let want = [nearest(a.mul_add(bb, c)), nearest(a * bb + c)];
            let got = (out[3 * t] >> (16 * half)) as u16;
            // The host's f32 math may change a NaN's payload; a NaN it stays.
            let nan = |h: u16| h & 0x7C00 == 0x7C00 && h & 0x3FF != 0;
            let same = want.contains(&got) || nan(want[0]) && nan(got);
            assert!(same, "fma({a}, {bb}, {c}): {got:#06x}, not {want:#06x?}");
        }
    }
}

/// The benchmark: ms for 262,144 threads each running a chain of 1,024
/// packed FMAs, per variant; the median of five.
#[test]
#[ignore]
fn f16_bench() {
    let inputs = [0x3C00_3C01, 0x3BFF_3C00, 0x0001_8001];
    let Some(mut b) = Bench::new(GROUPS * 64, &inputs) else {
        eprintln!("no host GPU or no SDK compiler: nothing measured");
        return;
    };
    let steps = 1024;
    println!(
        "ms per dispatch ({GROUPS} groups of 64 threads, {steps} packed FMAs each), median of 5"
    );
    let mut base = None;
    for (v, name) in VARIANTS.iter().enumerate() {
        b.run(v, steps, GROUPS); // warm-up
        let mut t: Vec<f64> = (0..5).map(|_| b.run(v, steps, GROUPS).0).collect();
        t.sort_by(f64::total_cmp);
        let ms = t[2];
        let base = *base.get_or_insert(ms);
        let ops = f64::from(GROUPS * 64) * f64::from(steps) * 2.0;
        println!(
            "{name:<10}{ms:>9.3} ms  {:>7.1} G f16 FMAs/s  {:>5.1}x narrow16's speed",
            ops / ms / 1e6,
            base / ms
        );
    }
}
