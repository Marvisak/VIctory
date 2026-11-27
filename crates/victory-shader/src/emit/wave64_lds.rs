use super::{Fields, Parts, Program};
use crate::structure::{Atom, Bool, LaneExit};

/// Returns `true` when every LDS access in `p` executes in workgroup-uniform
/// control flow (no per-lane `EXEC` guard, no divergent branch or `Indirect`
/// exit reaching any LDS block, and no non-waterfall `v_readfirstlane_b32`), so
/// DXIL group barriers can order single-wave Wave64 LDS sharing on Wave32 hosts
/// without forcing `WaveSize(64)`.
pub(super) fn can_sync_wave64_lds(p: &Program, x: &Parts) -> bool {
    let mut divergent = vec![false; p.blocks.len()];
    let mut queue = Vec::new();
    let is_uniform_bool = |when: Bool| {
        x.st.bools
            .support(when)
            .iter()
            .all(|a| matches!(a, Atom::Uniform(_)))
    };
    for (b, exit) in x.st.exits.iter().enumerate() {
        match *exit {
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } if !is_uniform_bool(when) => {
                for target in [taken, not_taken] {
                    if target < divergent.len() && !divergent[target] {
                        divergent[target] = true;
                        queue.push(target);
                    }
                }
            }
            LaneExit::Indirect => {
                for (succ, block) in p.blocks.iter().enumerate() {
                    if block.preds.contains(&b) && !divergent[succ] {
                        divergent[succ] = true;
                        queue.push(succ);
                    }
                }
            }
            _ => {}
        }
    }
    while let Some(b) = queue.pop() {
        let mut push = |target: usize| {
            if target < divergent.len() && !divergent[target] {
                divergent[target] = true;
                queue.push(target);
            }
        };
        match x.st.exits.get(b).copied() {
            Some(LaneExit::Jump(t)) => push(t),
            Some(LaneExit::Branch {
                taken, not_taken, ..
            }) => {
                push(taken);
                push(not_taken);
            }
            _ => {
                for (succ, block) in p.blocks.iter().enumerate() {
                    if block.preds.contains(&b) {
                        push(succ);
                    }
                }
            }
        }
    }
    for (b, block) in p.blocks.iter().enumerate() {
        for (i, inst) in block.insts.iter().enumerate() {
            if inst.name() == "v_readfirstlane_b32"
                && inst
                    .writes
                    .first()
                    .is_none_or(|w| !x.res.waterfall.reads.contains(&w.1))
            {
                return false;
            }
            if inst.name() == "s_barrier" && divergent[b] {
                return false;
            }
            // A scalar or SMEM instruction reading a lane mask (`x.st.masks` or
            // `x.st.highs`) as data (not a pure lane-mask operation writing a
            // tracked mask SGPR) can materialize a `waveActiveBallot` word that
            // differs between Wave64 and two Wave32 waves, making downstream
            // `Atom::Uniform` branches diverge across the two Wave32s.
            let is_mask = |r| x.st.masks.contains_key(&r) || x.st.highs.contains_key(&r);
            let writes_mask = matches!(inst.fields, Fields::Sop { .. })
                && inst.writes.iter().any(|&(reg, w)| {
                    matches!(reg, crate::ir::Reg::S(_)) && x.st.masks.contains_key(&w)
                });
            if matches!(inst.fields, Fields::Sop { .. } | Fields::Smem { .. })
                && inst.write(crate::ir::Reg::S(crate::ir::EXEC)).is_none()
                && !writes_mask
                && (inst.reads.iter().any(|&(_, r)| is_mask(r))
                    || inst.writes.iter().any(|&(reg, w)| {
                        matches!(reg, crate::ir::Reg::S(_)) && x.st.highs.contains_key(&w)
                    }))
            {
                return false;
            }
            if !matches!(inst.fields, Fields::Ds { gds: false, .. }) {
                continue;
            }
            if matches!(
                inst.name(),
                "ds_swizzle_b32" | "ds_bpermute_b32" | "ds_permute_b32" | "ds_nop"
            ) {
                continue;
            }
            if divergent[b]
                || x.st
                    .guards
                    .get(b)
                    .and_then(|g| g.get(i))
                    .is_some_and(Option::is_some)
            {
                return false;
            }
        }
    }
    true
}
