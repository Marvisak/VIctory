use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crate::ir::{Program, reverse_postorder};
use crate::structure::{Atom, LaneExit, Structured};

static HOST_MAX_WAVE: AtomicU32 = AtomicU32::new(0);

pub fn set_host_max_wave(lanes: u32) {
    HOST_MAX_WAVE.store(lanes, Relaxed);
}

pub fn host_max_wave() -> u32 {
    #[cfg(test)]
    if let Some(lanes) = TEST_HOST_WAVE.get() {
        return lanes;
    }
    HOST_MAX_WAVE.load(Relaxed)
}

#[cfg(test)]
thread_local! {
    static TEST_HOST_WAVE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

fn lane_regions(st: &Structured) -> Option<Vec<(usize, Vec<bool>)>> {
    let n = st.exits.len();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, e) in st.exits.iter().enumerate() {
        match *e {
            LaneExit::Jump(t) => succ[b].push(t),
            LaneExit::Branch { taken, not_taken, .. } => {
                succ[b].push(taken);
                succ[b].push(not_taken);
            }
            LaneExit::End => {}
            LaneExit::Indirect => return None,
        }
    }
    let words = n.div_ceil(64);
    let full = vec![u64::MAX; words];
    let mut pdom: Vec<Vec<u64>> = (0..n)
        .map(|b| {
            if succ[b].is_empty() {
                let mut v = vec![0; words];
                v[b / 64] |= 1 << (b % 64);
                v
            } else {
                full.clone()
            }
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            if succ[b].is_empty() {
                continue;
            }
            let mut v = full.clone();
            for &t in &succ[b] {
                for w in 0..words {
                    v[w] &= pdom[t][w];
                }
            }
            v[b / 64] |= 1 << (b % 64);
            if v != pdom[b] {
                pdom[b] = v;
                changed = true;
            }
        }
    }
    let mut regions = Vec::new();
    for (d, e) in st.exits.iter().enumerate() {
        let LaneExit::Branch { when, .. } = *e else { continue };
        if st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_))) {
            continue;
        }
        let mut seen = vec![false; n];
        let mut stack = succ[d].clone();
        while let Some(b) = stack.pop() {
            if seen[b] || pdom[d][b / 64] >> (b % 64) & 1 == 1 {
                continue;
            }
            seen[b] = true;
            stack.extend(succ[b].iter().copied());
        }
        regions.push((d, seen));
    }
    Some(regions)
}

fn all_lane_blocks(st: &Structured) -> Option<Vec<bool>> {
    let mut all = vec![true; st.exits.len()];
    for (_, region) in lane_regions(st)? {
        for (b, in_region) in region.into_iter().enumerate() {
            if in_region {
                all[b] = false;
            }
        }
    }
    Some(all)
}

pub(super) fn fence_only(p: &Program, st: &Structured) -> HashSet<usize> {
    if !p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .any(|i| i.name() == "s_barrier")
    {
        return HashSet::new();
    }
    let order = reverse_postorder(&p.blocks);
    let mut place = vec![usize::MAX; p.blocks.len()];
    for (n, &b) in order.iter().enumerate() {
        place[b] = n;
    }
    let back = |from: usize, to: usize| {
        let at = |b: usize| place.get(b).copied().unwrap_or(usize::MAX);
        at(from) != usize::MAX && at(to) <= at(from)
    };
    let cut_short = st
        .exits
        .iter()
        .enumerate()
        .any(|(b, e)| matches!(*e, LaneExit::Jump(t) if back(b, t)));
    let all = all_lane_blocks(st);
    (0..p.blocks.len())
        .filter(|&b| p.blocks[b].insts.iter().any(|i| i.name() == "s_barrier"))
        .filter(|&b| cut_short || all.as_ref().is_none_or(|all| !all.get(b).copied().unwrap_or(false)))
        .collect()
}

pub(super) fn barrier_mode(
    setting: u8,
    host_wave: u32,
    console_wave: u32,
    group_threads: Option<u32>,
    divergent: bool,
    writes_uavs: bool,
) -> u32 {
    let single_console_wave = group_threads.is_some_and(|t| t <= console_wave);
    let waits = match setting {
        0 => false,
        1 => !divergent,
        2 => true,
        _ => !(host_wave != 0 && host_wave < console_wave && single_console_wave && divergent),
    };
    8 | if writes_uavs { 2 } else { 0 } | u32::from(waits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barrier_flags_keep_fences_and_drop_only_unsafe_waits() {
        for writes in [false, true] {
            let fence = 8 | if writes { 2 } else { 0 };
            assert_eq!(barrier_mode(3, 32, 64, Some(64), true, writes), fence);
            assert_eq!(barrier_mode(3, 32, 64, Some(64), false, writes), fence | 1);
            assert_eq!(barrier_mode(3, 32, 64, Some(128), true, writes), fence | 1);
            assert_eq!(barrier_mode(3, 64, 64, Some(64), true, writes), fence | 1);
            assert_eq!(barrier_mode(3, 0, 64, Some(64), true, writes), fence | 1);
            assert_eq!(barrier_mode(0, 32, 64, Some(64), false, writes), fence);
            assert_eq!(barrier_mode(1, 32, 64, Some(64), true, writes), fence);
            assert_eq!(barrier_mode(1, 32, 64, Some(64), false, writes), fence | 1);
            assert_eq!(barrier_mode(2, 32, 64, Some(64), true, writes), fence | 1);
        }
    }

    use super::super::{Parts, lower};
    use crate::io;
    use crate::native::{
        ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
    };
    use crate::{ir, resource, structure};

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn root(root: RootType, user_data: u8, slot: u8) -> RootParam {
        RootParam {
            root,
            table_offset_dwords: 0,
            sgprs: if root == RootType::Descriptor { 2 } else { 1 },
            base_register: 0,
            user_data,
            slot,
        }
    }

    #[test]
    fn permlane_lowerings_keep_console_lane_reads() {
        let constants = RootParam {
            sgprs: 4,
            ..root(RootType::Constants, 0, 0)
        };
        let buffer = UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        };
        for op in [0x377u32, 0x378] {
            let words = [
                0xBE85_0380,
                0xF408_0202,
                0xFA00_0000,
                0x7E02_0200,
                0xD400_0000 | op << 16,
                0x0008_0301,
                0xE070_0000,
                0x8002_0000,
                S_ENDPGM,
            ];
            let native = NativeProgram {
                stage: HwStage::Cs,
                wave64: false,
                vgprs: 256,
                layout: vec![constants, root(RootType::Table, 4, 1)],
                used: vec![buffer],
                io: StageIo::Cs(ComputeIo {
                    rsrc2: 5 << 1,
                    threads: [64, 1, 1],
                }),
                code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            };
            let p = ir::build(&native.code, false, 256).unwrap();
            let st = structure::structure(&p).unwrap();
            let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
            let io = io::raise(&p, &native).unwrap();
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
            })
            .unwrap_or_else(|e| panic!("VOP3 {op:#x}: {e}"));
            assert!(lowered.wave_ops, "VOP3 {op:#x}: no wave operations");
            assert!(lowered.uses_lane_index, "VOP3 {op:#x}: no lane index");
            assert!(
                lowered.has_half_wave_mask,
                "VOP3 {op:#x}: no 32-lane half mask"
            );
        }
    }
}
