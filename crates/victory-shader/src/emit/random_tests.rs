//! Random GFX10 programs through the whole translation: SSA and
//! structurisation with bounded relative moves (`range`), resource
//! tracking, I/O and DXIL emission. Each program translates or is refused
//! with a reason, and every third one is translated again, from scratch, to
//! the same result. Half the programs are made with write marks (odd seeds),
//! as the title's pipelines are, half without.
//!
//! The title translates its shaders when it creates pipelines, in its own
//! process: a panic in any of these stages ends the title, and translation
//! that differs between runs defeats the shader cache and the pipeline
//! library. Several earlier panics were found this way (see the tests that
//! say "found by random-input testing"); this keeps the search running on
//! every change.
//!
//! The programs are random but plausible, so that most get past decoding:
//! opcodes from our tables with small register numbers, descriptors loaded
//! from the root tables first, branches that land on an instruction a few
//! before or after their own (loops among them), and a store or exports at
//! the end that need what the body computes. About one program in three
//! has a relative move, and in half of those `range` proves a bound on M0
//! and builds the program again. The stages, and what each varies
//! ([`Shape`]):
//!
//! * compute (45%), 32 or 64 lanes, group and thread ids, LDS;
//! * pixel (25%), 32 or 64 lanes: interpolation of one input element
//!   starting at any channel, in f32 or completed in f16, a 16-bit input;
//!   the colour, and depth and stencil;
//! * vertex (15%), 32 or 64 lanes: the attribute from v4; a parameter
//!   (16-bit, or two 16-bit rows per dword), the position and clip
//!   distances;
//! * mesh (15%), one to four waves of 32 or 64 lanes, vertex and primitive
//!   limits (some past the host's), lines now and then: the group id read
//!   from the first lane; per-vertex and per-primitive parameters, the
//!   primitive (a valid triangle or random indices) and the position.
//!
//! Now and then a body item is divergent control flow as the console's
//! compiler emits it, nested up to three deep around random bodies, its
//! masks in SGPRs no random instruction writes ([`construct`]): `if`
//! (`s_and_saveexec`, `s_cbranch_execz`, `s_or exec`), `if`-`else`
//! (`s_xor`, `s_or_saveexec`), a loop the lanes leave one by one (`s_or`
//! of the exits, `s_andn2 exec`, `s_cbranch_execnz`), a waterfall loop over
//! a VGPR's values (`v_readfirstlane`, `v_cmp_eq`), an `if` on `v_cmpx`,
//! and a pixel program's discard (`s_andn2` of the killed lanes, then
//! `s_cbranch_scc0` to an `exp null` kill block), in 32- and 64-lane
//! masks.
//!
//! Each stage translates about 4-10% of its programs (compute most).
//!
//! `VICTORY_RANDOM_PROGRAMS=<n>` sets how many programs run (4,096 by
//! default, a few seconds in a debug build) and `VICTORY_RANDOM_SEED=<s>`
//! the first program's seed; a failure names the seeds of the programs that
//! failed and lists the first. A stack overflow aborts the test before it
//! can: `VICTORY_RANDOM_TRACE=1` prints each seed before its program runs.
//! A long search runs best in a release build with overflow checks:
//!
//! ```text
//! CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true VICTORY_RANDOM_PROGRAMS=1000000 \
//!     cargo test --release -p victory-shader random_programs
//! ```

use super::container::{self, SigElement};
use crate::dxbc::{self, Container, FourCC};
use crate::gfx10::{self, Encoding};
use crate::native::{
    ComputeIo, HwStage, MeshIo, NativeProgram, PsInput, ResourceType, RootParam, RootType, StageIo,
    Usage, UsedResource, VsInput, VsOutput,
};
use crate::{io, range, resource};
use std::io::Write;

const DEFAULT_PROGRAMS: u64 = 4096;
const S_ENDPGM: u32 = 0xBF81_0000;
/// Operand codes: `vcc_lo`, `m0`, `null`, `exec_lo`, `scc`, a literal.
const VCC: u32 = 106;
const M0: u32 = 124;
const NULL: u32 = 125;
const EXEC: u32 = 126;
const SCC: u32 = 253;
const LITERAL: u32 = 255;
/// A branch whose offset is filled in once every instruction's place is known.
const BRANCH_PENDING: u32 = 0xDEAD;
/// `s_cbranch_scc0` to the pixel program's kill block, set once the block
/// is placed after the end.
const TO_KILL: u32 = 0xBF84_DEAE;
/// `exp null off, off, off, off done vm`: a killed pixel's only output.
const EXP_NULL_DONE: [u32; 2] = [0xF800_1890, 0x0000_0000];

/// xorshift64, seeded through splitmix64 so neighbouring seeds diverge;
/// with the program's first user SGPR.
struct Rng {
    state: u64,
    /// The SGPR of user-data dword 0 ([`HwStage::user_sgpr`]): 0, or 8 for
    /// a mesh program. The registers below are counted from it.
    base: u32,
    /// The program's lane count (sizes its lane masks) and stage.
    wave64: bool,
    stage: HwStage,
}

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Rng {
            state: (z ^ (z >> 31)) | 1,
            base: 0,
            wave64: false,
            stage: HwStage::Cs,
        }
    }

    fn next(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    /// SGPR `n` counted from the first user SGPR: s0-s7 hold the root
    /// parameters, s8-s23 the descriptors the prologue loads.
    fn s(&self, n: u32) -> u32 {
        self.base + n
    }

    fn word(&mut self) -> u32 {
        self.next() as u32
    }

    /// 0 to `n - 1`.
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n.max(1))) as u32
    }

    fn percent(&mut self, p: u32) -> bool {
        self.below(100) < p
    }

    fn opcode(&mut self, encoding: Encoding) -> u32 {
        let t = gfx10::opcodes(encoding);
        u32::from(t[self.below(t.len() as u32) as usize].0)
    }

    /// A VGPR: in a pixel program mostly past v0-v1, the barycentrics
    /// (read directly, refused), which [`interpolation`] reads.
    fn vgpr(&mut self) -> u32 {
        if self.stage == HwStage::Ps && self.percent(90) {
            2 + self.below(14)
        } else {
            self.below(16)
        }
    }

    /// An SGPR to write: mostly s24-s39, clear of the root parameters
    /// (s0-s7) and the descriptors (s8-s23), so that most programs keep
    /// their resources.
    fn sdst(&mut self) -> u32 {
        if self.percent(92) {
            let n = self.below(16);
            self.s(24 + n)
        } else {
            self.sreg()
        }
    }

    /// An SGPR to read: also the system SGPRs before the user data.
    fn sreg(&mut self) -> u32 {
        let (low, high) = (self.below(8), self.below(16));
        match self.below(100) {
            0..30 => self.s(low),
            30..60 => self.s(24 + high),
            60..66 => VCC + self.below(2),
            66..68 => M0,
            68..70 => NULL,
            70..76 => EXEC + self.below(2),
            _ => {
                let end = self.s(40);
                self.below(end)
            }
        }
    }

    /// An 8-bit scalar source; a literal's value goes to `lit`.
    fn ssrc(&mut self, lit: &mut Option<u32>) -> u32 {
        match self.below(100) {
            0..55 => self.sreg(),
            55..80 => 128 + self.below(81), // 0 to 64
            80..88 => 240 + self.below(9),  // ±0.5, ±1, ±2, ±4, 1/(2π)
            88..92 => SCC,
            92..97 => {
                *lit = Some(self.word());
                LITERAL
            }
            _ => 193 + self.below(16), // -1 to -16
        }
    }

    /// A 9-bit vector source.
    fn vsrc(&mut self, lit: &mut Option<u32>) -> u32 {
        if self.percent(60) {
            256 + self.vgpr()
        } else {
            self.ssrc(lit)
        }
    }
}

/// A register-relative move (`v_movrel*`, `v_swaprel_b32`), mostly after
/// an M0 that `range` can bound: a constant, or an SGPR clamped by
/// `s_min_u32` or `s_and_b32`. Branches around it make the bounds pass
/// through phis, and a move that writes the registers its own index comes
/// from tests its induction.
fn relative_move(r: &mut Rng) -> Vec<u32> {
    let moves: Vec<u32> = gfx10::opcodes(Encoding::Vop1)
        .iter()
        .filter(|(_, name)| name.contains("movrel") || name.contains("swaprel"))
        .map(|&(op, _)| u32::from(op))
        .collect();
    let op = moves[r.below(moves.len() as u32) as usize];
    let k = 128 + r.below(17); // 0 to 16
    let mut words = match r.below(4) {
        0 => vec![0xBE80_0300 | M0 << 16 | k], // s_mov_b32 m0, k
        1 => vec![0x8380_0000 | M0 << 16 | k << 8 | r.sreg()], // s_min_u32 m0, s, k
        2 => vec![0x8700_0000 | M0 << 16 | k << 8 | r.sreg()], // s_and_b32 m0, s, k
        _ => vec![],
    };
    words.push(0x7E00_0000 | r.vgpr() << 17 | op << 9 | (256 + r.vgpr()));
    words
}

/// An interpolation of a channel of attribute 0 (rarely 1, which no input
/// holds): mostly both halves over the first barycentrics (v0, v1), in f32
/// or completed in f16 (the low or high half of the result), or
/// `v_interp_mov_f32`; now and then a single random half.
fn interpolation(r: &mut Rng) -> Vec<u32> {
    let vintrp = |dst: u32, op: u32, attr: u32, chan: u32, src: u32| {
        0xC800_0000 | dst << 18 | op << 16 | attr << 10 | chan << 8 | src
    };
    let (dst, attr, chan) = (r.vgpr(), u32::from(r.percent(2)), r.below(4));
    match r.below(100) {
        // v_interp_p1_f32 dst, v0; v_interp_p2_f32 dst, v1
        0..55 => vec![vintrp(dst, 0, attr, chan, 0), vintrp(dst, 1, attr, chan, 1)],
        // v_interp_p1_f32 dst, v0; v_interp_p2_f16 dst, attr, v1, dst
        55..70 => {
            let high = if r.percent(50) { 1 << 14 } else { 0 };
            vec![
                vintrp(dst, 0, attr, chan, 0),
                0xD400_0000 | 0x35A << 16 | high | dst,
                attr | chan << 6 | (256 + 1) << 9 | (256 + dst) << 18,
            ]
        }
        // v_interp_mov_f32 dst, p10/p20/p0
        70..80 => vec![vintrp(dst, 2, attr, chan, r.below(3))],
        _ => vec![vintrp(dst, r.below(3), attr, chan, r.below(4))],
    }
}

/// The opcode of `name` in `encoding`'s table.
fn opcode(encoding: Encoding, name: &str) -> u32 {
    let t = gfx10::opcodes(encoding);
    let found = t.iter().find(|e| e.1 == name);
    u32::from(found.unwrap_or_else(|| panic!("no opcode {name}")).0)
}

/// The lane-mask form of a scalar opcode: `name` with `_b64` in a wave of
/// 64 lanes, `_b32` in one of 32.
fn mask_op(encoding: Encoding, name: &str, wave64: bool) -> u32 {
    opcode(
        encoding,
        &format!("{name}_b{}", if wave64 { 64 } else { 32 }),
    )
}

/// SOP1 `name sdst, ssrc0` on lane masks.
fn mask1(r: &Rng, name: &str, sdst: u32, ssrc0: u32) -> u32 {
    0xBE80_0000 | sdst << 16 | mask_op(Encoding::Sop1, name, r.wave64) << 8 | ssrc0
}

/// SOP2 `name sdst, ssrc0, ssrc1` on lane masks.
fn mask2(r: &Rng, name: &str, sdst: u32, ssrc0: u32, ssrc1: u32) -> u32 {
    0x8000_0000 | mask_op(Encoding::Sop2, name, r.wave64) << 23 | sdst << 16 | ssrc1 << 8 | ssrc0
}

/// SOPP `name` with a branch offset in dwords from the next instruction.
fn branch(name: &str, offset: usize, back: bool) -> u32 {
    let offset = if back {
        -(offset as i32)
    } else {
        offset as i32
    };
    0xBF80_0000 | opcode(Encoding::Sopp, name) << 16 | offset as u32 & 0xFFFF
}

/// A per-lane condition: a VOPC compare of 32-bit integers or floats into
/// VCC, or with `cmpx` into EXEC.
fn compare(r: &mut Rng, cmpx: bool) -> Vec<u32> {
    let prefix = if cmpx { "v_cmpx_" } else { "v_cmp_" };
    let ops: Vec<u32> = gfx10::opcodes(Encoding::Vopc)
        .iter()
        .filter(|(_, n)| {
            n.starts_with(prefix)
                && !n.contains("class")
                && ["_u32", "_i32", "_f32"].iter().any(|t| n.ends_with(t))
        })
        .map(|&(op, _)| u32::from(op))
        .collect();
    let op = ops[r.below(ops.len() as u32) as usize];
    let mut lit = None;
    let mut words = vec![0x7C00_0000 | op << 17 | r.vgpr() << 9 | r.vsrc(&mut lit)];
    words.extend(lit);
    words
}

/// A body of one to four items at nesting `depth`.
fn block(r: &mut Rng, depth: u32) -> Vec<u32> {
    (0..1 + r.below(4)).flat_map(|_| item(r, depth)).collect()
}

/// A body item at nesting `depth`: now and then a construct of divergent
/// control flow, else one instruction. Inside a construct no instruction
/// is a random branch: those get their offset at the top level only.
fn item(r: &mut Rng, depth: u32) -> Vec<u32> {
    if depth < 3 && r.percent(if depth == 0 { 10 } else { 15 }) {
        return construct(r, depth);
    }
    loop {
        let stage = r.stage;
        let words = instruction(r, stage);
        if depth == 0 || !(words[0] >> 23 == 0x17F && words[0] & 0xFFFF == BRANCH_PENDING) {
            return words;
        }
    }
}

/// Divergent control flow as the console's compiler emits it, around
/// random bodies: `if`, `if`-`else`, a loop the lanes leave one by one, a
/// waterfall loop over a VGPR's values, an `if` on `v_cmpx`, and in a
/// pixel program a discard. Its masks are kept in SGPRs from s48 (6 per
/// nesting level), which no random instruction writes.
fn construct(r: &mut Rng, depth: u32) -> Vec<u32> {
    let level = r.s(48 + 6 * depth);
    // The saved mask, a second mask (register pairs in wave64), a scalar.
    let (save, other, scalar) = (level, level + 2, level + 4);
    let kinds = if r.stage == HwStage::Ps { 6 } else { 5 };
    let mut w = Vec::new();
    match r.below(kinds) {
        // if: v_cmp; s_and_saveexec save, vcc; s_cbranch_execz end; body;
        // end: s_or exec, exec, save
        0 => {
            w.extend(compare(r, false));
            w.push(mask1(r, "s_and_saveexec", save, VCC));
            let body = block(r, depth + 1);
            w.push(branch("s_cbranch_execz", body.len(), false));
            w.extend(body);
            w.push(mask2(r, "s_or", EXEC, EXEC, save));
        }
        // if-else: then lanes in exec, the others in save; at the else,
        // s_or_saveexec save, save takes all and keeps the then lanes, the
        // s_xor leaves the else lanes.
        1 => {
            w.extend(compare(r, false));
            w.push(mask1(r, "s_and_saveexec", save, VCC));
            w.push(mask2(r, "s_xor", save, EXEC, save));
            let then = block(r, depth + 1);
            w.push(branch("s_cbranch_execz", then.len(), false));
            w.extend(then);
            w.push(mask1(r, "s_or_saveexec", save, save));
            w.push(mask2(r, "s_xor", EXEC, EXEC, save));
            let other_arm = block(r, depth + 1);
            w.push(branch("s_cbranch_execz", other_arm.len(), false));
            w.extend(other_arm);
            w.push(mask2(r, "s_or", EXEC, EXEC, save));
        }
        // A loop each lane leaves when its condition holds: s_mov done, 0;
        // top: body; v_cmp; s_or done, vcc, done; s_andn2 exec, exec, done;
        // s_cbranch_execnz top; s_or exec, exec, done
        2 => {
            w.push(mask1(r, "s_mov", other, 0x80));
            let body = block(r, depth + 1);
            let cond = compare(r, false);
            let back = body.len() + cond.len() + 3;
            w.extend(body);
            w.extend(cond);
            w.push(mask2(r, "s_or", other, VCC, other));
            w.push(mask2(r, "s_andn2", EXEC, EXEC, other));
            w.push(branch("s_cbranch_execnz", back, true));
            w.push(mask2(r, "s_or", EXEC, EXEC, other));
        }
        // A waterfall loop: each pass takes the first lane's value of a VGPR
        // as a scalar and runs the body with the lanes that hold it.
        3 => {
            let v = r.vgpr();
            w.push(mask1(r, "s_mov", save, EXEC));
            let body = block(r, depth + 1);
            let back = body.len() + 6;
            w.push(
                0x7E00_0000
                    | scalar << 17
                    | opcode(Encoding::Vop1, "v_readfirstlane_b32") << 9
                    | (256 + v),
            );
            w.push(0x7C00_0000 | opcode(Encoding::Vopc, "v_cmp_eq_u32") << 17 | v << 9 | scalar);
            w.push(mask1(r, "s_and_saveexec", other, VCC));
            let k = r.vgpr();
            w.push(opcode(Encoding::Vop2, "v_add_nc_u32") << 25 | k << 17 | k << 9 | scalar);
            w.extend(body);
            w.push(mask2(r, "s_xor", EXEC, EXEC, other));
            w.push(branch("s_cbranch_execnz", back, true));
            w.push(mask1(r, "s_mov", EXEC, save));
        }
        // if on v_cmpx: s_mov save, exec; v_cmpx; s_cbranch_execz end; body;
        // end: s_mov exec, save
        4 => {
            w.push(mask1(r, "s_mov", save, EXEC));
            w.extend(compare(r, true));
            let body = block(r, depth + 1);
            w.push(branch("s_cbranch_execz", body.len(), false));
            w.extend(body);
            w.push(mask1(r, "s_mov", EXEC, save));
        }
        // A pixel's discard: the lanes v_cmpx leaves in exec are killed;
        // with none left alive, to the kill block (`exp null`).
        _ => {
            w.push(mask1(r, "s_mov", save, EXEC));
            w.extend(compare(r, true));
            w.push(mask2(r, "s_andn2", save, save, EXEC));
            w.push(TO_KILL);
            w.push(mask1(r, "s_mov", EXEC, save));
        }
    }
    w
}

/// One instruction: its words, a pending branch's offset still unset.
fn instruction(r: &mut Rng, stage: HwStage) -> Vec<u32> {
    if stage == HwStage::Ps && r.percent(10) {
        return interpolation(r);
    }
    if r.percent(4) {
        return relative_move(r);
    }
    let mut lit = None;
    let mut words = match r.below(100) {
        // VOP2 and VOP1, a tenth of them SDWA, DPP16 or DPP8.
        0..26 => {
            let w = if r.percent(55) {
                let op = r.opcode(Encoding::Vop2);
                op << 25 | r.vgpr() << 17 | r.vgpr() << 9 | r.vsrc(&mut lit)
            } else {
                let op = r.opcode(Encoding::Vop1);
                0x7E00_0000 | r.vgpr() << 17 | op << 9 | r.vsrc(&mut lit)
            };
            if r.percent(10) {
                lit = None;
                let form = [0xF9, 0xFA, 0xE9][r.below(3) as usize];
                vec![w & !0x1FF | form, r.word() & 0xFFFF_FF0F]
            } else {
                vec![w]
            }
        }
        26..34 => {
            let op = r.opcode(Encoding::Vopc);
            vec![0x7C00_0000 | op << 17 | r.vgpr() << 9 | r.vsrc(&mut lit)]
        }
        // VOP3: its own opcodes and the VOP3 forms of VOPC, VOP2 and VOP1,
        // with modifiers now and then.
        34..48 => {
            let op = match r.below(4) {
                0 => r.opcode(Encoding::Vopc),
                1 => 0x100 + r.opcode(Encoding::Vop2),
                2 => 0x180 + r.opcode(Encoding::Vop1),
                _ => r.opcode(Encoding::Vop3),
            };
            let mods = if r.percent(20) { r.below(0x80) } else { 0 };
            let dst = if op < 0x100 && r.percent(50) {
                r.sdst()
            } else {
                r.vgpr()
            };
            let srcs = r.vsrc(&mut lit) | r.vsrc(&mut lit) << 9 | r.vsrc(&mut lit) << 18;
            let high = if r.percent(20) { r.below(32) << 27 } else { 0 };
            vec![0xD400_0000 | op << 16 | mods << 8 | dst, srcs | high]
        }
        48..51 => {
            let op = r.opcode(Encoding::Vop3p);
            let srcs = r.vsrc(&mut lit) | r.vsrc(&mut lit) << 9 | r.vsrc(&mut lit) << 18;
            vec![
                0xCC00_0000 | op << 16 | r.below(0x100) << 8 | r.vgpr(),
                srcs | r.below(32) << 27,
            ]
        }
        51..60 => {
            let op = r.opcode(Encoding::Sop2);
            let (a, b) = (r.ssrc(&mut lit), r.ssrc(&mut lit));
            vec![0x8000_0000 | op << 23 | r.sdst() << 16 | b << 8 | a]
        }
        60..66 => {
            let op = r.opcode(Encoding::Sop1);
            vec![0xBE80_0000 | r.sdst() << 16 | op << 8 | r.ssrc(&mut lit)]
        }
        66..71 => {
            let op = r.opcode(Encoding::Sopc);
            let (a, b) = (r.ssrc(&mut lit), r.ssrc(&mut lit));
            vec![0xBF00_0000 | op << 16 | b << 8 | a]
        }
        71..73 => {
            let op = r.opcode(Encoding::Sopk);
            vec![0xB000_0000 | op << 23 | r.sdst() << 16 | r.below(0x10000)]
        }
        // s_branch, s_cbranch_scc0/scc1/vccz/vccnz/execz/execnz.
        73..80 => {
            let op = [0x02, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09][r.below(7) as usize];
            vec![0xBF80_0000 | op << 16 | BRANCH_PENDING]
        }
        // Scalar loads (and rarely other SMEM) from the root tables'
        // addresses, or through the UAV's descriptor for the buffer forms.
        80..85 => {
            let t = gfx10::opcodes(Encoding::Smem);
            let (op, name) = loop {
                let (op, name) = t[r.below(t.len() as u32) as usize];
                if op < 0x20 || r.percent(5) {
                    break (u32::from(op), name);
                }
            };
            let table = [4, 6][r.below(2) as usize];
            let base = if name.starts_with("s_buffer_") {
                r.s(8) / 2 // s[8:11]
            } else {
                r.s(table) / 2 // s[4:5], s[6:7]
            };
            let soffset = if r.percent(70) { NULL } else { r.sreg() };
            vec![
                0xF400_0000 | op << 18 | r.sdst() << 6 | base,
                soffset << 25 | (r.below(64) * 4),
            ]
        }
        // Buffer access through the UAV at s[8:11].
        85..89 => {
            let op = r.opcode(Encoding::Mubuf);
            let soffset = if r.percent(70) { 0x80 } else { r.sreg() };
            vec![
                0xE000_0000 | op << 18 | r.below(4) << 12 | (r.below(64) * 4),
                soffset << 24 | (r.s(8) / 4) << 16 | r.vgpr() << 8 | r.vgpr(),
            ]
        }
        89..90 => {
            let op = r.opcode(Encoding::Mtbuf);
            vec![
                0xE800_0000 | r.below(128) << 19 | (op & 7) << 16 | r.below(4) << 12,
                0x80 << 24 | (op >> 3 & 1) << 21 | (r.s(8) / 4) << 16 | r.vgpr() << 8 | r.vgpr(),
            ]
        }
        // Images: the texture at s[12:19] (now and then the UAV), the sampler
        // at s[20:23].
        90..95 => {
            let op = r.opcode(Encoding::Mimg);
            let r128 = if r.percent(10) { 1 << 15 } else { 0 };
            let w0 = 0xF000_0000
                | (op & 0x7F) << 18
                | op >> 7
                | (1 + r.below(15)) << 8
                | r.below(8) << 3
                | r128;
            let view = if r.percent(90) { 12 } else { 8 };
            let rsrc = r.s(view) / 4;
            vec![
                w0,
                r.below(4) << 30 | (r.s(20) / 4) << 21 | rsrc << 16 | r.vgpr() << 8 | r.vgpr(),
            ]
        }
        95..98 => {
            let op = r.opcode(Encoding::Ds);
            vec![
                0xD800_0000 | op << 18 | r.below(0x10000),
                r.vgpr() << 24 | r.vgpr() << 16 | r.vgpr() << 8 | r.vgpr(),
            ]
        }
        _ => {
            let op = r.opcode(Encoding::Global);
            let saddr = if r.percent(50) {
                NULL
            } else {
                let table = [4, 6][r.below(2) as usize];
                r.s(table)
            };
            vec![
                0xDC00_8000 | op << 18 | r.below(0x1000),
                r.vgpr() << 24 | saddr << 16 | r.vgpr() << 8 | r.vgpr(),
            ]
        }
    };
    words.extend(lit);
    words
}

/// The stage of the program of `seed`: compute, pixel, vertex or mesh.
fn stage(seed: u64) -> HwStage {
    match Rng::new(seed).below(100) {
        0..45 => HwStage::Cs,
        45..70 => HwStage::Ps,
        70..85 => HwStage::Vs,
        _ => HwStage::Ms,
    }
}

/// The draws that make the shape of the program of `seed` ([`Shape`],
/// [`input_channels`]): their own sequence, so that the container is made
/// from the seed alone.
fn shape_rng(seed: u64) -> Rng {
    Rng::new(seed ^ 0x5348_4150_455F_5345)
}

/// The channels of a pixel or vertex program's input element: the first
/// (x to w) and how many.
fn input_channels(seed: u64) -> (u8, u8) {
    let mut r = shape_rng(seed);
    let col = r.below(4);
    (col as u8, 1 + r.below(4 - col) as u8)
}

/// What the program of `seed` exports and reads besides the basics, which
/// its container's signatures must match.
#[derive(Debug, Clone, Copy)]
struct Shape {
    /// Mesh: a per-vertex parameter (param0, row 1); always in a vertex
    /// program.
    vertex_param: bool,
    /// Mesh: a per-primitive parameter (param1, row 0 of `PSG1`).
    primitive_param: bool,
    /// Pixel: depth and stencil (`exp mrtz`) after the colour.
    depth: bool,
    /// Vertex: clip distances (`exp pos1`, `SV_ClipDistance` in row 2).
    clip: bool,
    /// Vertex and mesh: the per-vertex parameter as 16-bit values, two per
    /// dword with `pack2` (the second row then row 2).
    half: bool,
    pack2: bool,
    /// Pixel: the input as 16-bit values.
    half_input: bool,
}

fn shape(seed: u64) -> Shape {
    let mut r = shape_rng(seed);
    r.below(4); // the input's channels
    r.below(4);
    Shape {
        vertex_param: r.percent(50),
        primitive_param: r.percent(50),
        depth: r.percent(25),
        clip: r.percent(25),
        half: r.percent(25),
        pack2: r.percent(50),
        half_input: r.percent(25),
    }
}

/// `s_load_dwordx<n>` (`op` 2: 4 dwords, 3: 8) of `sdata` from the address
/// in `sbase` (even), at a byte offset.
fn s_load(op: u32, sdata: u32, sbase: u32, offset: u32) -> [u32; 2] {
    [
        0xF400_0000 | op << 18 | sdata << 6 | (sbase / 2),
        NULL << 25 | offset,
    ]
}

/// Triangle (0, 1, 2), each corner's index in its 10-bit field.
const TRIANGLE: u32 = 1 << 10 | 2 << 20;

/// The program of `seed`.
fn program(seed: u64) -> NativeProgram {
    let stage = stage(seed);
    let mut r = Rng::new(seed);
    r.below(100); // the stage's draw
    r.base = u32::from(stage.user_sgpr(0));
    let wave64 = match stage {
        HwStage::Cs | HwStage::Ms => r.percent(30),
        HwStage::Vs => r.percent(50),
        _ => r.percent(25),
    };
    (r.wave64, r.stage) = (wave64, stage);
    let base = r.base;
    let s = |n| base + n;
    let mut words = vec![
        0xBE80_0380 | s(5) << 16, // s_mov_b32 s5, 0
        0xBE80_0380 | s(7) << 16, // s_mov_b32 s7, 0
    ];
    words.extend(s_load(2, s(8), s(4), 0)); // s_load_dwordx4 s[8:11], s[4:5], 0
    words.extend(s_load(3, s(12), s(4), 0x20)); // s_load_dwordx8 s[12:19], s[4:5], 0x20
    words.extend(s_load(2, s(20), s(6), 0)); // s_load_dwordx4 s[20:23], s[6:7], 0
    if stage == HwStage::Vs && r.percent(75) {
        // The primitive id (v2) is refused in a vertex program: most leave
        // it unread.
        words.push(0x7E04_0280); // v_mov_b32 v2, 0
    }
    if stage == HwStage::Ms {
        // The group id is the same in every lane only as the first lane
        // reads it (`io` refuses a per-lane read): read it so, then free v5.
        let group = s(40);
        words.push(0x7E00_0000 | group << 17 | 0x02 << 9 | (256 + 5)); // v_readfirstlane_b32
        words.push(0x7E00_0000 | 5 << 17 | 0x01 << 9 | group); // v_mov_b32 v5, group
    }
    let body: Vec<Vec<u32>> = (0..2 + r.below(30)).map(|_| item(&mut r, 0)).collect();
    // Where each instruction starts, and the end of the body.
    let mut starts = Vec::with_capacity(body.len() + 1);
    let mut at = words.len();
    for inst in &body {
        starts.push(at);
        at += inst.len();
    }
    starts.push(at);
    for (k, mut inst) in body.into_iter().enumerate() {
        if inst[0] >> 23 == 0x17F && inst[0] & 0xFFFF == BRANCH_PENDING {
            let (lo, hi) = (k.saturating_sub(6), (k + 6).min(starts.len() - 1));
            let target = starts[lo + r.below((hi - lo + 1) as u32) as usize];
            let offset = target as i64 - starts[k] as i64 - 1;
            inst[0] = inst[0] & 0xFFFF_0000 | offset as u32 & 0xFFFF;
        }
        words.extend(inst);
    }
    let shape = shape(seed);
    let (vertex_param, primitive_param) = (shape.vertex_param, shape.primitive_param);
    match stage {
        // Past v0-v1, the barycentrics.
        HwStage::Ps if shape.depth => {
            words.extend([0xF800_100F, 0x0504_0302]); // exp mrt0 v2, v3, v4, v5 vm
            words.extend([0xF800_1883, 0x0000_0706]); // exp mrtz v6, v7, off, off done vm
        }
        HwStage::Ps => words.extend([0xF800_180F, 0x0504_0302]), // exp mrt0 v2, v3, v4, v5 done vm
        HwStage::Cs => {
            // buffer_store_dword v0, off, s[8:11], 0
            words.extend([0xE070_0000, 0x80 << 24 | (s(8) / 4) << 16]);
        }
        HwStage::Vs => {
            words.extend([0xF800_020F, 0x0706_0504]); // exp param0 v4, v5, v6, v7
            if shape.clip {
                words.extend([0xF800_00CF, 0x0302_0100]); // exp pos0 v0, v1, v2, v3
                words.extend([0xF800_08DF, 0x0B0A_0908]); // exp pos1 v8, v9, v10, v11 done
            } else {
                words.extend([0xF800_08CF, 0x0302_0100]); // exp pos0 v0, v1, v2, v3 done
            }
        }
        _ => {
            if vertex_param {
                words.extend([0xF800_020F, 0x0706_0504]); // exp param0 v4, v5, v6, v7
            }
            if primitive_param {
                words.extend([0xF800_021F, 0x0B0A_0908]); // exp param1 v8, v9, v10, v11
            }
            if r.percent(50) {
                words.extend([0x7E12_02FF, TRIANGLE]); // v_mov_b32 v9, triangle (0, 1, 2)
            }
            words.extend([0xF800_0941, 0x0000_0009]); // exp prim v9 done
            words.extend([0xF800_08CF, 0x0302_0100]); // exp pos0 v0, v1, v2, v3 done
        }
    }
    words.push(S_ENDPGM);
    // A discard's kill block, after the end.
    let kill = words.len();
    let mut killed = false;
    for (at, w) in words.iter_mut().enumerate() {
        if *w == TO_KILL {
            *w = branch("s_cbranch_scc0", kill - at - 1, false);
            killed = true;
        }
    }
    if killed {
        words.extend(EXP_NULL_DONE);
        words.push(S_ENDPGM);
    }

    let param = |root, sgprs, user_data, slot| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    let used = |kind, table_offset_dwords, dwords, user_data, slot| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data,
        slot,
    };
    let output = |row, param| VsOutput {
        usage: Usage::Generic,
        row,
        param,
        fp16: shape.half && param == 0,
        pack2: shape.half && shape.pack2 && param == 0,
    };
    let (col, cols) = input_channels(seed);
    let io = match stage {
        HwStage::Ps => StageIo::Ps {
            inputs: vec![PsInput {
                usage: Usage::Generic,
                row: 0,
                mask: ((1 << cols) - 1) << col,
                attr: 0,
                flat: false,
                fp16: shape.half_input,
                pack2: false,
                row2: 0,
            }],
            // PERSP_CENTER, PERSP_SAMPLE or LINEAR_CENTER (in v0-v1), now
            // and then all seven barycentric kinds (v0-v14).
            input_addr: [
                1 << 1,
                1,
                1 << 4,
                1 << 1,
                1,
                1 << 4,
                1 << 1,
                1,
                1 << 4,
                0x7F,
            ][r.below(10) as usize],
            // Depth and stencil export enables, POPS (not read by the
            // translation today).
            db_shader_control: [0, 3, 1 << 16][r.below(3) as usize],
            patches: vec![],
        },
        // Vertex, primitive and instance ids in v0, v2 and v3, the input
        // from v4.
        HwStage::Vs => StageIo::Vs {
            inputs: vec![VsInput {
                usage: Usage::Generic,
                row: 0,
                mask: ((1 << cols) - 1) << col,
                vgpr: 4,
            }],
            outputs: vec![output(1, 0)],
        },
        HwStage::Ms => {
            let lanes = if wave64 { 64 } else { 32 };
            // One or two waves, now and then four (past the host's 128).
            let threads = lanes << if r.percent(10) { 2 } else { r.below(2) };
            let mut outputs = Vec::new();
            if vertex_param {
                outputs.push(output(1, 0));
            }
            if primitive_param {
                outputs.push(output(0, 0x81));
            }
            StageIo::Ms(MeshIo {
                outputs,
                clip_cntl: 0,
                threads,
                // Mostly one vertex and one primitive per lane, or fewer; now
                // and then more than the host allows (256).
                max_vertices: match r.below(10) {
                    0 => 1 + r.below(300),
                    1..4 => threads / 2,
                    _ => threads,
                },
                max_primitives: match r.below(10) {
                    0 => 1 + r.below(300),
                    1..4 => threads / 4,
                    _ => threads,
                },
                topology: if r.percent(95) { 2 } else { 1 },
                lds_bytes: if r.percent(50) { 512 * r.below(32) } else { 0 },
            })
        }
        // 8 user SGPRs, group ids, thread ids and LDS now and then.
        _ => StageIo::Cs(ComputeIo {
            rsrc2: 8 << 1
                | r.below(8) << 7
                | r.below(3) << 11
                | if r.percent(70) {
                    (1 + r.below(16)) << 15
                } else {
                    0
                },
            threads: [if wave64 { 64 } else { 32 }, 1 + r.below(2), 1],
        }),
    };
    NativeProgram {
        stage,
        wave64,
        vgprs: 16,
        layout: vec![
            param(RootType::Constants, 4, 0, 0),
            param(RootType::Table, 1, 4, 1),
            param(RootType::Table, 1, 6, 2),
        ],
        used: vec![
            used(ResourceType::Uav, 0, 4, 4, 1),
            used(ResourceType::Srv, 8, 8, 4, 1),
            used(ResourceType::Sampler, 0, 4, 6, 2),
        ],
        io,
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// The container of the program of `seed`: the signatures its stage uses
/// ([`Shape`] says which). Pixel: one float input element in row 0, a
/// float4 target, and depth and stencil. Vertex: the same input; the
/// position, a float4 in row 1 (16-bit, or two 16-bit rows 1 and 2) and
/// clip distances in row 2 (row 3 then) out. Mesh: the position and the
/// parameters (per primitive in `PSG1`). Compute: none.
fn container(seed: u64, stage: HwStage) -> Vec<u8> {
    let el = |name: &str, kind, row, col, cols: u8| SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp: 0,
        row: Some(row),
        col,
        cols,
        usage: (1 << cols) - 1,
        half: false,
    };
    let shape = shape(seed);
    let (col, cols) = input_channels(seed);
    let input = SigElement {
        half: stage == HwStage::Ps && shape.half_input,
        ..el("TEXCOORD", 0, 0, col, cols)
    };
    let position = el("SV_Position", 3, 0, 0, 4);
    // The per-vertex parameter: row 1, and with `pack2` a second 16-bit
    // row 2.
    let params = |index_base: u32| -> Vec<SigElement> {
        let half = |row, index| SigElement {
            index,
            half: shape.half,
            ..el("TEXCOORD", 0, row, 0, 4)
        };
        let mut v = vec![half(1, index_base)];
        if shape.half && shape.pack2 {
            v.push(half(2, index_base + 1));
        }
        v
    };
    let (isg1, osg1, psg1) = match stage {
        HwStage::Ps => {
            let mut osg1 = vec![el("SV_Target", 16, 0, 0, 4)];
            if shape.depth {
                let system = |name: &str, kind, comp| SigElement {
                    row: None,
                    comp,
                    ..el(name, kind, 0, 0, 1)
                };
                osg1.push(system("SV_Depth", 17, 9));
                osg1.push(system("SV_StencilRef", 20, 5));
            }
            (vec![input], osg1, vec![])
        }
        HwStage::Vs => {
            let mut osg1 = vec![position];
            osg1.extend(params(0));
            if shape.clip {
                let row = if shape.half && shape.pack2 { 3 } else { 2 };
                osg1.push(el("SV_ClipDistance", 6, row, 0, 4));
            }
            (vec![input], osg1, vec![])
        }
        HwStage::Ms => {
            let mut osg1 = vec![position];
            if shape.vertex_param {
                osg1.extend(params(0));
            }
            let psg1 = shape
                .primitive_param
                .then(|| el("TEXCOORD", 0, 0, 0, 4))
                .into_iter()
                .map(|e| SigElement { index: 2, ..e })
                .collect();
            (vec![], osg1, psg1)
        }
        _ => return dxbc::build(&[]),
    };
    let isg1 = container::signature(&isg1, true);
    let osg1 = container::signature(&osg1, false);
    let psg1 = container::signature(&psg1, false);
    let mut parts = vec![(FourCC::OSG1, &osg1[..])];
    if stage != HwStage::Ms {
        parts.push((FourCC::ISG1, &isg1[..]));
    }
    if stage == HwStage::Ms {
        parts.push((FourCC::PSG1, &psg1[..]));
    }
    dxbc::build(&parts)
}

/// What translating `native` came to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Translated(super::HostShader),
    Refused(String),
}

/// The stages in the order and with the inputs [`super::translate_with`]
/// gives them.
fn translate(native: &NativeProgram, c: &Container, options: super::Options) -> Outcome {
    let run = || -> Result<super::HostShader, crate::ShaderError> {
        let (p, st) = range::build(&native.patched_code(), native.wave64, native.vgprs)?;
        let res = resource::track(&p, &native.layout, native.stage, st.as_ref().ok())?;
        let io = io::raise(&p, native)?;
        super::emit_with(c, native, &p, &st?, &io, &res, options)
    };
    match run() {
        Ok(host) => Outcome::Translated(host),
        Err(e) => Outcome::Refused(e.to_string()),
    }
}

/// The program as SP3 text, to see what failed.
fn listing(native: &NativeProgram) -> String {
    let mut text = format!(
        "wave{} {:?}\n",
        if native.wave64 { 64 } else { 32 },
        native.io
    );
    for inst in gfx10::decode(&native.patched_code()).unwrap_or_default() {
        let words: Vec<String> = inst.words.iter().map(|w| format!("{w:08x}")).collect();
        text += &format!(
            "  {:4x}: {:28} {}\n",
            inst.offset,
            words.join(" "),
            inst.text(native.wave64)
        );
    }
    text
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

#[test]
fn random_programs_translate_or_are_refused_the_same_way_every_time() {
    let count = env("VICTORY_RANDOM_PROGRAMS", DEFAULT_PROGRAMS);
    let first = env("VICTORY_RANDOM_SEED", 1);
    let trace = env("VICTORY_RANDOM_TRACE", 0) != 0;
    let mut failed = Vec::new();
    // Programs and translated programs per stage.
    let mut stages: Vec<(HwStage, u64, u64)> = Vec::new();
    for seed in first..first.saturating_add(count) {
        if trace {
            // Past the test's output capture, so that the last seed shows
            // even when a stack overflow aborts the process.
            let _ = writeln!(std::io::stderr(), "random program {seed}");
        }
        let native = program(seed);
        let bytes = container(seed, native.stage);
        let c = Container::parse(&bytes).unwrap();
        // Odd seeds with write marks, as the title's pipelines are made
        // (`victory-gpu`'s shader cache), even ones without, as `translate`.
        let options = super::Options {
            view_marks: seed % 2 == 1,
        };
        let once = || std::panic::catch_unwind(|| translate(&native, &c, options));
        let first = once();
        // Every third program again, from scratch (with and without write
        // marks among them): the result must not depend on hash or
        // allocation order.
        let again = (seed % 3 == 0).then(once);
        let why = match (first, again) {
            (Err(panic), _) | (_, Some(Err(panic))) => {
                let message = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("(no message)");
                format!("panics: {message}")
            }
            (Ok(a), Some(Ok(b))) if a != b => {
                format!("translates differently: {a:?} then {b:?}")
            }
            (Ok(a), _) => {
                let at = match stages.iter().position(|s| s.0 == native.stage) {
                    Some(at) => at,
                    None => {
                        stages.push((native.stage, 0, 0));
                        stages.len() - 1
                    }
                };
                stages[at].1 += 1;
                stages[at].2 += u64::from(matches!(a, Outcome::Translated(_)));
                continue;
            }
        };
        if failed.is_empty() {
            eprintln!("seed {seed} {why}\n{}", listing(&native));
        }
        failed.push(seed);
    }
    assert!(
        failed.is_empty(),
        "{} of {count} random programs failed (rerun one with VICTORY_RANDOM_SEED=<seed> \
         VICTORY_RANDOM_PROGRAMS=1); the first seeds: {:?}",
        failed.len(),
        &failed[..failed.len().min(20)]
    );
    // The generator still reaches DXIL emission in every stage: a change
    // that has a stage's programs all refused early would leave this test
    // checking nothing there.
    if count >= 1000 {
        for stage in [HwStage::Cs, HwStage::Ps, HwStage::Vs, HwStage::Ms] {
            let (programs, translated) = stages
                .iter()
                .find(|s| s.0 == stage)
                .map_or((0, 0), |s| (s.1, s.2));
            assert!(
                translated * 100 >= programs && programs * 20 >= count,
                "{stage:?}: {translated} of {programs} random programs translated"
            );
        }
    }
}

/// A seed always makes the same program, and neighbouring seeds different
/// ones.
#[test]
fn random_programs_are_reproducible_from_their_seed() {
    for seed in 1..64 {
        assert_eq!(program(seed).code, program(seed).code);
    }
    assert_ne!(program(1).code, program(2).code);
}
