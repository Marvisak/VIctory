//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So every record is clamped on the GPU,
//! just before its execution, by a compute shader of ours (HLSL compiled by
//! the system's compiler; a 5.1 shader with its root signature inside).
//!
//! Each list also has a budget (2^27 vertex instances or threads, groups
//! counted as 64 threads, mesh groups as 128): records past it run empty,
//! so a list of thousands of garbage records cannot run for seconds
//! either.
//!
//! shortcut: the bounds (2^24 vertices, 2^26 vertex instances, 65,535
//! groups per dimension and 2^20 per dispatch, one-dimensional mesh
//! dispatches, the budget) cut legitimate work beyond them; raise them if
//! the title needs more.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host, transition};

/// Minimum record count before stream compaction is enabled; below this,
/// the count-buffer indirection and compaction pass cost more on Arc A770
/// than skipping empty records saves.
const COMPACT_MIN_RECORDS: u32 = 16;

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), UAV(u1), UAV(u0)"
cbuffer Record : register(b0) { uint kind; uint stride; uint records; uint do_compact; };
// The title's records, read only; the clamped ones go to the list's own.
ByteAddressBuffer args : register(t0);
RWByteAddressBuffer dest : register(u0);
// The list's work so far (vertex instances, threads): past the budget,
// records run empty.
RWByteAddressBuffer budget : register(u1);
static const uint BUDGET = 1u << 27;
// Groups of one dispatch; more run empty.
static const uint GROUPS = 1u << 20;
// Loads and clamps the record at `at` into `w` (plus `w4` for indexed draws); its cost.
uint load_clamped(uint at, out uint4 w, out uint w4) {
    w = uint4(0, 0, 0, 0);
    w4 = 0;
    uint cost;
    if (kind <= 1) {
        // Draws: vertex or index count, then instances.
        uint4 raw = args.Load4(at);
        uint count = min(raw.x, 1u << 24);
        uint instances = min(raw.y, (1u << 26) / max(count, 1u));
        cost = count * instances;
        w = uint4(count, instances, raw.z, raw.w);
        if (kind == 1) {
            w4 = args.Load(at + 16);
        }
    } else if (kind == 2) {
        uint3 g = min(args.Load3(at), uint3(65535, 65535, 65535));
        // x * y fits 32 bits; the whole count may not.
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
        cost = g.x * g.y * g.z * 64;
        w.xyz = g;
    } else {
        // Mesh dispatches: the console's are one-dimensional.
        uint3 g = args.Load3(at);
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        cost = g.x * g.y * g.z * 128;
        w.xyz = g;
    }
    return cost;
}
void store_record(uint at, uint4 w, uint w4) {
    if (kind == 0) {
        dest.Store4(at, w);
    } else if (kind == 1) {
        dest.Store4(at, w);
        dest.Store(at + 16, w4);
    } else {
        dest.Store3(at, w.xyz);
    }
}
// A record past the budget (no cost reaches it).
static const uint PAST = 0xffffffffu;
groupshared uint costs[64];
groupshared uint offsets[64];
// One group walks the records in tiles of 64: every lane clamps its own
// record, then the first lane charges the tile's records in order and assigns
// compacted slots for non-empty survivors.
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint lane : SV_GroupIndex) {
    uint left = 0;
    uint total = 0;
    if (lane == 0) {
        left = BUDGET - min(budget.Load(0), BUDGET);
    }
    for (uint tile = 0; tile < records / 64 + min(records % 64, 1u); tile++) {
        uint record = tile * 64 + lane;
        uint at = record * stride;
        uint4 w = uint4(0, 0, 0, 0);
        uint w4 = 0;
        uint cost = 0;
        if (record < records) {
            cost = load_clamped(at, w, w4);
        }
        costs[lane] = cost;
        GroupMemoryBarrierWithGroupSync();
        if (lane == 0) {
            uint n = min(64u, records - tile * 64u);
            if (do_compact != 0) {
                for (uint i = 0; i < n; i++) {
                    uint c = costs[i];
                    if (c > left) {
                        costs[i] = PAST;
                        left = 0;
                    } else {
                        left -= c;
                        if (c > 0) {
                            offsets[i] = total;
                            total++;
                        }
                    }
                }
            } else {
                for (uint i = 0; i < n; i++) {
                    uint c = costs[i];
                    if (c > left) {
                        // The first record that does not fit takes what is
                        // left, so those after it run empty too.
                        costs[i] = PAST;
                        left = 0;
                    } else {
                        left -= c;
                    }
                }
            }
        }
        GroupMemoryBarrierWithGroupSync();
        if (record < records) {
            if (do_compact != 0) {
                if (costs[lane] != PAST && cost > 0) {
                    store_record(16u + offsets[lane] * stride, w, w4);
                }
            } else {
                if (costs[lane] == PAST) {
                    // Draws keep their layout with no vertices; groups become zero.
                    w.x = 0;
                }
                store_record(at, w, w4);
            }
        }
    }
    if (lane == 0) {
        budget.Store(0, BUDGET - left);
        if (do_compact != 0) {
            dest.Store(0, total);
        }
    }
}
"#;

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match make() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

fn make() -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("clamp"),
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
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize())
            };
            tracing::warn!("argument clamp: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}

/// Output of [`bound`]: the list's own clamped argument buffer and offsets
/// for `ExecuteIndirect`.
pub(crate) struct Clamped {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) args_offset: u64,
    pub(crate) count_offset: Option<u64>,
}

/// Records, in `list`, the clamp of `records` records of `kind` (PC
/// argument numbering) `stride` bytes apart at host address `gpu` (a title
/// buffer's copy, only read) into the list's own argument buffer, left
/// ready to be read as arguments: that buffer, for [`done`] after the
/// execution. When `enable_compaction` is set and `records` reaches
/// [`COMPACT_MIN_RECORDS`], non-empty surviving records are packed after a
/// 16-byte header holding the compacted command count at offset 0. Replaces
/// the list's compute root signature and pipeline (callers bind theirs
/// after). `None` if the clamp is not available.
///
/// Title buffers' copies are never transitioned: they are common between
/// submissions and every use promotes them (shader and root views, the
/// input assembler, these reads), with unordered-access barriers for order.
/// Only the list's argument buffer moves, common, unordered access,
/// indirect argument, common: its states are known whatever order lists
/// run in.
// shortcut: a buffer a shader writes through an unordered view and the
// input assembler then reads in one submission is, by the letter of D3D12,
// promoted to a write state the input assembler may not use (the debug
// layer cannot see view accesses; GPU-based validation might); upgrade to
// per-list tracked buffer states if a driver or that validation minds.
pub(crate) fn bound(
    list: &HostList,
    gpu: u64,
    spec: (u32, u32, u32),
    enable_compaction: bool,
) -> Option<Clamped> {
    let do_compact = enable_compaction && spec.2 >= COMPACT_MIN_RECORDS;
    bound_inner(list, gpu, spec, do_compact)
}

fn bound_inner(
    list: &HostList,
    gpu: u64,
    (kind, stride, records): (u32, u32, u32),
    do_compact: bool,
) -> Option<Clamped> {
    let (Some(c), Some(budget)) = (clamp(), list.budget()) else {
        return None;
    };
    if !gpu.is_multiple_of(4) || !stride.is_multiple_of(4) {
        return None;
    }
    let payload_bytes = u64::from(stride).checked_mul(u64::from(records.max(1)))?;
    let needed = if do_compact {
        16u64.checked_add(payload_bytes)?
    } else {
        payload_bytes
    };
    let out = list.arguments(needed)?;
    let l = &list.list;
    let constants = [kind, stride, records, u32::from(do_compact)];
    // SAFETY: an open list; our root signature, pipeline and buffers, and
    // a title buffer's copy the caller's list holds.
    unsafe {
        // Earlier shader writes (the title's argument passes) first.
        l.ResourceBarrier(&[
            uav_barrier(),
            transition(&out, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
        ]);
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        l.SetComputeRootShaderResourceView(1, gpu);
        l.SetComputeRootUnorderedAccessView(2, budget);
        l.SetComputeRootUnorderedAccessView(3, out.GetGPUVirtualAddress());
        // One group walks every record.
        l.Dispatch(records.min(1), 1, 1);
        l.ResourceBarrier(&[
            uav_barrier(),
            transition(&out, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT),
        ]);
    }
    Some(Clamped {
        buffer: out,
        args_offset: if do_compact { 16 } else { 0 },
        count_offset: do_compact.then_some(0),
    })
}

/// After the execution: the list's argument buffer back to common.
pub(crate) fn done(list: &HostList, arguments: &ID3D12Resource) {
    // SAFETY: an open list and a live buffer.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            arguments,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COMMON,
        )])
    };
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
        },
    }
}

#[cfg(test)]
mod tests {
    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        if super::host().is_none() {
            return; // no host GPU here
        }
        assert!(super::clamp().is_some());
    }

    /// Clamps `records` records of `kind`, filled by repeating `words`,
    /// `calls` times in one list: each record's first word in the list's
    /// argument buffer after (zero when it runs empty). The title's copy,
    /// one record more included, stays as written. `None` without a host
    /// GPU.
    fn clamped(kind: u32, words: &[u32], records: u32, calls: u32) -> Option<Vec<u32>> {
        let host = super::host()?;
        // Words in a record of `kind`.
        let per = match kind {
            0 => 4,
            1 => 5,
            _ => 3,
        };
        let spec = (kind, per as u32 * 4, records);
        let all = per * (records as usize + 1);
        let (buffer, data) = host.shared_buffer(all as u64 * 4).unwrap();
        let data = data.cast::<u32>();
        let list = host.new_list().unwrap();
        // SAFETY: our mapped buffer, written before the list runs and read
        // after it has; a live buffer.
        unsafe {
            for at in 0..all {
                data.add(at).write_volatile(words[at % words.len()]);
            }
            let gpu = buffer.GetGPUVirtualAddress();
            let mut out = None;
            for _ in 0..calls {
                let clamped = super::bound(&list, gpu, spec, false).unwrap();
                super::done(&list, &clamped.buffer);
                out = Some(clamped.buffer);
            }
            list.close();
            host.execute(&[&list]);
            for at in 0..all {
                assert_eq!(data.add(at).read_volatile(), words[at % words.len()], "the title's copy as written");
            }
            let mut out_data = std::ptr::null_mut();
            out?.Map(0, None, Some(&mut out_data)).unwrap();
            let first = |i: usize| out_data.cast::<u32>().add(i * per).read_volatile();
            Some((0..records as usize).map(first).collect())
        }
    }

    /// Rejected records do not wrap the budget: of 128 garbage draws (2^26
    /// vertex instances each once clamped) two run, where a wrapping add
    /// let four; of 600 garbage mesh dispatches 16, not 31.
    #[test]
    fn rejected_records_do_not_wrap_the_budget() {
        let Some(draws) = clamped(0, &[u32::MAX], 128, 1) else {
            return; // no host GPU here
        };
        assert_eq!(draws.iter().filter(|&&count| count != 0).count(), 2);
        let meshes = clamped(10, &[u32::MAX], 600, 1).unwrap();
        assert_eq!(meshes.iter().filter(|&&groups| groups != 0).count(), 16);
    }

    /// The budget goes to the first records, in order: of 200 draws of
    /// 3 * 2^18 vertices the first 170 run (it ends inside the third tile);
    /// small records that all fit all run.
    #[test]
    fn the_budget_goes_to_the_first_records() {
        let Some(draws) = clamped(0, &[3 << 18, 1, 0, 0], 200, 1) else {
            return; // no host GPU here
        };
        assert!(draws[..170].iter().all(|&count| count != 0));
        assert!(draws[170..].iter().all(|&count| count == 0));
        let small = clamped(0, &[1], 1 << 16, 1).unwrap();
        assert!(small.iter().all(|&count| count == 1));
    }

    /// A record that does not fit ends the budget: after two draws of
    /// 3 * 2^24 vertex instances a third takes what is left, and the small
    /// ones after it run empty.
    #[test]
    fn a_record_that_does_not_fit_ends_the_budget() {
        let mut words = [1 << 24, 3, 0, 0].repeat(3);
        words.extend([3, 1, 0, 0].repeat(6));
        let Some(draws) = clamped(0, &words, 8, 1) else {
            return; // no host GPU here
        };
        let ran: Vec<bool> = draws.iter().map(|&count| count != 0).collect();
        assert_eq!(ran, [true, true, false, false, false, false, false, false]);
    }

    /// Two lists clamping into their own argument buffers, run in either
    /// order: every barrier starts from the buffer's current state (the
    /// debug layer's rule; runs light19 and light21 had the title's GPU
    /// heap moved to unordered access under the input assembler), the
    /// title's copy is never transitioned, and all end common.
    #[test]
    fn argument_buffers_move_from_their_current_states() {
        use crate::host::tests::{issued, replay};
        use windows::core::Interface;
        let Some(host) = super::host() else {
            return; // no host GPU here
        };
        let (title, data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: our mapped buffer; a live buffer.
        let gpu = unsafe {
            data.cast::<[u32; 3]>().write_volatile([70_000, 1, 1]);
            title.GetGPUVirtualAddress()
        };
        let key = |r: &super::ID3D12Resource| r.as_raw() as usize;
        for a_first in [true, false] {
            let lists = [host.new_list().unwrap(), host.new_list().unwrap()];
            issued();
            let recorded: Vec<_> = lists
                .iter()
                .map(|l| {
                    let out = super::bound(l, gpu, (2, 12, 1), true).unwrap();
                    super::done(l, &out.buffer);
                    l.close();
                    (out.buffer, issued())
                })
                .collect();
            let order = if a_first { [0, 1] } else { [1, 0] };
            host.execute(&[&lists[order[0]], &lists[order[1]]]);
            let mut run = issued();
            run.extend(order.iter().flat_map(|&i| recorded[i].1.clone()));
            let mut states: std::collections::HashMap<usize, u32> =
                [key(&title), key(&recorded[0].0), key(&recorded[1].0)].into_iter().map(|k| (k, 0)).collect();
            replay(&run, &mut states).unwrap();
            assert!(run.iter().all(|t| t.0 != key(&title)), "the title's copy is never transitioned");
            assert!(states.values().all(|&s| s == 0), "all common again");
            for (out, _) in &recorded {
                let mut p = std::ptr::null_mut();
                // SAFETY: a mapped buffer of ours, the lists have run.
                let clamped = unsafe {
                    out.Map(0, None, Some(&mut p)).unwrap();
                    p.cast::<[u32; 3]>().read_volatile()
                };
                assert_eq!(clamped, [65_535, 1, 1]);
            }
            // SAFETY: as above.
            assert_eq!(unsafe { data.cast::<[u32; 3]>().read_volatile() }, [70_000, 1, 1], "the title's copy as written");
        }
    }

    /// The budget is the list's: a second execution of the same records
    /// finds it spent, and they all run empty.
    #[test]
    fn a_spent_budget_stays_spent() {
        let Some(twice) = clamped(1, &[u32::MAX], 64, 2) else {
            return; // no host GPU here
        };
        assert!(twice.iter().all(|&count| count == 0));
    }

    /// Compaction skips zero-work records while preserving exact submission
    /// order across multi-tile batches, writes zero count when all records
    /// are empty, skips compaction below the record threshold, and reuses
    /// oversized per-list argument buffers (> 2 MiB) across calls.
    #[test]
    fn compaction_preserves_order_and_drops_empty_records() {
        use windows::core::Interface;
        let Some(host) = super::host() else {
            return; // no host GPU here
        };
        let records = 150u32;
        let per = 4usize;
        let stride = (per * 4) as u32;
        let (buffer, data) = host.shared_buffer((records as u64) * u64::from(stride)).unwrap();
        let data = data.cast::<u32>();
        let list = host.new_list().unwrap();
        // SAFETY: mapped buffers alive for the duration of the command list.
        unsafe {
            for i in 0..records {
                let base = i as usize * per;
                let verts = if i % 3 == 0 { 0 } else { 10 + i };
                data.add(base).write_volatile(verts);
                data.add(base + 1).write_volatile(1);
                data.add(base + 2).write_volatile(i);
                data.add(base + 3).write_volatile(0);
            }
            let gpu = buffer.GetGPUVirtualAddress();
            let compacted = super::bound(&list, gpu, (0, stride, records), true).unwrap();
            assert_eq!(compacted.count_offset, Some(0));
            assert_eq!(compacted.args_offset, 16);
            super::done(&list, &compacted.buffer);

            list.close();
            host.execute(&[&list]);

            let mut mapped = std::ptr::null_mut();
            compacted.buffer.Map(0, None, Some(&mut mapped)).unwrap();
            let count = mapped.cast::<u32>().read_volatile();
            let expected: Vec<u32> = (0..records).filter(|i| i % 3 != 0).collect();
            assert_eq!(count as usize, expected.len());
            let args_ptr = (mapped as *const u8).add(compacted.args_offset as usize).cast::<u32>();
            for (slot, &orig) in expected.iter().enumerate() {
                assert_eq!(args_ptr.add(slot * per).read_volatile(), 10 + orig);
                assert_eq!(args_ptr.add(slot * per + 2).read_volatile(), orig);
            }
            compacted.buffer.Unmap(0, None);

            // Second submission on the same list: all-empty batch produces count 0,
            // and oversized (> 2 MiB) buffer allocations reuse the same ID3D12Resource.
            list.reset();
            for i in 0..records {
                data.add(i as usize * per).write_volatile(0);
            }
            let all_empty = super::bound(&list, gpu, (0, stride, records), true).unwrap();
            super::done(&list, &all_empty.buffer);

            let tiny = super::bound(&list, gpu, (0, stride, super::COMPACT_MIN_RECORDS - 1), true).unwrap();
            assert_eq!(tiny.count_offset, None);
            assert_eq!(tiny.args_offset, 0);
            super::done(&list, &tiny.buffer);

            let huge_records = 200_000u32; // ~3.2 MiB > 2 MiB
            let (huge_buf, huge_data) = host.shared_buffer(u64::from(huge_records) * u64::from(stride)).unwrap();
            huge_data.cast::<u32>().write_volatile(3);
            let huge_gpu = huge_buf.GetGPUVirtualAddress();
            let first_huge = super::bound(&list, huge_gpu, (0, stride, huge_records), true).unwrap();
            super::done(&list, &first_huge.buffer);
            let second_huge = super::bound(&list, huge_gpu, (0, stride, huge_records), true).unwrap();
            super::done(&list, &second_huge.buffer);
            assert_eq!(first_huge.buffer.as_raw(), second_huge.buffer.as_raw());

            list.close();
            host.execute(&[&list]);

            let mut empty_mapped = std::ptr::null_mut();
            all_empty.buffer.Map(0, None, Some(&mut empty_mapped)).unwrap();
            assert_eq!(empty_mapped.cast::<u32>().read_volatile(), 0);
            all_empty.buffer.Unmap(0, None);
        }
    }

    /// Synthetic hardware benchmark on the host GPU comparing uncompacted vs
    /// compacted indirect dispatches and draws across record counts and sparsity.
    #[test]
    #[ignore = "hardware GPU timing benchmark; run with --ignored --nocapture"]
    fn bench_indirect_compaction() {
        use super::*;
        use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::core::s;

        let Some(host) = super::host() else {
            return;
        };
        let Some(freq) = host.timestamp_frequency() else {
            return;
        };
        let device = &host.device;

        let compile = |src: &str, entry: windows::core::PCSTR, target: windows::core::PCSTR| -> Vec<u8> {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: static shader strings and valid out pointer.
            unsafe {
                D3DCompile(
                    src.as_ptr().cast(),
                    src.len(),
                    None,
                    None,
                    None,
                    entry,
                    target,
                    0,
                    0,
                    &mut code,
                    None,
                )
                .unwrap();
                let blob = code.unwrap();
                std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()).to_vec()
            }
        };

        let cs_bytes = compile(
            r#"#define RS ""
               [RootSignature(RS)]
               [numthreads(1, 1, 1)]
               void main() {}"#,
            s!("main"),
            s!("cs_5_1"),
        );
        let vs_bytes = compile(
            r#"#define RS ""
               [RootSignature(RS)]
               float4 main() : SV_Position { return float4(0, 0, 0, 1); }"#,
            s!("main"),
            s!("vs_5_1"),
        );
        let ps_bytes = compile(
            r#"float4 main() : SV_Target { return float4(0, 0, 0, 0); }"#,
            s!("main"),
            s!("ps_5_1"),
        );

        // SAFETY: valid compiled bytecode blobs and descriptor structs for device object creation.
        let (cs_root, cs_pso, dispatch_sig, gfx_root, gfx_pso, draw_sig, query_heap, readback) = unsafe {
            let cs_root: ID3D12RootSignature = device.CreateRootSignature(0, &cs_bytes).unwrap();
            let cs_pso: ID3D12PipelineState = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&cs_root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: cs_bytes.as_ptr().cast(),
                        BytecodeLength: cs_bytes.len(),
                    },
                    ..Default::default()
                })
                .unwrap();
            let gfx_root: ID3D12RootSignature = device.CreateRootSignature(0, &vs_bytes).unwrap();
            let mut gfx_desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&gfx_root),
                VS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: vs_bytes.as_ptr().cast(),
                    BytecodeLength: vs_bytes.len(),
                },
                PS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: ps_bytes.as_ptr().cast(),
                    BytecodeLength: ps_bytes.len(),
                },
                SampleMask: u32::MAX,
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 0,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                ..Default::default()
            };
            gfx_desc.RasterizerState = D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                ..Default::default()
            };
            gfx_desc.BlendState.RenderTarget[0].RenderTargetWriteMask = 0x0F;
            let gfx_pso: ID3D12PipelineState = device.CreateGraphicsPipelineState(&gfx_desc).unwrap();

            let mk_sig = |kind: i32, stride: u32| -> ID3D12CommandSignature {
                let arg = D3D12_INDIRECT_ARGUMENT_DESC {
                    Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind),
                    ..Default::default()
                };
                let desc = D3D12_COMMAND_SIGNATURE_DESC {
                    ByteStride: stride,
                    NumArgumentDescs: 1,
                    pArgumentDescs: &arg,
                    NodeMask: 0,
                };
                let mut sig: Option<ID3D12CommandSignature> = None;
                device.CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut sig).unwrap();
                sig.unwrap()
            };
            let dispatch_sig = mk_sig(2, 12);
            let draw_sig = mk_sig(0, 16);

            let mut qh: Option<ID3D12QueryHeap> = None;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 4,
                        NodeMask: 0,
                    },
                    &mut qh,
                )
                .unwrap();
            let mut rb: Option<ID3D12Resource> = None;
            let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
            let rdesc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 32,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &rdesc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut rb)
                .unwrap();
            (cs_root, cs_pso, dispatch_sig, gfx_root, gfx_pso, draw_sig, qh.unwrap(), rb.unwrap())
        };

        let list = host.new_list().unwrap();
        let max_records = 4096u32;
        let (title_buf, title_ptr) = host.shared_buffer(u64::from(max_records) * 16).unwrap();
        // SAFETY: live shared buffer.
        let title_gpu = unsafe { title_buf.GetGPUVirtualAddress() };

        let run_case = |kind: u32, records: u32, active_every: u32, compact: bool| -> (f64, f64) {
            let stride = if kind == 2 { 12u32 } else { 16u32 };
            let words_per = (stride / 4) as usize;
            // SAFETY: mapped shared buffer sized for `max_records * 16` bytes.
            unsafe {
                let p = title_ptr.cast::<u32>();
                for i in 0..records {
                    let active = active_every != 0 && i % active_every == 0;
                    let base = i as usize * words_per;
                    if kind == 2 {
                        p.add(base).write_volatile(u32::from(active));
                        p.add(base + 1).write_volatile(1);
                        p.add(base + 2).write_volatile(1);
                    } else {
                        p.add(base).write_volatile(if active { 3 } else { 0 });
                        p.add(base + 1).write_volatile(1);
                        p.add(base + 2).write_volatile(0);
                        p.add(base + 3).write_volatile(0);
                    }
                }
            }
            let mut total_samples = Vec::with_capacity(15);
            let mut exec_samples = Vec::with_capacity(15);
            for iter in 0..18 {
                // SAFETY: open command list, live PSOs, signatures, buffers, and query heap.
                unsafe {
                    list.list.EndQuery(&query_heap, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                    let clamped = super::bound_inner(&list, title_gpu, (kind, stride, records), compact).unwrap();
                    if kind == 2 {
                        list.list.SetComputeRootSignature(&cs_root);
                        list.list.SetPipelineState(&cs_pso);
                    } else {
                        list.list.SetGraphicsRootSignature(&gfx_root);
                        list.list.SetPipelineState(&gfx_pso);
                        list.list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    }
                    list.list.EndQuery(&query_heap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                    let sig = if kind == 2 { &dispatch_sig } else { &draw_sig };
                    list.list.ExecuteIndirect(
                        sig,
                        records,
                        &clamped.buffer,
                        clamped.args_offset,
                        clamped.count_offset.map(|_| &clamped.buffer),
                        clamped.count_offset.unwrap_or(0),
                    );
                    list.list.EndQuery(&query_heap, D3D12_QUERY_TYPE_TIMESTAMP, 2);
                    super::done(&list, &clamped.buffer);
                    list.list.EndQuery(&query_heap, D3D12_QUERY_TYPE_TIMESTAMP, 3);
                    list.list.ResolveQueryData(&query_heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, 4, &readback, 0);
                }
                list.close();
                host.execute(&[&list]);
                list.reset();
                if iter >= 3 {
                    // SAFETY: list execution finished; readback buffer holds 4 resolved u64 timestamps.
                    unsafe {
                        let mut p = std::ptr::null_mut();
                        readback.Map(0, None, Some(&mut p)).unwrap();
                        let ts = std::slice::from_raw_parts(p.cast::<u64>(), 4);
                        let total_us = (ts[3].saturating_sub(ts[0])) as f64 * 1e6 / freq as f64;
                        let exec_us = (ts[2].saturating_sub(ts[1])) as f64 * 1e6 / freq as f64;
                        readback.Unmap(0, None);
                        total_samples.push(total_us);
                        exec_samples.push(exec_us);
                    }
                }
            }
            total_samples.sort_by(f64::total_cmp);
            exec_samples.sort_by(f64::total_cmp);
            (total_samples[total_samples.len() / 2], exec_samples[exec_samples.len() / 2])
        };

        for (label, kind) in [("dispatch", 2u32), ("draw", 0u32)] {
            println!("{label} benchmark (freq={freq} Hz):");
            for &records in &[1u32, 2, 4, 8, 16, 64, 256, 1024, 4096] {
                for &(sparsity, every) in &[("0%_active", 0u32), ("5%_active", 20), ("100%_active", 1)] {
                    let (u_tot, u_exec) = run_case(kind, records, every, false);
                    let (c_tot, c_exec) = run_case(kind, records, every, true);
                    println!(
                        "records={records:>4} {sparsity:<11}: uncompacted total={u_tot:7.2}us exec={u_exec:7.2}us | compacted total={c_tot:7.2}us exec={c_exec:7.2}us"
                    );
                }
            }
        }
    }
}
