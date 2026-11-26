//! Narrow-wave divergent barrier analysis for single-console-wave thread groups.

use std::collections::{HashMap, HashSet};

/// Blocks of natural loops whose exit condition depends on the lane: a group
/// barrier inside such a loop can be reached a different number of times by
/// different lanes. Loops whose exits are all uniform (even when they contain
/// lane-dependent inner branches that rejoin before the back-edge) are not
/// marked.
pub(super) fn lane_exit_loop_blocks(
    n: usize,
    succs: impl Fn(usize) -> [Option<usize>; 2],
    lane_branch: impl Fn(usize) -> bool,
) -> HashSet<usize> {
    let mut preds = vec![Vec::new(); n];
    for b in 0..n {
        for s in succs(b).into_iter().flatten() {
            if s < n {
                preds[s].push(b);
            }
        }
    }
    // Back edges: to a block still on the depth-first stack.
    let mut back = Vec::new();
    let (mut state, mut stack) = (vec![0u8; n], vec![(0usize, 0usize)]);
    if n > 0 {
        state[0] = 1;
    }
    while let Some(&mut (b, ref mut next)) = stack.last_mut() {
        let s = succs(b);
        if *next < 2 {
            let t = s[*next];
            *next += 1;
            let Some(t) = t else { continue };
            if t >= n {
                continue;
            }
            match state[t] {
                0 => {
                    state[t] = 1;
                    stack.push((t, 0));
                }
                1 => back.push((b, t)),
                _ => {}
            }
        } else {
            state[b] = 2;
            stack.pop();
        }
    }
    // Group back-edges by loop header so a single loop with multiple
    // back-edges (e.g. an if/else whose branches both jump back to `head`)
    // forms one natural loop body rather than two partial bodies.
    let mut loops: HashMap<usize, Vec<usize>> = HashMap::new();
    for (tail, head) in back {
        loops.entry(head).or_default().push(tail);
    }
    let mut out = HashSet::new();
    for (head, tails) in loops {
        let mut body = HashSet::from([head]);
        let mut work = tails;
        while let Some(b) = work.pop() {
            if body.insert(b) {
                work.extend(preds[b].iter().copied());
            }
        }
        // Only a branch that can leave the loop makes lanes execute different
        // iteration counts; an inner if/else that stays inside `body` does not.
        let exits_loop = |b: usize| succs(b).into_iter().flatten().any(|s| !body.contains(&s));
        if body.iter().any(|&b| exits_loop(b) && lane_branch(b)) {
            out.extend(body);
        }
    }
    out
}

pub(super) fn lane_loop_blocks(st: &crate::structure::Structured) -> HashSet<usize> {
    use crate::structure::{Atom, LaneExit};
    lane_exit_loop_blocks(
        st.exits.len(),
        |b| match st.exits[b] {
            LaneExit::Jump(t) => [Some(t), None],
            LaneExit::Branch { taken, not_taken, .. } => [Some(taken), Some(not_taken)],
            LaneExit::End | LaneExit::Indirect => [None, None],
        },
        |b| match st.exits[b] {
            LaneExit::Branch { when, .. } => st
                .bools
                .support(when)
                .iter()
                .any(|a| !matches!(a, Atom::Uniform(_))),
            _ => false,
        },
    )
}

/// Whether an `s_barrier` drops `SyncThreadGroup` (keeping memory fences) on
/// a narrow-wave host.
///
/// Keyed on `effective_wave` (`s.wave_size` when the shader requests an exact
/// wave size, otherwise `WaveLaneCountMin` so unpinned shaders on 32–64
/// adapters are treated conservatively). Restricted to thread groups that fit
/// in a single console wave (`group_threads <= console_wave`) AND have no
/// observable cross-wave shared memory read/write effects (`!has_shared_rw`),
/// so neither multi-wave groups nor LDS/UAV/global read-after-write paths can
/// race.
pub(super) fn should_drop_barrier_sync(
    effective_wave: u32,
    console_wave: u32,
    group_threads: Option<u32>,
    has_shared_rw: bool,
    in_lane_exit_loop: bool,
    non_uniform_reach: bool,
) -> bool {
    if effective_wave >= console_wave || has_shared_rw {
        return false;
    }
    if !group_threads.is_some_and(|t| t <= console_wave) {
        return false;
    }
    in_lane_exit_loop || non_uniform_reach
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{lane_exit_loop_blocks, should_drop_barrier_sync};

    #[test]
    fn lane_loop_blocks_only_marks_loops_with_lane_dependent_exits() {
        // Loop with a uniform exit (block 0 -> 1 or 4) containing a
        // lane-dependent inner if/else (block 1 -> 2 or 3, both rejoin at 0).
        // Because the loop exit is uniform, every lane runs the same iterations.
        let uniform_exit_succs = |b: usize| match b {
            0 => [Some(1), Some(4)], // uniform loop header exit
            1 => [Some(2), Some(3)], // lane-dependent inner branch
            2 | 3 => [Some(0), None], // back-edge to header
            _ => [None, None],
        };
        let uniform_exit = lane_exit_loop_blocks(5, uniform_exit_succs, |b| b == 1);
        assert!(
            uniform_exit.is_empty(),
            "uniform-exit loop with inner lane branch must not be marked: {uniform_exit:?}"
        );

        // Same CFG, but now the loop header exit (block 0) is lane-dependent.
        let lane_header_exit = lane_exit_loop_blocks(5, uniform_exit_succs, |b| b == 0);
        assert_eq!(lane_header_exit, HashSet::from([0, 1, 2, 3]));

        // Uniform header exit (block 0 -> 1 or 3), plus a lane-dependent early
        // break out of the loop (block 1 -> 2 or 3, where 2 jumps back to 0).
        let early_break_succs = |b: usize| match b {
            0 => [Some(1), Some(3)], // uniform header exit
            1 => [Some(2), Some(3)], // lane-dependent early exit to 3 (outside loop)
            2 => [Some(0), None],    // back-edge
            _ => [None, None],
        };
        let early_break = lane_exit_loop_blocks(4, early_break_succs, |b| b == 1);
        assert_eq!(early_break, HashSet::from([0, 1, 2]));
    }

    #[test]
    fn narrow_wave_barrier_sync_only_drops_for_divergent_single_console_wave_groups() {
        // 64-only host or shader pinned to WaveSize(64) (effective_wave == 64): never drops.
        assert!(!should_drop_barrier_sync(64, 64, Some(64), false, true, true));
        assert!(!should_drop_barrier_sync(64, 64, Some(128), false, true, true));

        // Narrow effective wave (32-only host, or unpinned shader on a 32-64 host),
        // single-console-wave group (64 threads), no shared R/W memory effects:
        // uniform barrier keeps SyncThreadGroup; divergent or lane-exit-loop drops it.
        assert!(!should_drop_barrier_sync(32, 64, Some(64), false, false, false));
        assert!(should_drop_barrier_sync(32, 64, Some(64), false, false, true));
        assert!(should_drop_barrier_sync(32, 64, Some(64), false, true, false));

        // Shaders with shared read/write memory effects (LDS, or both memory
        // writes and memory reads across buffers/images/flat global): must keep
        // SyncThreadGroup so host waves cannot race on shared data.
        assert!(!should_drop_barrier_sync(32, 64, Some(64), true, false, true));
        assert!(!should_drop_barrier_sync(32, 64, Some(64), true, true, true));

        // Multi-console-wave group (128 threads = 2 console waves):
        // must keep SyncThreadGroup so cross-wave sync is preserved.
        assert!(!should_drop_barrier_sync(32, 64, Some(128), false, false, true));
        assert!(!should_drop_barrier_sync(32, 64, Some(128), false, true, true));

        // Unknown thread-group size: conservative, keeps SyncThreadGroup.
        assert!(!should_drop_barrier_sync(32, 64, None, false, true, true));
    }

    /// Hardware dispatch proof: a 64-thread wave64 compute shader with zero LDS
    /// and write-only UAV output that executes `s_barrier` inside a
    /// lane-dependent branch (`lane < below`) completes on a narrow-wave GPU
    /// (e.g. NVIDIA wave32) for partial-wave counts (1, 10, 32, 33, 64) and
    /// every lane writes its exact reference value (`19 + lane`).
    #[test]
    #[cfg(windows)]
    fn divergent_single_wave_barrier_runs_on_narrow_wave_hardware() {
        use super::super::host::{Dispatch, Host, Root, View};
        use super::super::lower::{Parts, lower};
        use super::super::{HostShader, TEST_HOST_WAVE_MIN, validator};
        use crate::native::{
            ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo,
            UsedResource,
        };
        use crate::{io, ir, resource, structure};
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
        };

        const S_ENDPGM: u32 = 0xBF81_0000;
        let root = |root: RootType, user_data: u8, slot: u8| RootParam {
            root,
            table_offset_dwords: 0,
            sgprs: if root == RootType::Descriptor { 2 } else { 1 },
            base_register: 0,
            user_data,
            slot,
        };
        let words = [
            0xBE85_0380, // s_mov_b32 s5, 0
            0xF408_0202, // s_load_dwordx4 s[8:11], s[4:5], 0
            0xFA00_0000,
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0 (lane < below)
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0001, // s_cbranch_execz +1 (skip s_barrier)
            0xBF8A_0000, // s_barrier
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
            0x3402_0082, // v_lshlrev_b32 v1, 2, v0 (byte offset = lane * 4)
            0x4A04_0093, // v_add_nc_u32 v2, 19, v0 (value = 19 + lane)
            0xE070_1000, // buffer_store_dword v2, v1, s[8:11], 0 offen
            0x8002_0201,
            S_ENDPGM,
        ];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 16,
            layout: vec![
                RootParam {
                    sgprs: 1,
                    ..root(RootType::Constants, 0, 0)
                },
                root(RootType::Table, 4, 1),
            ],
            used: vec![UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            }],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [64, 1, 1],
            }),
            code,
        };
        let p = ir::build(&native.code, true, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resources");
        let io = io::raise(&p, &native).expect("io");

        let _one = super::super::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; skipped");
            return;
        };
        let mut options1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        // SAFETY: plain feature query on the live device.
        unsafe {
            host.device
                .CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS1,
                    std::ptr::from_mut(&mut options1).cast(),
                    std::mem::size_of_val(&options1) as u32,
                )
                .expect("D3D12_OPTIONS1");
        }
        if options1.WaveLaneCountMin == 0 || options1.WaveLaneCountMin >= 64 {
            eprintln!(
                "host min wave is {} (>= 64); narrow-wave hardware test skipped",
                options1.WaveLaneCountMin
            );
            return;
        }

        let old = TEST_HOST_WAVE_MIN.replace(Some(options1.WaveLaneCountMin));
        let lowered = lower(&Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
            view_marks: false,
        });
        TEST_HOST_WAVE_MIN.set(old);
        let lowered = lowered.expect("lower divergent single-wave barrier");

        if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
            let mut dxil = lowered.dxil.clone();
            v.validate(&mut dxil).expect("DXIL validation");
        }

        let shader = HostShader {
            stage: HwStage::Cs,
            dxil: lowered.dxil,
            user_data: lowered.user_data,
            samplers: lowered.samplers,
            static_samplers: Vec::new(),
            root_cbvs: lowered.root_cbvs,
            root_srvs: lowered.root_srvs,
            root_uavs: lowered.root_uavs,
            memory: lowered.memory,
            memory_writes: lowered.memory_writes,
            view_addresses: lowered.view_addresses,
            heap: lowered.heap,
            heap_writes: lowered.heap_writes,
            view_marks: false,
        };
        let (root_sig, pso) = host.compute_pipeline(&shader).expect("compute PSO");
        let expected: Vec<u32> = (0..64).map(|lane| 19 + lane).collect();
        for below in [1u32, 10, 32, 33, 64] {
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                    ud[u.index as usize] = below;
                }
            }
            let out = host
                .dispatch(
                    &root_sig,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &[View::RawUav(64)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("below={below}: {e}"));
            assert_eq!(out[0], expected, "below={below}: all 64 lanes must write 19 + lane");
        }
        eprintln!(
            "divergent single-wave barrier passed on hardware (WaveLaneCountMin={}, Max={}): below=1,10,32,33,64",
            options1.WaveLaneCountMin, options1.WaveLaneCountMax
        );
    }
}
