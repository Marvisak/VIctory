//! Limits of the console flow in halves ([`super::CONSOLE_FLOW`] on a host
//! without 64-lane waves) that do not depend on how a program is lowered.

use super::Why;
use crate::ir::Program;

/// Why a wave64 program that reaches title memory by address
/// (`Lower::root_memory`) does not take the console flow in halves when it
/// also shares LDS, has whole-wave sections or permutes lanes.
pub(crate) const TITLE_MEMORY_IN_HALVES: Why = "LDS, whole-wave sections or lane permutes in a wave64 program that reaches title memory by address, on a host without 64-lane waves";

/// Whether the console flow in halves should leave out a program that
/// reaches title memory by address (`root_memory`): one that shares LDS
/// (`lds_dwords`), has whole-wave sections or permutes lanes.
///
/// latentdev reported (#439) that without this limit the ray-tracing
/// structure builder's stages `caa78b8ca62bc3d6` and `28918b7150a7aad5`
/// translated in this form, part of the builder ran and the GPU hung (RTX
/// 4090). Such stages run all or none. What in them goes wrong is not
/// known yet; until it is, they keep to the forms that do not need the
/// console flow. Programs that only exchange lanes or read masks whole
/// keep the console flow.
pub(super) fn leaves_title_memory_out(p: &Program, root_memory: bool, lds_dwords: u32) -> bool {
    root_memory
        && (lds_dwords > 0
            || (0..p.blocks.len()).any(|b| !super::super::wave::sections(p, b).is_empty())
            || p.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .any(|i| i.name().contains("permute")))
}
