//! Opt-in CPU benchmark of 12-byte structured SRVs. Run this test alone:
//! the descriptor and submission counters are process-wide.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory4,
};

use super::*;
use crate::descriptor::host::{HostHeap, view_copy};
use crate::host::{PREPARING, host};

const VIEWS: usize = 30;
const WORDS: usize = 12; // Four 12-byte records per view.
const WARMUP: usize = 8;
const SAMPLES: usize = 64;

const READ_VIEWS: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors=30)), UAV(u0)"
ByteAddressBuffer views[30] : register(t0);
RWByteAddressBuffer output : register(u0);
[RootSignature(RS)]
[numthreads(12, 1, 1)]
void main(uint3 group : SV_GroupID, uint word : SV_GroupIndex) {
    output.Store(4 * (12 * group.x + word), views[group.x].Load(4 * word));
}
"#;

#[derive(Default)]
struct Samples {
    create: Vec<Duration>,
    refresh: Vec<Duration>,
}

fn percentiles_us(samples: &mut [Duration]) -> (f64, f64) {
    samples.sort_unstable();
    let pick = |p: usize| samples[(p * samples.len()).div_ceil(100) - 1].as_secs_f64() * 1e6;
    (pick(50), pick(95))
}

#[test]
#[ignore = "hardware benchmark; run alone in release mode with --features diagnostics --ignored --nocapture"]
fn structured_srv_alignment_cost() {
    let host = host().expect("benchmark needs D3D12");
    // SAFETY: these queries resolve the LUID of the device that runs the shader.
    let adapter = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByLuid(host.device.GetAdapterLuid())
            .unwrap();
        adapter.GetDesc1().unwrap()
    };
    let end = adapter
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(adapter.Description.len());
    eprintln!(
        "adapter={} vendor={:04x} device={:04x}",
        String::from_utf16_lossy(&adapter.Description[..end]),
        adapter.VendorId,
        adapter.DeviceId
    );

    // Each case replaces its own slots, so retiring the other case's old
    // descriptors cannot change this case's creation time.
    let heaps = [(); 2].map(|_| HostHeap::new(false, VIEWS as u32).unwrap());
    let (root, pso) = compile_cs(READ_VIEWS);
    let (out, mapped) = host.shared_buffer((VIEWS * WORDS * 4) as u64).unwrap();
    // Each view starts in a separate 96-byte block. The greatest offset is
    // 48 bytes, followed by 48 bytes of view data, all inside that block.
    let src = buffer(((WARMUP + SAMPLES) * VIEWS * 96) as u64);
    let (data, _) = crate::resource::buffer_bytes(src).unwrap();
    let mut results: [[Samples; 2]; 2] = Default::default();
    for round in 0..WARMUP + SAMPLES {
        // Alternate which case runs first. Both get the same slot count,
        // view lengths, data pattern and number of rewrites.
        for case in [round % 2, 1 - round % 2] {
            let heap = &heaps[case];
            let aligned = case == 0;
            let offsets: &[usize] = if aligned { &[0, 48] } else { &[12, 24, 36] };
            let starts: [usize; VIEWS] =
                std::array::from_fn(|i| (round * VIEWS + i) * 96 + offsets[i % offsets.len()]);
            let views = starts.map(|byte| {
                View::Shader(
                    src as usize,
                    Some([0, 1, 0x1688, 0, (byte / 12) as u32, 0, 4, 12, 0, 0]),
                )
            });
            for live in [false, true] {
                let salt = 0x7812_0000 ^ ((round * 4 + case * 2 + usize::from(live)) as u32 * 1024);
                let expected: [u32; VIEWS * WORDS] = std::array::from_fn(|i| salt ^ i as u32);
                for (slot, &start) in starts.iter().enumerate() {
                    // SAFETY: each 48-byte range is inside the live title buffer.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            expected.as_ptr().add(slot * WORDS),
                            data.add(start).cast(),
                            WORDS,
                        )
                    };
                }

                let before = crate::shadow::exact_view_totals();
                let start = Instant::now();
                for (slot, view) in views.iter().enumerate() {
                    heap.make_at(slot as u32, view);
                }
                let create = start.elapsed();
                let after = crate::shadow::exact_view_totals();
                let totals: [u64; 3] = std::array::from_fn(|i| after[i] - before[i]);
                let requests = if aligned { 0 } else { VIEWS as u64 };
                let new = if live { 0 } else { requests };
                assert_eq!(totals, [requests, new, new * 48]);

                let list = host.new_list().unwrap();
                let began = Arc::new(OnceLock::new());
                let elapsed = Arc::new(OnceLock::new());
                let begin = Arc::clone(&began);
                list.defer_host(move || {
                    begin.set(Instant::now()).unwrap();
                });
                for view in &views {
                    let (shadow, address, len) = view_copy(view).unwrap();
                    shadow.refresh(&list, address, len);
                }
                let end = Arc::clone(&elapsed);
                // A title-write step flushes the normal submission batch before
                // invoking the closure. Neither dispatch nor fence waiting is timed.
                list.defer(move || {
                    end.set(began.get().unwrap().elapsed()).unwrap();
                });
                // SAFETY: the list is open; the descriptors and resources live
                // through the completion wait in execute.
                unsafe {
                    list.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                    list.list.SetComputeRootSignature(&root);
                    list.list.SetPipelineState(&pso);
                    list.list.SetComputeRootDescriptorTable(0, heap.gpu(0));
                    list.list
                        .SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
                    list.list.Dispatch(VIEWS as u32, 1, 1);
                }
                list.close();
                let before = PREPARING[1].load(Ordering::Relaxed);
                host.execute(&[&list]);
                let copied = PREPARING[1].load(Ordering::Relaxed) - before;
                // The aligned ranges coalesce on a shared untracked resource,
                // including the gaps. Separate exact slices refresh individually.
                let want_bytes = if aligned {
                    starts[VIEWS - 1] + 48 - starts[0]
                } else {
                    VIEWS * 48
                };
                assert_eq!(copied, want_bytes as u64);
                // SAFETY: execute waited; the mapped output has 360 dwords.
                let got =
                    unsafe { std::slice::from_raw_parts(mapped.cast::<u32>(), VIEWS * WORDS) };
                assert_eq!(got, expected, "aligned={aligned} live={live} round={round}");
                let refresh = *elapsed.get().unwrap();
                if round >= WARMUP {
                    let samples = &mut results[case][usize::from(live)];
                    samples.create.push(create);
                    samples.refresh.push(refresh);
                }
                if round == WARMUP {
                    eprintln!(
                        "aligned={aligned} live={live} requests={} new_slices={} new_slice_bytes={} refreshed_bytes={copied} GPU_words={}",
                        totals[0],
                        totals[1],
                        totals[2],
                        got.len()
                    );
                }
            }
        }
    }
    for (case, pair) in results.iter_mut().enumerate() {
        for (live, samples) in pair.iter_mut().enumerate() {
            let (create50, create95) = percentiles_us(&mut samples.create);
            let (refresh50, refresh95) = percentiles_us(&mut samples.refresh);
            eprintln!(
                "aligned={} live={} n={SAMPLES} views={VIEWS} CPU_us_per_batch create_p50={create50:.1} create_p95={create95:.1} refresh_p50={refresh50:.1} refresh_p95={refresh95:.1}",
                case == 0,
                live == 1
            );
        }
    }
    crate::object::release(src);
}
